//! Firefox's logins. Unlike Chromium, Firefox doesn't encrypt cookies: each
//! profile keeps them in plain `cookies.sqlite`, so there's no keyring or
//! Keychain to ask. Profiles have random folder names, listed in each
//! Firefox folder's `profiles.ini`. Firefox can't be driven over the Chrome
//! DevTools Protocol, so TikTok and Instagram run in another browser with
//! these cookies loaded (`Browser::installed_headless`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};

use super::Cookie;

/// Whether a cookie belongs to the site itself rather than to a page
/// embedding it: no container tab, and either unpartitioned or partitioned
/// under the site's own address (CHIPS cookies YouTube sets on itself).
/// `?1` is the domain.
const FIRST_PARTY: &str =
    "(originAttributes = '' OR originAttributes = '^partitionKey=%28https%2C' || ?1 || '%29')";

/// A cookie's host matches domain `?1` ("youtube.com" matches ".youtube.com"
/// and "www.youtube.com").
const ON_DOMAIN: &str = "(host = ?1 OR host LIKE '%.' || ?1)";

/// Expiry as Unix seconds: Firefox 158 stores milliseconds, earlier ones
/// seconds, and a database carries on with what it has until a cookie is set
/// again, so either can turn up. Seconds won't pass 10^11 until the year 5138.
const EXPIRY_SECONDS: &str = "(CASE WHEN expiry > 100000000000 THEN expiry / 1000 ELSE expiry END)";

/// SQL for whether a Firefox cookie database holds an unexpired first-party
/// cookie for domain `?1` named `?2`. (Firefox keeps session cookies out of
/// `cookies.sqlite`, so every row has an expiry.)
pub(super) fn has_login_sql() -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM moz_cookies \
         WHERE {ON_DOMAIN} AND {FIRST_PARTY} AND name = ?2 AND {EXPIRY_SECONDS} > unixepoch())"
    )
}

/// The first-party cookies for `domain` in the profile in use.
pub(super) fn cookies_for(domain: &str) -> Result<Vec<Cookie>> {
    let (_copy, conn) = super::open_copy(&cookie_db()?)?;
    cookies_in(&conn, domain)
}

fn cookies_in(conn: &rusqlite::Connection, domain: &str) -> Result<Vec<Cookie>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT host, path, isSecure, {EXPIRY_SECONDS}, name, value FROM moz_cookies \
         WHERE {ON_DOMAIN} AND {FIRST_PARTY}"
    ))?;
    let cookies = stmt
        .query_map([domain], |r| {
            Ok(Cookie {
                host: r.get(0)?,
                path: r.get(1)?,
                secure: r.get::<_, i64>(2)? != 0,
                expires: r.get(3)?,
                name: r.get(4)?,
                value: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(cookies)
}

/// The cookie database of the profile in use: of every profile Firefox lists,
/// the one whose cookies changed last. People keep old profiles around, and
/// Developer Edition and Nightly each have a default of their own.
pub(super) fn cookie_db() -> Result<PathBuf> {
    let roots = roots()?;
    roots
        .iter()
        .filter_map(|root| {
            let ini = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
            Some(profile_dirs(root, &ini))
        })
        .flatten()
        .map(|profile| profile.join("cookies.sqlite"))
        .filter_map(|db| Some((last_changed(&db)?, db)))
        .max_by_key(|(changed, _)| *changed)
        .map(|(_, db)| db)
        .ok_or_else(|| anyhow!("no Firefox profile with cookies under {roots:?}"))
}

/// When `db` or its write-ahead log (where recent changes wait) last changed.
fn last_changed(db: &Path) -> Option<SystemTime> {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    modified(db).max(modified(Path::new(&wal)))
}

/// The folders that may hold a `profiles.ini`.
#[cfg(target_os = "macos")]
fn roots() -> Result<Vec<PathBuf>> {
    // ~/Library/Application Support
    let base = dirs::config_dir().context("no Application Support folder")?;
    Ok(vec![base.join("Firefox")])
}

/// The folder that holds `profiles.ini`.
#[cfg(windows)]
fn roots() -> Result<Vec<PathBuf>> {
    // %APPDATA%
    let base = dirs::config_dir().context("no AppData folder")?;
    Ok(vec![base.join("Mozilla/Firefox")])
}

/// The folders that may hold a `profiles.ini`: the classic one, the XDG one
/// recent Firefox uses for new installs, and the Snap (Ubuntu's default) and
/// Flatpak sandboxes' copies of each.
#[cfg(not(any(target_os = "macos", windows)))]
fn roots() -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().context("no home folder")?;
    let config = dirs::config_dir().context("no XDG config dir")?;
    let flatpak = home.join(".var/app/org.mozilla.firefox");
    Ok(vec![
        home.join(".mozilla/firefox"),
        config.join("mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
        flatpak.join(".mozilla/firefox"),
        flatpak.join("config/mozilla/firefox"),
    ])
}

/// The profile folders listed in `ini`, the `profiles.ini` in `root`: one
/// `[ProfileN]` section each, with a `Path` relative to `root` when
/// `IsRelative=1`.
fn profile_dirs(root: &Path, ini: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut in_profile = false;
    let mut path = None;
    let mut relative = true;
    let mut finish = |path: &mut Option<String>, relative: bool| {
        if let Some(path) = path.take() {
            dirs.push(if relative {
                root.join(path)
            } else {
                PathBuf::from(path)
            });
        }
    };
    for line in ini.lines().map(str::trim) {
        if line.starts_with('[') {
            finish(&mut path, relative);
            in_profile = line.starts_with("[Profile");
            relative = true;
        } else if in_profile && let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Path" => path = Some(value.trim().to_owned()),
                "IsRelative" => relative = value.trim() != "0",
                _ => {}
            }
        }
    }
    finish(&mut path, relative);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_profiles_from_profiles_ini() {
        let ini = "\
[Install4F96D1932A9F858E]
Default=Profiles/abcd.default-release
Locked=1

[Profile1]
Name=default
IsRelative=1
Path=Profiles/efgh.default
Default=1

[Profile0]
Name=default-release
IsRelative=1
Path=Profiles/abcd.default-release

[Profile2]
Name=elsewhere
IsRelative=0
Path=/mnt/profiles/ijkl.elsewhere

[General]
StartWithLastProfile=1
Version=2
";
        let root = Path::new("/home/me/.mozilla/firefox");
        assert_eq!(
            profile_dirs(root, ini),
            [
                root.join("Profiles/efgh.default"),
                root.join("Profiles/abcd.default-release"),
                PathBuf::from("/mnt/profiles/ijkl.elsewhere"),
            ]
        );
    }

    /// A Firefox cookie database holding `cookies` as (`host`, `name`,
    /// `expiry`, `originAttributes`), in Firefox 158's schema.
    fn cookie_store(cookies: &[(&str, &str, i64, &str)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, \
             originAttributes TEXT NOT NULL DEFAULT '', name TEXT, value TEXT, host TEXT, \
             path TEXT, expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER, \
             isSecure INTEGER, isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0, \
             sameSite INTEGER DEFAULT 0, schemeMap INTEGER DEFAULT 0, \
             isPartitionedAttributeSet INTEGER DEFAULT 0, updateTime INTEGER, \
             CONSTRAINT moz_uniqueid UNIQUE (name, host, path, originAttributes))",
            [],
        )
        .unwrap();
        for (host, name, expiry, origin) in cookies {
            conn.execute(
                "INSERT INTO moz_cookies (originAttributes, name, value, host, path, expiry, \
                 isSecure) VALUES (?4, ?2, 'v', ?1, '/', ?3, 1)",
                (host, name, expiry, origin),
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn reads_first_party_cookies_in_seconds() {
        let conn = cookie_store(&[
            (".youtube.com", "PREF", 1_826_647_198_824, ""),
            ("www.youtube.com", "OLD", 1_826_647_198, ""),
            (
                ".youtube.com",
                "__Secure-ROLLOUT_TOKEN",
                1_807_639_198_816,
                "^partitionKey=%28https%2Cyoutube.com%29",
            ),
            (
                ".youtube.com",
                "EMBEDDED",
                1_807_639_198_816,
                "^partitionKey=%28https%2Cexample.com%29",
            ),
            (
                ".youtube.com",
                "CONTAINER",
                1_807_639_198_816,
                "^userContextId=2",
            ),
            (".notyoutube.com", "PREF", 1_826_647_198_824, ""),
        ]);
        let cookies: Vec<_> = cookies_in(&conn, "youtube.com")
            .unwrap()
            .into_iter()
            .map(|c| (c.name, c.expires, c.secure))
            .collect();
        assert_eq!(
            cookies,
            [
                ("PREF".into(), 1_826_647_198, true),
                ("OLD".into(), 1_826_647_198, true),
                ("__Secure-ROLLOUT_TOKEN".into(), 1_807_639_198, true),
            ]
        );
    }

    #[test]
    fn finds_unexpired_logins() {
        let conn = cookie_store(&[
            (".tiktok.com", "sessionid", 99_999_999_999_999, ""),
            (".instagram.com", "sessionid", 1_600_000_000_000, ""),
            (
                ".youtube.com",
                "LOGIN_INFO",
                99_999_999_999,
                "^userContextId=1",
            ),
        ]);
        let has = |domain: &str, name: &str| {
            conn.query_row(&has_login_sql(), (domain, name), |r| r.get::<_, bool>(0))
                .unwrap()
        };
        assert!(has("tiktok.com", "sessionid"));
        assert!(!has("instagram.com", "sessionid"));
        assert!(!has("youtube.com", "LOGIN_INFO"));
    }
}
