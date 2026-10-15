//! Safari's logins, on macOS. Safari doesn't encrypt cookies either: it keeps
//! them all in one file, `Cookies.binarycookies`, so there's no Keychain to
//! ask. But macOS only lets apps with Full Disk Access open that file, and a
//! read without it fails with "Operation not permitted" (`NoFullDiskAccess`).
//! Safari can't be driven over the Chrome DevTools Protocol, so TikTok and
//! Instagram run in another browser with these cookies loaded
//! (`Browser::installed_headless`).
//!
//! The file's format, as yt-dlp's `cookies.py` reads it: "cook", then the
//! number of pages and each page's size (big-endian), then the pages. A page
//! is `00 00 01 00`, its number of records and each one's offset in the page
//! (little-endian, like everything from here on). A record holds its size,
//! flags (1 is secure), offsets from the record's start to its NUL-terminated
//! host, name, path and value, and its expiry as seconds since 2001 (Mac
//! absolute time, a double).

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, ensure};

use super::Cookie;

/// Seconds between 1970-01-01 and 2001-01-01, where Mac absolute time starts.
const MAC_EPOCH: i64 = 978_307_200;

/// Reading Safari's cookies failed because macOS wouldn't let us: the app
/// needs Full Disk Access. The message says where to turn it on, and is shown
/// in the UI as the reason a sync failed.
#[derive(Debug)]
pub struct NoFullDiskAccess;

impl std::fmt::Display for NoFullDiskAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Run from a terminal, macOS asks whether the terminal has access.
        if crate::desktop::bundle().is_some() {
            // An ad-hoc signed app looks like a different app to macOS after
            // every update, so a grant from before one no longer applies even
            // though the switch still shows it on.
            write!(
                f,
                "macOS only lets apps with Full Disk Access read Safari's logins: turn on \
                 Joyscroller in System Settings → Privacy & Security → Full Disk Access \
                 (if it's already on, remove it with − and add it again), then restart Joyscroller"
            )
        } else {
            write!(
                f,
                "macOS only lets apps with Full Disk Access read Safari's logins: turn it on \
                 for the terminal app running Joyscroller in System Settings → Privacy & \
                 Security → Full Disk Access, then restart it"
            )
        }
    }
}

impl std::error::Error for NoFullDiskAccess {}

/// The cookies for `domain` ("youtube.com" matches ".youtube.com" and
/// "www.youtube.com").
pub(super) fn cookies_for(domain: &str) -> Result<Vec<Cookie>> {
    Ok(read()?
        .into_iter()
        .filter(|cookie| on_domain(&cookie.host, domain))
        .collect())
}

/// Whether a cookie for `host` is sent to `domain`.
pub(super) fn on_domain(host: &str, domain: &str) -> bool {
    host.strip_prefix('.').unwrap_or(host) == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|sub| sub.ends_with('.'))
}

/// Every cookie Safari has.
pub(super) fn read() -> Result<Vec<Cookie>> {
    let file = cookie_file()?;
    let data = match std::fs::read(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(anyhow::Error::new(e).context(NoFullDiskAccess));
        }
        read => read.with_context(|| format!("reading {}", file.display()))?,
    };
    parse(&data).with_context(|| format!("reading {}", file.display()))
}

/// Safari's cookie file: in its sandbox's container on current macOS, in
/// `~/Library/Cookies` on old ones. macOS lets anyone see the file is there,
/// but not open it.
#[cfg(target_os = "macos")]
pub(super) fn cookie_file() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home folder")?;
    let candidates = [
        home.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies"),
        home.join("Library/Cookies/Cookies.binarycookies"),
    ];
    candidates
        .iter()
        .find(|file| file.exists())
        .cloned()
        .ok_or_else(|| anyhow!("Safari has no cookies at {candidates:?}"))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn cookie_file() -> Result<PathBuf> {
    Err(anyhow!("Safari isn't made for this OS"))
}

/// Parses a `Cookies.binarycookies` file.
fn parse(data: &[u8]) -> Result<Vec<Cookie>> {
    ensure!(data.starts_with(b"cook"), "not a Safari cookie file");
    let pages = be_u32(data, 4)? as usize;
    let mut start = 8 + 4 * pages;
    let mut cookies = Vec::new();
    for i in 0..pages {
        let size = be_u32(data, 8 + 4 * i)? as usize;
        let page = data
            .get(start..start + size)
            .with_context(|| format!("page {i} runs past the end"))?;
        parse_page(page, &mut cookies).with_context(|| format!("in page {i}"))?;
        start += size;
    }
    Ok(cookies)
}

fn parse_page(page: &[u8], cookies: &mut Vec<Cookie>) -> Result<()> {
    ensure!(page.starts_with(&[0, 0, 1, 0]), "bad page signature");
    let records = le_u32(page, 4)? as usize;
    for i in 0..records {
        let offset = le_u32(page, 8 + 4 * i)? as usize;
        let record = page
            .get(offset..)
            .with_context(|| format!("record {i} starts past the page's end"))?;
        cookies.push(parse_record(record).with_context(|| format!("in record {i}"))?);
    }
    Ok(())
}

fn parse_record(record: &[u8]) -> Result<Cookie> {
    let size = le_u32(record, 0)? as usize;
    let record = record.get(..size).context("record runs past the page")?;
    let text = |at: usize| -> Result<String> {
        let start = le_u32(record, at)? as usize;
        let rest = record.get(start..).context("text starts past the record")?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .context("text isn't NUL-terminated")?;
        Ok(String::from_utf8(rest[..end].to_vec())?)
    };
    let expires = record
        .get(40..48)
        .and_then(|b| b.try_into().ok())
        .map(f64::from_le_bytes)
        .context("record too short for its expiry")?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Mac absolute times are whole seconds well inside i64"
    )]
    let expires = expires as i64 + MAC_EPOCH;
    Ok(Cookie {
        host: text(16)?,
        name: text(20)?,
        path: text(24)?,
        value: text(28)?,
        secure: le_u32(record, 8)? & 1 != 0,
        expires,
    })
}

fn be_u32(data: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(four_bytes(data, at)?))
}

fn le_u32(data: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(four_bytes(data, at)?))
}

fn four_bytes(data: &[u8], at: usize) -> Result<[u8; 4]> {
    data.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .with_context(|| format!("cut short at byte {at}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// yt-dlp's sample (`test/test_cookies.py`), from a real Safari.
    const YT_DLP_SAMPLE: &[u8] = b"cook\x00\x00\x00\x01\x00\x00\x00i\x00\x00\x01\x00\x01\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00Y\
\x00\x00\x00\x00\x00\x00\x00 \x00\x00\x00\x00\x00\x00\x008\x00\x00\x00B\x00\x00\x00F\x00\x00\x00H\
\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x80\x03\xa5>\xc3A\x00\x00\x80\xc3\x07:\xc3A\
localhost\x00foo\x00/\x00test%20%3Bcookie\x00\x00\x00\x054\x07\x17 \x05\x00\x00\x00Kbplist00\xd1\x01\
\x02_\x10\x18NSHTTPCookieAcceptPolicy\x10\x02\x08\x0b&\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\
\x00\x00\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00(";

    #[test]
    fn parses_yt_dlps_sample() {
        let cookies = parse(YT_DLP_SAMPLE).unwrap();
        assert_eq!(cookies.len(), 1);
        let cookie = &cookies[0];
        assert_eq!(
            (&*cookie.host, &*cookie.name, &*cookie.path, &*cookie.value),
            ("localhost", "foo", "/", "test%20%3Bcookie")
        );
        assert!(!cookie.secure);
        // 2021-06-18 21:39:19 UTC
        assert_eq!(cookie.expires, 1_624_052_359);
    }

    /// A record as Safari writes it.
    fn record(flags: u32, expires: f64, [host, name, path, value]: [&str; 4]) -> Vec<u8> {
        let mut texts = Vec::new();
        let mut offsets = Vec::new();
        for text in [host, name, path, value] {
            offsets.push(56 + texts.len());
            texts.extend_from_slice(text.as_bytes());
            texts.push(0);
        }
        let mut out = Vec::new();
        let size = u32::try_from(56 + texts.len()).unwrap();
        for field in [size, 0, flags, 0] {
            out.extend(field.to_le_bytes());
        }
        for offset in offsets {
            out.extend(u32::try_from(offset).unwrap().to_le_bytes());
        }
        out.extend([0; 8]);
        out.extend(expires.to_le_bytes());
        out.extend(0f64.to_le_bytes());
        out.extend(texts);
        out
    }

    /// A cookie file with `records` split over pages.
    fn file(pages: &[&[Vec<u8>]]) -> Vec<u8> {
        let pages: Vec<Vec<u8>> = pages
            .iter()
            .map(|records| {
                let mut page = vec![0, 0, 1, 0];
                page.extend(u32::try_from(records.len()).unwrap().to_le_bytes());
                let mut offset = 8 + 4 * records.len() + 4;
                for record in *records {
                    page.extend(u32::try_from(offset).unwrap().to_le_bytes());
                    offset += record.len();
                }
                page.extend([0; 4]);
                for record in *records {
                    page.extend(record);
                }
                page
            })
            .collect();
        let mut out = b"cook".to_vec();
        out.extend(u32::try_from(pages.len()).unwrap().to_be_bytes());
        for page in &pages {
            out.extend(u32::try_from(page.len()).unwrap().to_be_bytes());
        }
        for page in pages {
            out.extend(page);
        }
        // The footer's checksum, which yt-dlp doesn't check either.
        out.extend([0; 8]);
        out
    }

    #[test]
    fn parses_pages_of_records() {
        let data = file(&[
            &[
                record(1, 900_000_000.0, [".youtube.com", "SAPISID", "/", "abc"]),
                record(5, 900_000_000.5, ["www.tiktok.com", "tt", "/foo", ""]),
            ],
            &[],
            &[record(0, 0.0, [".instagram.com", "sessionid", "/", "x"])],
        ]);
        let cookies: Vec<_> = parse(&data)
            .unwrap()
            .into_iter()
            .map(|c| (c.host, c.name, c.path, c.value, c.secure, c.expires))
            .collect();
        assert_eq!(
            cookies,
            [
                (
                    ".youtube.com".into(),
                    "SAPISID".into(),
                    "/".into(),
                    "abc".into(),
                    true,
                    1_878_307_200
                ),
                (
                    "www.tiktok.com".into(),
                    "tt".into(),
                    "/foo".into(),
                    String::new(),
                    true,
                    1_878_307_200
                ),
                (
                    ".instagram.com".into(),
                    "sessionid".into(),
                    "/".into(),
                    "x".into(),
                    false,
                    MAC_EPOCH
                ),
            ]
        );
    }

    #[test]
    fn rejects_damaged_files() {
        let good = file(&[&[record(0, 0.0, ["a.com", "n", "/", "v"])]]);
        assert!(parse(b"nope").is_err());
        assert!(parse(&good[..good.len() - 20]).is_err());
        let mut no_nul = good;
        let last = no_nul.len() - 9;
        no_nul[last] = b'x';
        assert!(parse(&no_nul).is_err());
    }

    #[test]
    fn matches_domains() {
        assert!(on_domain(".youtube.com", "youtube.com"));
        assert!(on_domain("youtube.com", "youtube.com"));
        assert!(on_domain("www.youtube.com", "youtube.com"));
        assert!(!on_domain(".notyoutube.com", "youtube.com"));
        assert!(!on_domain("youtube.com.evil", "youtube.com"));
    }
}
