//! Drives a headless copy of the user's browser over the Chrome DevTools
//! Protocol (CDP). Some sites, TikTok among them, reject API calls that their
//! own page script didn't sign. So instead of signing requests ourselves we
//! open the site in a real browser, logged in with the user's cookies, and make
//! the calls from inside the page.
//!
//! The browser gets a throwaway profile: it never opens or touches the user's
//! real one, so it's safe to run while the user's browser is open.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::browser::{Browser, Cookie};

/// How long one CDP call (including a page script's `await`s) may take.
const CALL_TIMEOUT: Duration = Duration::from_mins(1);

/// A running headless browser with one open tab. Call `close` when done.
pub struct Headless {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// The tab's flattened CDP session; commands carrying it go to the tab.
    session_id: String,
    next_id: u64,
    /// Killed on drop, as a backstop for when `close` isn't reached.
    child: Child,
    /// Deleted on drop. Declared after `child` so it's dropped after it.
    _profile: tempfile::TempDir,
}

impl Headless {
    /// Start `browser` headless with `cookies` loaded and a blank tab open:
    /// another installed browser for one that can't run headless, or failing
    /// that, Chrome's headless shell, fetched the first time.
    pub async fn launch(browser: Browser, cookies: &[Cookie]) -> Result<Self> {
        let exe = match browser.installed_headless() {
            Some(exe) => exe,
            None => crate::tools::headless_chrome().await?,
        };
        let profile = tempfile::Builder::new()
            .prefix("joyscroller-browser-")
            .tempdir()?;
        let mut child = Command::new(&exe)
            .arg("--headless=new")
            // Port 0 picks a free port; the browser prints the address it chose.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--mute-audio")
            // Keep the throwaway profile away from the desktop keyring
            // (Linux) and the Keychain (macOS), which would ask the user.
            .arg("--password-store=basic")
            .arg("--use-mock-keychain")
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start {}", exe.display()))?;

        let ws = match devtools_socket(&mut child).await {
            Ok(ws) => ws,
            Err(e) => {
                // Make sure the browser is gone before `profile` is deleted.
                if let Err(kill) = child.kill().await {
                    eprintln!("warning: couldn't stop the headless browser: {kill}");
                }
                return Err(e);
            }
        };
        let mut headless = Headless {
            ws,
            session_id: String::new(),
            next_id: 0,
            child,
            _profile: profile,
        };
        if let Err(e) = headless.open_tab(cookies).await {
            headless.close().await;
            return Err(e);
        }
        Ok(headless)
    }

    /// Load `cookies` and open the tab every later call goes to.
    async fn open_tab(&mut self, cookies: &[Cookie]) -> Result<()> {
        self.browser_call(
            "Storage.setCookies",
            json!({ "cookies": cdp_cookies(cookies) }),
        )
        .await?;
        let target = self
            .browser_call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = target["targetId"]
            .as_str()
            .context("Target.createTarget returned no targetId")?;
        let attached = self
            .browser_call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        attached["sessionId"]
            .as_str()
            .context("Target.attachToTarget returned no sessionId")?
            .clone_into(&mut self.session_id);

        // Headless mode announces itself as "HeadlessChrome" in the user agent,
        // which bot checks look for. Present the regular desktop one instead.
        let version = self.browser_call("Browser.getVersion", json!({})).await?;
        let user_agent = version["userAgent"]
            .as_str()
            .context("Browser.getVersion returned no userAgent")?
            .replace("HeadlessChrome", "Chrome");
        self.call(
            "Network.setUserAgentOverride",
            json!({ "userAgent": user_agent }),
        )
        .await?;
        Ok(())
    }

    /// Open `url` in the tab, then wait until the JavaScript expression
    /// `ready` is truthy there (the site's own scripts have loaded, say).
    pub async fn goto(&mut self, url: &str, ready: &str) -> Result<()> {
        self.call("Page.navigate", json!({ "url": url })).await?;
        // Page.navigate returns once the request is sent, not when the page is
        // usable, and load events fire before late scripts run. Polling the
        // condition the caller actually needs avoids guessing at either.
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        let ready = format!("!!({ready})");
        loop {
            // Mid-navigation the old document can be torn down under the
            // evaluate, so an error here just means "not ready yet".
            let check = self.eval(&ready);
            if let Ok(Ok(Value::Bool(true))) = tokio::time::timeout_at(deadline, check).await {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                bail!("{url} didn't become ready (waited for `{ready}`)");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Run `expression` in the tab and return its value. A promise is awaited,
    /// so `(async () => { ... })()` works.
    pub async fn eval(&mut self, expression: &str) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let message = exception
                .pointer("/exception/description")
                .or_else(|| exception.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("page script threw: {message}");
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Shut the browser down and delete its profile. Dropping a `Headless`
    /// kills the browser too, but can't wait for it to exit, and a browser
    /// still shutting down recreates files in the profile after it's deleted.
    pub async fn close(mut self) {
        // The browser drops the connection as it closes, so this call
        // usually errors even when the close works; waiting for the exit
        // below is what tells us it worked.
        let _ = self.browser_call("Browser.close", json!({})).await;
        let exited = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        if !matches!(exited, Ok(Ok(_)))
            && let Err(e) = self.child.kill().await
        {
            eprintln!("warning: couldn't stop the headless browser: {e}");
        }
    }

    /// Send a command to the tab.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let session = self.session_id.clone();
        self.send(method, params, Some(&session)).await
    }

    /// Send a command to the browser itself rather than a tab.
    async fn browser_call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.send(method, params, None).await
    }

    async fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            msg["sessionId"] = json!(session);
        }
        self.ws.send(Message::text(msg.to_string())).await?;

        let reply = tokio::time::timeout(CALL_TIMEOUT, async {
            while let Some(frame) = self.ws.next().await {
                let Message::Text(text) = frame? else {
                    continue;
                };
                let reply: Value = serde_json::from_str(&text)?;
                // Everything without our id is an event or an older reply.
                if reply["id"].as_u64() == Some(id) {
                    return Ok(reply);
                }
            }
            bail!("browser closed the DevTools connection")
        })
        .await
        .with_context(|| format!("{method} timed out"))??;

        if let Some(error) = reply.get("error") {
            return Err(anyhow!("{method} failed: {error}"));
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Wait for a just-started browser to report its DevTools address, then
/// connect to it.
async fn devtools_socket(child: &mut Child) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let stderr = child.stderr.take().context("browser stderr not piped")?;
    let mut lines = BufReader::new(stderr).lines();
    let ws_url = tokio::time::timeout(CALL_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            if let Some(url) = line.strip_prefix("DevTools listening on ") {
                return Ok(url.trim().to_string());
            }
        }
        bail!("browser exited before opening its DevTools port")
    })
    .await
    .context("timed out waiting for the browser to start")??;
    // Keep draining stderr so a chatty browser can't block on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .with_context(|| format!("connecting to DevTools at {ws_url}"))?;
    Ok(ws)
}

/// Cookies in the shape `Storage.setCookies` takes.
fn cdp_cookies(cookies: &[Cookie]) -> Vec<Value> {
    cookies
        .iter()
        .map(|c| {
            let mut cookie = json!({
                "name": c.name,
                "value": c.value,
                "path": c.path,
                "secure": c.secure,
            });
            // A leading dot marks a domain cookie. Without one the cookie is
            // host-only, which CDP expresses by giving a URL instead of a domain.
            if c.host.starts_with('.') {
                cookie["domain"] = json!(c.host);
            } else {
                let scheme = if c.secure { "https" } else { "http" };
                cookie["url"] = json!(format!("{scheme}://{}{}", c.host, c.path));
            }
            if c.expires > 0 {
                cookie["expires"] = json!(c.expires);
            }
            cookie
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(host: &str, expires: i64) -> Cookie {
        Cookie {
            host: host.into(),
            path: "/".into(),
            secure: true,
            expires,
            name: "sid".into(),
            value: "abc".into(),
        }
    }

    #[test]
    fn domain_cookie_keeps_its_domain() {
        let out = cdp_cookies(&[cookie(".tiktok.com", 42)]);
        assert_eq!(out[0]["domain"], ".tiktok.com");
        assert_eq!(out[0]["expires"], 42);
        assert!(out[0].get("url").is_none());
    }

    #[test]
    fn host_only_cookie_becomes_a_url() {
        let out = cdp_cookies(&[cookie("www.tiktok.com", 0)]);
        assert_eq!(out[0]["url"], "https://www.tiktok.com/");
        assert!(out[0].get("domain").is_none());
        // Session cookies carry no expiry.
        assert!(out[0].get("expires").is_none());
    }
}
