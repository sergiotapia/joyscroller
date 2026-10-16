//! Drives a headless copy of the user's browser over the Chrome DevTools
//! Protocol (CDP). Some sites, TikTok among them, reject API calls that their
//! own page script didn't sign. So instead of signing requests ourselves we
//! open the site in a real browser, logged in with the user's cookies, and make
//! the calls from inside the page. It presents itself as the desktop browser
//! it is, client hints included, and can note the requests a page sends (X's
//! page names the API's current query ids in its own).
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
use crate::tools::Running;

/// How long one CDP call (including a page script's `await`s) may take.
const CALL_TIMEOUT: Duration = Duration::from_mins(1);

/// The most requests `watch_requests` keeps, the latest.
const MOST_WATCHED: usize = 500;

/// A running headless browser with one open tab. Call `close` when done.
pub struct Headless {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// The tab's flattened CDP session; commands carrying it go to the tab.
    session_id: String,
    next_id: u64,
    /// The requests the tab has sent since `watch_requests`, oldest first.
    sent: Vec<SentRequest>,
    watching: bool,
    /// Killed on drop with every process it started, as a backstop for when
    /// `close` isn't reached.
    child: Running,
    /// Deleted on drop. Declared after `child` so it's dropped after it.
    _profile: tempfile::TempDir,
}

impl Headless {
    /// Start `browser` headless with `cookies` loaded and a blank tab open:
    /// another installed browser for one that can't run headless, or failing
    /// that, Chrome's headless shell, fetched the first time.
    pub async fn launch(browser: Browser, cookies: &[Cookie]) -> Result<Self> {
        let (runner, exe) = match browser.headless_runner() {
            Some(found) => found,
            // Chrome's headless shell, which sites should take for Chrome.
            None => (Browser::Chrome, crate::tools::headless_chrome().await?),
        };
        let profile = tempfile::Builder::new()
            .prefix("joyscroller-browser-")
            .tempdir()?;
        let mut cmd = Command::new(&exe);
        cmd.arg("--headless=new")
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
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = Running::spawn(&mut cmd)
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
            sent: Vec::new(),
            watching: false,
            child,
            _profile: profile,
        };
        if let Err(e) = headless.open_tab(cookies, runner).await {
            headless.close().await;
            return Err(e);
        }
        Ok(headless)
    }

    /// Load `cookies` and open the tab every later call goes to, presenting
    /// itself as the desktop `runner` it is.
    async fn open_tab(&mut self, cookies: &[Cookie], runner: Browser) -> Result<()> {
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
        // which bot checks look for. Present the regular desktop one instead,
        // with the client hints (`sec-ch-ua` and the rest) it would send:
        // overriding the user agent without them sends none at all, and
        // leaves `navigator.userAgentData` empty, which no desktop browser does.
        let version = self.browser_call("Browser.getVersion", json!({})).await?;
        let user_agent = version["userAgent"]
            .as_str()
            .context("Browser.getVersion returned no userAgent")?
            .replace("HeadlessChrome", "Chrome");
        // "Chrome/153.0.8010.48", whichever Chromium browser it is.
        let full_version = version["product"]
            .as_str()
            .and_then(|product| product.split_once('/'))
            .map(|(_, version)| version)
            .context("Browser.getVersion returned no product version")?;
        let metadata =
            user_agent_metadata(runner, full_version, &user_agent, &platform_version().await)?;
        self.call(
            "Network.setUserAgentOverride",
            json!({ "userAgent": user_agent, "userAgentMetadata": metadata }),
        )
        .await?;
        Ok(())
    }

    /// From now on, note the requests the tab sends (`wait_for_request`
    /// finds them), forgetting those noted before, and block those whose URLs
    /// match any of `blocked`'s patterns (`*` matches anything): pictures and
    /// video a page would load for nothing, say.
    pub async fn watch_requests(&mut self, blocked: &[&str]) -> Result<()> {
        // Our own requests from the page are noted too, and an old one could
        // otherwise pass for what the page sends next.
        self.sent.clear();
        self.call("Network.enable", json!({})).await?;
        self.call("Network.setBlockedURLs", json!({ "urls": blocked }))
            .await?;
        self.watching = true;
        Ok(())
    }

    /// Stop noting the tab's requests, and forget those noted. The blocking
    /// `watch_requests` set up stays.
    pub fn stop_watching(&mut self) {
        self.watching = false;
        self.sent.clear();
    }

    /// The first request the tab sent since `watch_requests` whose URL
    /// `wanted` accepts, waiting up to a minute for one.
    pub async fn wait_for_request(&mut self, wanted: impl Fn(&str) -> bool) -> Result<SentRequest> {
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        loop {
            if let Some(at) = self.sent.iter().position(|r| wanted(&r.url)) {
                return Ok(self.sent.remove(at));
            }
            if tokio::time::Instant::now() > deadline {
                bail!("the page didn't send the request we were waiting for");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            // Events are read while waiting for a reply, so any call picks up
            // the requests sent meanwhile. Mid-navigation it can fail; that's
            // only a wait.
            let _ = self.eval("0").await;
        }
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

        let (ws, sent) = (&mut self.ws, &mut self.sent);
        let watching = self.watching.then_some(self.session_id.as_str());
        let reply = tokio::time::timeout(CALL_TIMEOUT, async {
            while let Some(frame) = ws.next().await {
                let Message::Text(text) = frame? else {
                    continue;
                };
                let reply: Value = serde_json::from_str(&text)?;
                // Everything without our id is an event or an older reply.
                if reply["id"].as_u64() == Some(id) {
                    return Ok(reply);
                }
                if let Some(session) = watching
                    && let Some(request) = sent_request(&reply, session)
                {
                    if sent.len() == MOST_WATCHED {
                        sent.remove(0);
                    }
                    sent.push(request);
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

/// A request a page sent.
#[derive(Debug)]
pub struct SentRequest {
    pub url: String,
    /// The headers the page set (the browser adds its own, such as cookies,
    /// which aren't here), as a JSON object.
    pub headers: Value,
}

/// The request in a `Network.requestWillBeSent` event from the tab's
/// `session`, or None for any other message.
fn sent_request(message: &Value, session: &str) -> Option<SentRequest> {
    if message["method"] != "Network.requestWillBeSent" || message["sessionId"] != session {
        return None;
    }
    let request = message.pointer("/params/request")?;
    Some(SentRequest {
        url: request["url"].as_str()?.to_string(),
        headers: request["headers"].clone(),
    })
}

/// The version of the OS that desktop browsers give in their client hints:
/// macOS's ("15.5.0"). On Linux they leave it empty, and so do we on
/// Windows, where reading logins isn't released yet.
#[cfg(target_os = "macos")]
async fn platform_version() -> String {
    let mut cmd = Command::new("sw_vers");
    cmd.arg("-productVersion");
    let version = match crate::tools::output(&mut cmd).await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return String::new(),
    };
    // Browsers give three parts, and sw_vers leaves off a zero third.
    let mut parts: Vec<&str> = version.split('.').collect();
    parts.resize(3, "0");
    parts.join(".")
}

#[cfg(not(target_os = "macos"))]
fn platform_version() -> std::future::Ready<String> {
    std::future::ready(String::new())
}

/// The platform a desktop browser names in its client hints.
const PLATFORM: &str = if cfg!(target_os = "macos") {
    "macOS"
} else if cfg!(windows) {
    "Windows"
} else {
    "Linux"
};

/// The CPU architecture a desktop browser names in its client hints.
const ARCHITECTURE: &str = if cfg!(target_arch = "aarch64") {
    "arm"
} else {
    "x86"
};

/// Chromium's GREASE brand, which it lists among the real ones so sites
/// can't come to depend on the list's exact shape: its punctuation, its
/// version and where it goes all follow from the major version.
const GREASE_CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
/// Where the GREASE brand, Chromium and the browser's own brand go in a list
/// of three, by the major version.
const BRAND_ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

/// The brands a desktop copy of `brand` (None for plain Chromium; its name
/// and its own major version) on Chromium `major` lists in `sec-ch-ua`, as
/// (brand, version), in the order it lists them.
fn brand_list(brand: Option<(&str, &str)>, major: u32) -> Vec<(String, String)> {
    let seed = usize::try_from(major).unwrap_or(0);
    let grease = (
        format!(
            "Not{}A{}Brand",
            GREASE_CHARS[seed % GREASE_CHARS.len()],
            GREASE_CHARS[(seed + 1) % GREASE_CHARS.len()]
        ),
        GREASE_VERSIONS[seed % GREASE_VERSIONS.len()].to_string(),
    );
    let chromium = ("Chromium".to_string(), major.to_string());
    let list = if let Some((brand, version)) = brand {
        let order = BRAND_ORDERS[seed % BRAND_ORDERS.len()];
        let mut list = vec![None; 3];
        list[order[0]] = Some(grease);
        list[order[1]] = Some(chromium);
        list[order[2]] = Some((brand.to_string(), version.to_string()));
        list
    } else {
        let mut list = vec![None; 2];
        list[seed % 2] = Some(grease);
        list[(seed + 1) % 2] = Some(chromium);
        list
    };
    list.into_iter().flatten().collect()
}

/// The client hints `runner` sends at `full_version` (its Chromium version,
/// e.g. "153.0.8010.48") with `user_agent`, as `Network.setUserAgentOverride`
/// takes them.
fn user_agent_metadata(
    runner: Browser,
    full_version: &str,
    user_agent: &str,
    platform_version: &str,
) -> Result<Value> {
    let major: u32 = full_version
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .with_context(|| format!("unusual browser version {full_version:?}"))?;
    // Brave rounds every version it tells sites down to the major one.
    let chromium_full = if matches!(runner, Browser::Brave) {
        format!("{major}.0.0.0")
    } else {
        full_version.to_string()
    };
    // Opera and Edge number their own versions apart from Chromium's, and
    // give them in their user agent (`OPR/120.0.5543.161`, `Edg/…`).
    let own_full = match runner {
        Browser::Opera | Browser::OperaGx => version_after(user_agent, "OPR/"),
        Browser::Edge => version_after(user_agent, "Edg/"),
        _ => None,
    }
    .unwrap_or(&chromium_full)
    .to_string();
    let own_major = own_full.split('.').next().unwrap_or_default().to_string();
    let brand = runner.client_hints_brand();
    let brands = brand_list(brand.map(|name| (name, own_major.as_str())), major);
    let short: Vec<Value> = brands
        .iter()
        .map(|(brand, version)| json!({ "brand": brand, "version": version }))
        .collect();
    let full: Vec<Value> = brands
        .iter()
        .map(|(name, version)| {
            let version = if name == "Chromium" {
                chromium_full.clone()
            } else if Some(name.as_str()) == brand {
                own_full.clone()
            } else {
                format!("{version}.0.0.0")
            };
            json!({ "brand": name, "version": version })
        })
        .collect();
    Ok(json!({
        "brands": short,
        "fullVersionList": full,
        "fullVersion": own_full,
        "platform": PLATFORM,
        "platformVersion": platform_version,
        "architecture": ARCHITECTURE,
        "bitness": "64",
        "model": "",
        "mobile": false,
        "wow64": false,
    }))
}

/// The version in `user_agent` after `marker`, as in `OPR/120.0.5543.161`.
fn version_after<'a>(user_agent: &'a str, marker: &str) -> Option<&'a str> {
    let (_, rest) = user_agent.split_once(marker)?;
    let version = rest.split(' ').next()?;
    version
        .split('.')
        .next()
        .is_some_and(|major| !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()))
        .then_some(version)
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

    fn brands(list: &[(String, String)]) -> Vec<(&str, &str)> {
        list.iter()
            .map(|(brand, version)| (brand.as_str(), version.as_str()))
            .collect()
    }

    #[test]
    fn lists_brands_as_the_desktop_browser_does() {
        // What Brave 153 sent x.com, from a HAR of its own requests.
        assert_eq!(
            brands(&brand_list(Some(("Brave", "153")), 153)),
            [("Brave", "153"), ("Not_A Brand", "8"), ("Chromium", "153")]
        );
        assert_eq!(
            brands(&brand_list(None, 153)),
            [("Chromium", "153"), ("Not_A Brand", "8")]
        );
    }

    #[test]
    fn describes_the_browser_in_client_hints() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                  Chrome/153.0.0.0 Safari/537.36";
        let brave = user_agent_metadata(Browser::Brave, "153.0.8010.48", ua, "").unwrap();
        assert_eq!(brave["fullVersion"], "153.0.0.0");
        assert_eq!(
            brave["fullVersionList"],
            json!([
                {"brand": "Brave", "version": "153.0.0.0"},
                {"brand": "Not_A Brand", "version": "8.0.0.0"},
                {"brand": "Chromium", "version": "153.0.0.0"},
            ])
        );
        assert_eq!(brave["mobile"], false);
        let chrome = user_agent_metadata(Browser::Chrome, "153.0.8010.48", ua, "15.5.0").unwrap();
        assert_eq!(chrome["brands"][0]["brand"], "Google Chrome");
        assert_eq!(chrome["fullVersionList"][0]["version"], "153.0.8010.48");
        assert_eq!(chrome["platformVersion"], "15.5.0");
        assert!(user_agent_metadata(Browser::Chrome, "beta", ua, "").is_err());
        // Opera names its own version, apart from Chromium's.
        let opera_ua = format!("{ua} OPR/120.0.5543.161");
        let opera = user_agent_metadata(Browser::Opera, "153.0.8010.48", &opera_ua, "").unwrap();
        assert_eq!(opera["fullVersion"], "120.0.5543.161");
        let short = opera["brands"].as_array().unwrap();
        assert!(short.contains(&json!({"brand": "Opera", "version": "120"})));
        assert!(short.contains(&json!({"brand": "Chromium", "version": "153"})));
        let full = opera["fullVersionList"].as_array().unwrap();
        assert!(full.contains(&json!({"brand": "Opera", "version": "120.0.5543.161"})));
        assert!(full.contains(&json!({"brand": "Chromium", "version": "153.0.8010.48"})));
        assert_eq!(version_after(ua, "OPR/"), None);
    }

    #[test]
    fn notes_only_the_tabs_requests() {
        let event = |method: &str, session: &str| {
            json!({"method": method, "sessionId": session,
                   "params": {"request": {"url": "https://x.com/a", "headers": {"x": "1"}}}})
        };
        let seen = sent_request(&event("Network.requestWillBeSent", "s"), "s").unwrap();
        assert_eq!(seen.url, "https://x.com/a");
        assert_eq!(seen.headers["x"], "1");
        assert!(sent_request(&event("Network.requestWillBeSent", "other"), "s").is_none());
        assert!(sent_request(&event("Network.responseReceived", "s"), "s").is_none());
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
