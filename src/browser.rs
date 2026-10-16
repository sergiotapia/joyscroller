//! Reads (and decrypts) cookies straight out of the user's browser's on-disk
//! cookie store, so joyscroller can act as the logged-in user. Firefox's and
//! Safari's are plain (`browser/firefox.rs`, `browser/safari.rs`); the rest of
//! this is for the Chromium family.
//!
//! Linux and macOS for now. Chromium encrypts cookie values with AES-128-CBC
//! using a key derived (PBKDF2-SHA1, salt "saltysalt") from a password:
//! - Linux: v10 values use the fixed password "peanuts", v11 ones a password
//!   from the desktop keyring (Secret Service: gnome-keyring or KWallet).
//! - macOS: every value is v10, with the password from the login Keychain's
//!   "<Browser> Safe Storage" item and 1003 PBKDF2 iterations. Reading it makes
//!   macOS ask the user to allow Joyscroller, so it's read once per run.
//!
//! `sniff` picks which browser to read from: the one logged in to the most of
//! YouTube, TikTok, Instagram and X. TikTok, Instagram and X also run a browser
//! headless (`headless.rs`): the same one, or for a browser that can't run
//! headless (Firefox, Safari, Arc), another installed one, or failing that a copy of
//! Chrome's headless shell that `tools.rs` downloads.

mod firefox;
mod safari;

pub use safari::NoFullDiskAccess;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use anyhow::bail;
use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;
use sha2::{Digest, Sha256};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Browser {
    Brave,
    Chrome,
    Chromium,
    Vivaldi,
    Edge,
    Opera,
    /// Opera's gaming edition, made for macOS and Windows only.
    OperaGx,
    /// Made for macOS and Windows only.
    Arc,
    Firefox,
    /// Made for macOS only.
    Safari,
}

impl Browser {
    /// Every browser Joyscroller can read logins from, the most widely used
    /// first: Chrome by far, then Safari (on every Mac), Edge (on every
    /// Windows PC and many Macs), Firefox, Opera, Brave, Opera GX, Arc,
    /// Chromium (the default on many Linux distros) and Vivaldi.
    const MOST_USED_FIRST: [Browser; 10] = [
        Browser::Chrome,
        Browser::Safari,
        Browser::Edge,
        Browser::Firefox,
        Browser::Opera,
        Browser::Brave,
        Browser::OperaGx,
        Browser::Arc,
        Browser::Chromium,
        Browser::Vivaldi,
    ];

    /// The browser's name, as people know it.
    pub fn name(self) -> &'static str {
        match self {
            Browser::Brave => "Brave",
            Browser::Chrome => "Chrome",
            Browser::Chromium => "Chromium",
            Browser::Vivaldi => "Vivaldi",
            Browser::Edge => "Edge",
            Browser::Opera => "Opera",
            Browser::OperaGx => "Opera GX",
            Browser::Arc => "Arc",
            Browser::Firefox => "Firefox",
            Browser::Safari => "Safari",
        }
    }

    /// Whether the browser can run headless for TikTok, Instagram and X over the
    /// Chrome DevTools Protocol. Firefox dropped its support for it, and Safari
    /// never had it. Arc only
    /// takes a DevTools connection when no other copy of it is running, and
    /// crashes when asked to open a tab (`Target.createTarget`).
    fn runs_headless(self) -> bool {
        !matches!(self, Browser::Arc | Browser::Firefox | Browser::Safari)
    }

    /// The installed program to run headless with this browser's logins: its
    /// own, or for a browser that can't run headless, the most widely used
    /// installed one that can. The logins are copied into a throwaway profile
    /// either way, so any of them will do. `None` leaves Chrome's headless
    /// shell, which `tools::headless_chrome` downloads.
    pub fn installed_headless(self) -> Option<PathBuf> {
        self.headless_runner().map(|(_, exe)| exe)
    }

    /// `installed_headless`, along with which browser that program is.
    pub fn headless_runner(self) -> Option<(Browser, PathBuf)> {
        if self.runs_headless() {
            return self.executable().ok().map(|exe| (self, exe));
        }
        Browser::MOST_USED_FIRST
            .into_iter()
            .filter(|browser| browser.runs_headless())
            .find_map(|browser| browser.executable().ok().map(|exe| (browser, exe)))
    }

    /// The brand a desktop copy of this Chromium browser names in its
    /// user-agent client hints (`sec-ch-ua`) next to Chromium, as sites see
    /// it. Chromium itself and Vivaldi name none.
    pub fn client_hints_brand(self) -> Option<&'static str> {
        match self {
            Browser::Chrome => Some("Google Chrome"),
            Browser::Edge => Some("Microsoft Edge"),
            Browser::Brave => Some("Brave"),
            Browser::Opera => Some("Opera"),
            Browser::OperaGx => Some("Opera GX"),
            Browser::Chromium
            | Browser::Vivaldi
            | Browser::Arc
            | Browser::Firefox
            | Browser::Safari => None,
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn config_dir(self) -> Result<PathBuf> {
        let base = dirs::config_dir().context("no XDG config dir")?;
        Ok(base.join(match self {
            Browser::Brave => "BraveSoftware/Brave-Browser",
            Browser::Chrome => "google-chrome",
            Browser::Chromium => "chromium",
            Browser::Vivaldi => "vivaldi",
            Browser::Edge => "microsoft-edge",
            Browser::Opera => "opera",
            Browser::OperaGx | Browser::Arc | Browser::Safari => {
                return not_made_for_this_os(self);
            }
            Browser::Firefox => return firefox_has_no_config_dir(),
        }))
    }

    #[cfg(target_os = "macos")]
    fn config_dir(self) -> Result<PathBuf> {
        // ~/Library/Application Support
        let base = dirs::config_dir().context("no Application Support folder")?;
        Ok(base.join(match self {
            Browser::Brave => "BraveSoftware/Brave-Browser",
            Browser::Chrome => "Google/Chrome",
            Browser::Chromium => "Chromium",
            Browser::Vivaldi => "Vivaldi",
            Browser::Edge => "Microsoft Edge",
            Browser::Opera => "com.operasoftware.Opera",
            Browser::OperaGx => "com.operasoftware.OperaGX",
            Browser::Arc => "Arc/User Data",
            Browser::Firefox => return firefox_has_no_config_dir(),
            Browser::Safari => return Err(anyhow!("Safari keeps its cookies elsewhere")),
        }))
    }

    /// The browser's program, found on `PATH` under its usual Linux names.
    #[cfg(not(target_os = "macos"))]
    pub fn executable(self) -> Result<PathBuf> {
        let names: &[&str] = match self {
            Browser::Brave => &["brave", "brave-browser"],
            Browser::Chrome => &["google-chrome-stable", "google-chrome"],
            Browser::Chromium => &["chromium", "chromium-browser"],
            Browser::Vivaldi => &["vivaldi-stable", "vivaldi"],
            Browser::Edge => &["microsoft-edge-stable", "microsoft-edge"],
            Browser::Opera => &["opera"],
            Browser::OperaGx | Browser::Arc | Browser::Safari => {
                return not_made_for_this_os(self);
            }
            // The last is Flatpak's, on `PATH` where Flatpak adds its exports.
            Browser::Firefox => &["firefox", "firefox-esr", "org.mozilla.firefox"],
        };
        crate::tools::on_path(names).ok_or_else(|| anyhow!("none of {names:?} is on PATH"))
    }

    /// The browser's program inside its app bundle, in /Applications or the
    /// user's own ~/Applications. The bundle names its program, which isn't
    /// always named after the app.
    #[cfg(target_os = "macos")]
    pub fn executable(self) -> Result<PathBuf> {
        use core_foundation::{bundle::CFBundle, url::CFURL};

        let app = format!("{}.app", self.mac_app_name());
        let folders = [
            Some(PathBuf::from("/Applications")),
            dirs::home_dir().map(|home| home.join("Applications")),
        ];
        folders
            .into_iter()
            .flatten()
            .map(|folder| folder.join(&app))
            .filter(|bundle| bundle.is_dir())
            .find_map(|bundle| {
                CFBundle::new(CFURL::from_path(bundle, true)?)?
                    .executable_url()?
                    .to_path()
            })
            .filter(|program| program.is_file())
            .ok_or_else(|| anyhow!("{app} is in neither /Applications nor ~/Applications"))
    }

    /// The app's name on macOS, which is also its Keychain item's account.
    #[cfg(target_os = "macos")]
    fn mac_app_name(self) -> &'static str {
        match self {
            Browser::Brave => "Brave Browser",
            Browser::Chrome => "Google Chrome",
            Browser::Chromium => "Chromium",
            Browser::Vivaldi => "Vivaldi",
            Browser::Edge => "Microsoft Edge",
            Browser::Opera => "Opera",
            Browser::OperaGx => "Opera GX",
            Browser::Arc => "Arc",
            Browser::Firefox => "Firefox",
            Browser::Safari => "Safari",
        }
    }

    /// The Keychain item holding the cookie password, as (service, account).
    /// Checked on a real Mac for Brave and Edge, and on CI's Macs for Chrome.
    /// Opera's is yt-dlp's. Arc's and Opera GX's services are from users'
    /// reports, and their accounts are guesses, so `keychain_password` falls
    /// back to finding the item by service alone.
    /// Firefox and Safari don't encrypt their cookies, so they have none.
    #[cfg(target_os = "macos")]
    fn keychain_item(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Browser::Brave => ("Brave Safe Storage", "Brave"),
            Browser::Chrome => ("Chrome Safe Storage", "Chrome"),
            Browser::Chromium => ("Chromium Safe Storage", "Chromium"),
            Browser::Vivaldi => ("Vivaldi Safe Storage", "Vivaldi"),
            Browser::Edge => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
            // Opera GX shares Opera's item.
            Browser::Opera | Browser::OperaGx => ("Opera Safe Storage", "Opera"),
            Browser::Arc => ("Arc Safe Storage", "Arc"),
            Browser::Firefox | Browser::Safari => return None,
        })
    }

    /// The `application` attribute Chromium stores its keyring secret under.
    /// Edge's was found on CI's Linux runners (Edge 152-155). Opera GX and Arc
    /// aren't made for Linux, nor is Safari, and Firefox doesn't encrypt its
    /// cookies.
    #[cfg(target_os = "linux")]
    fn keyring_application(self) -> Option<&'static str> {
        Some(match self {
            Browser::Brave => "brave",
            Browser::Chrome => "chrome",
            // Edge and Opera (per yt-dlp) keep Chromium's own name rather
            // than their brand.
            Browser::Chromium | Browser::Edge | Browser::Opera => "chromium",
            Browser::Vivaldi => "vivaldi",
            Browser::OperaGx | Browser::Arc | Browser::Firefox | Browser::Safari => return None,
        })
    }
}

/// Firefox's profiles are found through `profiles.ini` instead
/// (`firefox::cookie_db`).
fn firefox_has_no_config_dir<T>() -> Result<T> {
    Err(anyhow!("Firefox keeps its profiles elsewhere"))
}

#[cfg(not(target_os = "macos"))]
fn not_made_for_this_os<T>(browser: Browser) -> Result<T> {
    Err(anyhow!("{} isn't made for this OS", browser.name()))
}

#[derive(Debug, Clone)]
pub struct Cookie {
    pub host: String,
    pub path: String,
    pub secure: bool,
    /// Expiry as a Unix timestamp (seconds). 0 means a session cookie.
    pub expires: i64,
    pub name: String,
    pub value: String,
}

/// A Chromium cookie's host matches domain `?1`: "x.com" matches ".x.com"
/// and "api.x.com", but not "stockx.com", which a bare suffix would.
const ON_DOMAIN: &str = "(host_key = ?1 OR host_key LIKE '%.' || ?1)";

/// All cookies whose host matches `domain` (e.g. "youtube.com" matches
/// ".youtube.com" and "www.youtube.com").
/// Firefox and Safari have no profiles named like Chromium's, so they read
/// the profile in use whatever `profile` says.
pub async fn cookies_for(browser: Browser, profile: &str, domain: &str) -> Result<Vec<Cookie>> {
    match browser {
        Browser::Firefox => return firefox::cookies_for(domain),
        Browser::Safari => return safari::cookies_for(domain),
        _ => {}
    }
    let (_copy, conn) = open_copy(&cookie_db(browser, profile)?)?;

    let db_version: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| {
            r.get::<_, String>(0)
        })
        .map_or(0, |v| v.parse().unwrap_or(0));

    let Keys { v10, v11 } = keys(browser).await?;

    let mut stmt = conn.prepare(&format!(
        "SELECT host_key, path, is_secure, expires_utc, name, value, encrypted_value \
         FROM cookies WHERE {ON_DOMAIN}"
    ))?;
    let rows = stmt.query_map([domain], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Vec<u8>>(6)?,
        ))
    })?;

    let mut cookies = Vec::new();
    let mut failed = 0;
    for row in rows {
        let (host, path, secure, expires_utc, name, value, encrypted) = row?;
        let value = if encrypted.is_empty() {
            value
        } else {
            let key = match &encrypted[..3] {
                b"v10" => v10.as_ref(),
                b"v11" => v11.as_ref(),
                _ => None,
            };
            let Some(v) = key.and_then(|k| decrypt(k, &encrypted[3..], &host, db_version).ok())
            else {
                failed += 1;
                continue;
            };
            v
        };
        cookies.push(Cookie {
            host,
            path,
            secure,
            expires: chromium_time_to_unix(expires_utc),
            name,
            value,
        });
    }
    if failed > 0 {
        eprintln!("warning: could not decrypt {failed} cookie(s) for {domain}");
    }
    Ok(cookies)
}

/// The profile's cookie database, which moved into `Network/` in Chromium 96.
fn cookie_db(browser: Browser, profile: &str) -> Result<PathBuf> {
    if let Browser::Firefox = browser {
        return firefox::cookie_db();
    }
    let config_dir = browser.config_dir()?;
    let profile_dir = config_dir.join(profile);
    let mut candidates = vec![
        profile_dir.join("Network/Cookies"),
        profile_dir.join("Cookies"),
    ];
    // Opera used to keep its only profile in the top folder rather than in
    // `Default/`, and profiles from then are still there.
    if matches!(browser, Browser::Opera | Browser::OperaGx) {
        candidates.extend([
            config_dir.join("Network/Cookies"),
            config_dir.join("Cookies"),
        ]);
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("no cookie database under {}", profile_dir.display()))
}

/// Opens a copy of the cookie database at `db_path`, since the browser holds
/// a lock on the live one. Its write-ahead log comes along when it has one
/// (Firefox's does), since recent changes wait there. The copy lasts as long
/// as the returned folder.
fn open_copy(db_path: &Path) -> Result<(tempfile::TempDir, rusqlite::Connection)> {
    let copy = tempfile::tempdir()?;
    let db = copy.path().join("cookies.sqlite");
    std::fs::copy(db_path, &db).with_context(|| format!("copying {}", db_path.display()))?;
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    match std::fs::copy(&wal, copy.path().join("cookies.sqlite-wal")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("copying {}", Path::new(&wal).display()));
        }
        _ => {}
    }
    let conn = rusqlite::Connection::open(&db)?;
    Ok((copy, conn))
}

/// The sites Joyscroller reads likes from: each one's name, domain and the
/// cookies it only sets while you're logged in.
const LOGINS: [(&str, &str, &[&str]); 4] = [
    (
        "YouTube",
        "youtube.com",
        &["SAPISID", "__Secure-3PAPISID", "LOGIN_INFO"],
    ),
    ("TikTok", "tiktok.com", &["sessionid"]),
    ("Instagram", "instagram.com", &["sessionid"]),
    ("X", "x.com", &["auth_token"]),
];

/// The browser to read logins from, picked afresh on every launch: of the
/// installed browsers with a `profile`, the one logged in to the most of
/// YouTube, TikTok, Instagram and X, and on a tie the most widely used. Only
/// cookie names and expiry dates are read, which Chromium doesn't encrypt,
/// so this never asks the keyring or the Keychain for anything. Safari's
/// logins can't be read without Full Disk Access, so then it's picked only
/// when no other browser is logged in to anything, and its syncs fail saying
/// what to turn on. With no browser to be found, it's Chrome, whose errors
/// at least say what's missing.
pub fn sniff(profile: &str) -> Browser {
    let found = Browser::MOST_USED_FIRST.into_iter().filter_map(|browser| {
        // No program means it isn't installed (what's left of one that was is
        // stale), and no cookies means it has never been opened.
        browser.executable().ok()?;
        match logins(browser, profile)? {
            Ok(sites) => Some((browser, Some(sites))),
            Err(e) if e.is::<safari::NoFullDiskAccess>() => {
                eprintln!("warning: couldn't check Safari's logins: {e}");
                Some((browser, None))
            }
            Err(e) => {
                eprintln!("warning: couldn't read {}'s logins: {e:#}", browser.name());
                None
            }
        }
    });
    let Some((browser, sites)) = most_logged_in(found) else {
        let names: Vec<&str> = Browser::MOST_USED_FIRST.iter().map(|b| b.name()).collect();
        eprintln!(
            "warning: found none of {} with a {profile} profile; install one, log in to \
             YouTube, TikTok, Instagram or X in it, and restart Joyscroller",
            names.join(", ")
        );
        return Browser::Chrome;
    };
    let name = browser.name();
    let Some(sites) = sites else {
        eprintln!("warning: using {name}, as no other browser is logged in, but can't read it yet");
        return browser;
    };
    if sites.is_empty() {
        eprintln!(
            "warning: using {name}, which isn't logged in to YouTube, TikTok, Instagram or X; \
             log in to them in your browser and restart Joyscroller"
        );
    } else {
        eprintln!("browser: using {name}, logged in to {}", sites.join(", "));
    }
    if browser.installed_headless().is_none() {
        eprintln!(
            "browser: {name} can't run TikTok, Instagram and X in the background, so \
             Joyscroller runs its own copy of Chrome's headless shell for them"
        );
    }
    browser
}

/// The sites a browser is logged in to, or `None` where they couldn't be
/// checked (Safari without Full Disk Access).
type Found = (Browser, Option<Vec<&'static str>>);

/// The first of `found` logged in to the most sites. One that couldn't be
/// checked might be logged in, so it beats one logged in to none. `found`
/// comes most used first, so that's the tie-break.
fn most_logged_in(found: impl IntoIterator<Item = Found>) -> Option<Found> {
    // Doubled, so an unknown one fits between none and one.
    let rank = |sites: &Option<Vec<_>>| sites.as_ref().map_or(1, |sites| 2 * sites.len());
    found.into_iter().fold(None, |best, candidate| match best {
        Some(best) if rank(&best.1) >= rank(&candidate.1) => Some(best),
        _ => Some(candidate),
    })
}

/// Which of `LOGINS`' sites `browser` holds an unexpired login for. `None`
/// when it has no cookies there at all.
fn logins(browser: Browser, profile: &str) -> Option<Result<Vec<&'static str>>> {
    if let Browser::Safari = browser {
        safari::cookie_file().ok()?;
        return Some(safari::read().map(|cookies| safari_logins(&cookies, crate::db::now())));
    }
    let db = cookie_db(browser, profile).ok()?;
    Some(open_copy(&db).and_then(|(_copy, conn)| logged_in_sites(&conn, browser)))
}

/// Which of `LOGINS`' sites Safari's `cookies` hold a login for that's
/// unexpired at `now`.
fn safari_logins(cookies: &[Cookie], now: i64) -> Vec<&'static str> {
    LOGINS
        .into_iter()
        .filter(|(_, domain, names)| {
            cookies.iter().any(|c| {
                safari::on_domain(&c.host, domain) && names.contains(&&*c.name) && c.expires > now
            })
        })
        .map(|(site, _, _)| site)
        .collect()
}

/// Which of `LOGINS`' sites `browser`'s cookie database holds an unexpired
/// login for.
fn logged_in_sites(conn: &rusqlite::Connection, browser: Browser) -> Result<Vec<&'static str>> {
    // Chromium's times are microseconds since 1601; 0 is a session cookie.
    let sql = match browser {
        Browser::Firefox => firefox::has_login_sql(),
        _ => format!(
            "SELECT EXISTS (SELECT 1 FROM cookies \
             WHERE {ON_DOMAIN} AND name = ?2 \
             AND (expires_utc = 0 \
                  OR expires_utc > (unixepoch() + 11644473600) * 1000000))"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut sites = Vec::new();
    for (site, domain, names) in LOGINS {
        for name in names {
            if stmt.query_row((domain, name), |r| r.get::<_, bool>(0))? {
                sites.push(site);
                break;
            }
        }
    }
    Ok(sites)
}

/// The keys for each encryption prefix; `None` where a value with that prefix
/// can't be decrypted, and is skipped.
struct Keys {
    v10: Option<[u8; 16]>,
    v11: Option<[u8; 16]>,
}

#[cfg(target_os = "linux")]
async fn keys(browser: Browser) -> Result<Keys> {
    let v11 = match keyring_password(browser).await {
        Ok(pw) => Some(derive_key(&pw, 1)),
        Err(e) => {
            eprintln!("warning: keyring lookup failed ({e:#}); v11 cookies will be skipped");
            None
        }
    };
    Ok(Keys {
        v10: Some(derive_key(b"peanuts", 1)),
        v11,
    })
}

#[cfg(target_os = "macos")]
async fn keys(browser: Browser) -> Result<Keys> {
    // Every sync and download reads cookies, and unless the user clicked
    // Always Allow, every Keychain read shows a dialog. So keep the key for
    // the rest of the run. The lock is held across the read so that two
    // readers at once wait for one dialog rather than showing two.
    static KEYS: tokio::sync::Mutex<Option<HashMap<&'static str, [u8; 16]>>> =
        tokio::sync::Mutex::const_new(None);
    let mut known = KEYS.lock().await;
    let known = known.get_or_insert_with(HashMap::new);
    let (service, account) = browser
        .keychain_item()
        .with_context(|| format!("{} has no Keychain item", browser.name()))?;
    let key = if let Some(key) = known.get(service) {
        *key
    } else {
        // Blocks until the user answers the dialog, so off the runtime.
        let password = tokio::task::spawn_blocking(move || keychain_password(service, account))
            .await?
            .with_context(|| {
                format!("reading '{service}' from the Keychain (was Joyscroller denied access?)")
            })?;
        let key = derive_key(&password, 1003);
        known.insert(service, key);
        key
    };
    Ok(Keys {
        v10: Some(key),
        v11: None,
    })
}

/// The password in the Keychain's generic password item for `service` and
/// `account`, or failing that, the first one for `service`, since some
/// browsers' accounts are only guesses (`Browser::keychain_item`).
#[cfg(target_os = "macos")]
fn keychain_password(service: &str, account: &str) -> Result<Vec<u8>> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;
    match security_framework::passwords::get_generic_password(service, account) {
        Err(e) if e.code() == NOT_FOUND => {
            let found = ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(service)
                .load_data(true)
                .limit(1)
                .search()?;
            match found.into_iter().next() {
                Some(SearchResult::Data(password)) => Ok(password),
                _ => bail!("no '{service}' item in the Keychain"),
            }
        }
        found => Ok(found?),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[expect(clippy::unused_async, reason = "matches the Linux and macOS versions")]
async fn keys(_browser: Browser) -> Result<Keys> {
    anyhow::bail!("reading browser logins isn't supported on this OS yet")
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn derive_key(password: &[u8], iterations: u32) -> [u8; 16] {
    pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, b"saltysalt", iterations)
}

fn decrypt(key: &[u8; 16], ciphertext: &[u8], host: &str, db_version: i64) -> Result<String> {
    let iv = [b' '; 16];
    let mut buf = ciphertext.to_vec();
    let plain = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("bad padding (wrong key?): {e}"))?;

    // Since cookie DB v24, the plaintext is prefixed with SHA256(host_key).
    let plain = if db_version >= 24 && plain.len() >= 32 && plain[..32] == Sha256::digest(host)[..]
    {
        &plain[32..]
    } else {
        plain
    };
    Ok(String::from_utf8(plain.to_vec())?)
}

#[cfg(target_os = "linux")]
async fn keyring_password(browser: Browser) -> Result<Vec<u8>> {
    use secret_service::{EncryptionType, SecretService};

    let application = browser
        .keyring_application()
        .with_context(|| format!("{} has no keyring secret", browser.name()))?;
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let attrs = HashMap::from([("application", application)]);
    let found = ss.search_items(attrs).await?;
    let item = match (found.unlocked.first(), found.locked.first()) {
        (Some(item), _) => item,
        (None, Some(item)) => {
            item.unlock().await?;
            item
        }
        (None, None) => bail!("no '{application}' secret in keyring"),
    };
    Ok(item.get_secret().await?)
}

/// Chromium stores times as microseconds since 1601-01-01 (the Windows epoch).
/// Convert to a Unix timestamp in seconds; 0 (session cookie) stays 0.
fn chromium_time_to_unix(microseconds: i64) -> i64 {
    if microseconds == 0 {
        return 0;
    }
    // 11644473600 = seconds between 1601-01-01 and 1970-01-01.
    microseconds / 1_000_000 - 11_644_473_600
}

/// Serialize cookies in the Netscape `cookies.txt` format that yt-dlp,
/// curl and friends accept.
pub fn to_netscape(cookies: &[Cookie]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n# Generated by joyscroller\n");
    for c in cookies {
        let include_subdomains = if c.host.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        // Writing to a String can't fail.
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            c.host, include_subdomains, c.path, secure, c.expires, c.name, c.value
        );
    }
    out
}

/// The browser profile isn't logged in to `site` ("TikTok", "Instagram"):
/// its cookies hold no session, or the site turned the session away. The UI
/// tells the user to log in there.
#[derive(Debug)]
pub struct LoggedOut(pub &'static str);

impl std::fmt::Display for LoggedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not logged in to {} in this browser profile", self.0)
    }
}

impl std::error::Error for LoggedOut {}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockModeEncrypt;

    use super::*;

    fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let mut buf = vec![0; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let iv = [b' '; 16];
        cbc::Encryptor::<aes::Aes128>::new(key.into(), &iv.into())
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec()
    }

    #[test]
    fn decrypts_pre_v24_value() {
        let key = derive_key(b"peanuts", 1);
        let ct = encrypt(&key, b"hello");
        assert_eq!(decrypt(&key, &ct, ".youtube.com", 23).unwrap(), "hello");
    }

    #[test]
    fn strips_host_hash_from_v24_value() {
        let key = derive_key(b"peanuts", 1);
        let mut plain = Sha256::digest(".youtube.com").to_vec();
        plain.extend_from_slice(b"hello");
        let ct = encrypt(&key, &plain);
        assert_eq!(decrypt(&key, &ct, ".youtube.com", 24).unwrap(), "hello");
    }

    #[test]
    fn wrong_key_fails() {
        let ct = encrypt(&derive_key(b"peanuts", 1), b"hello");
        assert!(decrypt(&derive_key(b"walnuts", 1), &ct, ".youtube.com", 23).is_err());
    }

    #[test]
    fn converts_chromium_time() {
        assert_eq!(chromium_time_to_unix(0), 0);
        assert_eq!(chromium_time_to_unix(11_644_473_600_000_000), 0);
        assert_eq!(chromium_time_to_unix(13_000_000_000_000_000), 1_355_526_400);
    }

    #[test]
    fn writes_netscape_lines() {
        let cookie = |host: &str, secure| Cookie {
            host: host.into(),
            path: "/".into(),
            secure,
            expires: 42,
            name: "SID".into(),
            value: "abc".into(),
        };
        let txt = to_netscape(&[
            cookie(".youtube.com", true),
            cookie("www.youtube.com", false),
        ]);
        let lines: Vec<&str> = txt.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                ".youtube.com\tTRUE\t/\tTRUE\t42\tSID\tabc",
                "www.youtube.com\tFALSE\t/\tFALSE\t42\tSID\tabc",
            ]
        );
    }

    /// A cookie database holding `cookies` as (`host_key`, `name`, `expires_utc`).
    fn cookie_store(cookies: &[(&str, &str, i64)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE cookies (host_key TEXT, name TEXT, expires_utc INTEGER)",
            [],
        )
        .unwrap();
        for cookie in cookies {
            conn.execute("INSERT INTO cookies VALUES (?1, ?2, ?3)", *cookie)
                .unwrap();
        }
        conn
    }

    #[test]
    fn matches_only_the_domain_and_its_subdomains() {
        let conn = cookie_store(&[
            (".x.com", "a", 0),
            ("x.com", "b", 0),
            ("api.x.com", "c", 0),
            (".stockx.com", "d", 0),
            ("x.com.evil", "e", 0),
        ]);
        let names: Vec<String> = conn
            .prepare(&format!("SELECT name FROM cookies WHERE {ON_DOMAIN}"))
            .unwrap()
            .query_map(["x.com"], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(names, ["a", "b", "c"]);
    }

    /// Far in the future, in Chromium's time.
    const LATER: i64 = 99_000_000_000_000_000;

    #[test]
    fn finds_logged_in_sites() {
        let conn = cookie_store(&[
            (".youtube.com", "LOGIN_INFO", LATER),
            (".youtube.com", "VISITOR_INFO1_LIVE", LATER),
            (".tiktok.com", "sessionid", 0),
            (".instagram.com", "csrftoken", LATER),
            (".x.com", "auth_token", LATER),
        ]);
        assert_eq!(
            logged_in_sites(&conn, Browser::Chrome).unwrap(),
            ["YouTube", "TikTok", "X"]
        );
    }

    #[test]
    fn ignores_expired_and_lookalike_logins() {
        let conn = cookie_store(&[
            (".youtube.com", "SAPISID", 13_000_000_000_000_000),
            (".nottiktok.com", "sessionid", LATER),
            ("www.instagram.com", "sessionid", LATER),
        ]);
        assert_eq!(
            logged_in_sites(&conn, Browser::Chrome).unwrap(),
            ["Instagram"]
        );
    }

    #[test]
    fn sniffs_every_browser() {
        assert_eq!(
            Browser::MOST_USED_FIRST.len(),
            Browser::value_variants().len()
        );
    }

    #[test]
    fn prefers_most_logins_then_most_used() {
        let picked =
            |found: Vec<Found>| most_logged_in(found).map(|(browser, _)| format!("{browser:?}"));
        assert_eq!(picked(vec![]), None);
        assert_eq!(
            picked(vec![
                (Browser::Chrome, Some(vec!["YouTube"])),
                (Browser::Safari, None),
                (Browser::Brave, Some(vec!["YouTube", "TikTok"])),
                (Browser::Vivaldi, Some(vec!["YouTube", "Instagram"])),
            ]),
            Some("Brave".into())
        );
        assert_eq!(
            picked(vec![
                (Browser::Chrome, Some(vec![])),
                (Browser::Edge, Some(vec![]))
            ]),
            Some("Chrome".into())
        );
        // Safari might be logged in, where Chrome surely isn't, but it isn't
        // picked over a browser that is.
        assert_eq!(
            picked(vec![
                (Browser::Chrome, Some(vec![])),
                (Browser::Safari, None)
            ]),
            Some("Safari".into())
        );
        assert_eq!(
            picked(vec![
                (Browser::Safari, None),
                (Browser::Edge, Some(vec!["TikTok"]))
            ]),
            Some("Edge".into())
        );
    }

    #[test]
    fn finds_safari_logins() {
        let cookie = |host: &str, name: &str, expires| Cookie {
            host: host.into(),
            path: "/".into(),
            secure: true,
            expires,
            name: name.into(),
            value: "v".into(),
        };
        let cookies = [
            cookie(".youtube.com", "SAPISID", 2_000),
            cookie(".tiktok.com", "sessionid", 500),
            cookie(".notinstagram.com", "sessionid", 2_000),
            cookie(".instagram.com", "csrftoken", 2_000),
        ];
        assert_eq!(safari_logins(&cookies, 1_000), ["YouTube"]);
    }
}
