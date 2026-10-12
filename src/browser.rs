//! Reads (and decrypts) cookies straight out of a Chromium-family browser's
//! on-disk cookie store, so joyscroller can act as the logged-in user.
//!
//! Linux only for now. Chromium encrypts cookie values with AES-128-CBC using a
//! key derived from a password that lives in the desktop keyring
//! ("<Browser> Safe Storage" in gnome-keyring / KWallet).

use std::collections::HashMap;
use std::path::PathBuf;

use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result, anyhow, bail};
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
}

impl Browser {
    fn config_dir(self) -> PathBuf {
        let base = dirs::config_dir().expect("no XDG config dir");
        base.join(match self {
            Browser::Brave => "BraveSoftware/Brave-Browser",
            Browser::Chrome => "google-chrome",
            Browser::Chromium => "chromium",
            Browser::Vivaldi => "vivaldi",
            Browser::Edge => "microsoft-edge",
        })
    }

    /// The `application` attribute Chromium stores its keyring secret under.
    fn keyring_application(self) -> &'static str {
        match self {
            Browser::Brave => "brave",
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Vivaldi => "vivaldi",
            Browser::Edge => "microsoft-edge",
        }
    }
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

/// All cookies whose host matches `domain` (e.g. "youtube.com" matches
/// ".youtube.com" and "www.youtube.com").
pub async fn cookies_for(browser: Browser, profile: &str, domain: &str) -> Result<Vec<Cookie>> {
    let profile_dir = browser.config_dir().join(profile);
    let db_path = [profile_dir.join("Network/Cookies"), profile_dir.join("Cookies")]
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("no cookie database under {}", profile_dir.display()))?;

    // The browser holds a lock on the live DB, so read from a copy.
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::copy(&db_path, tmp.path())
        .with_context(|| format!("copying {}", db_path.display()))?;
    let conn = rusqlite::Connection::open(tmp.path())?;

    let db_version: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| {
            r.get::<_, String>(0)
        })
        .map(|v| v.parse().unwrap_or(0))
        .unwrap_or(0);

    let v10_key = derive_key(b"peanuts");
    let v11_key = match keyring_password(browser).await {
        Ok(pw) => Some(derive_key(&pw)),
        Err(e) => {
            eprintln!("warning: keyring lookup failed ({e:#}); v11 cookies will be skipped");
            None
        }
    };

    let mut stmt = conn.prepare(
        "SELECT host_key, path, is_secure, expires_utc, name, value, encrypted_value \
         FROM cookies WHERE host_key LIKE ?1",
    )?;
    let rows = stmt.query_map([format!("%{domain}")], |r| {
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
                b"v10" => Some(&v10_key),
                b"v11" => v11_key.as_ref(),
                _ => None,
            };
            match key.and_then(|k| decrypt(k, &encrypted[3..], &host, db_version).ok()) {
                Some(v) => v,
                None => {
                    failed += 1;
                    continue;
                }
            }
        };
        cookies.push(Cookie { host, path, secure, expires: chromium_time_to_unix(expires_utc), name, value });
    }
    if failed > 0 {
        eprintln!("warning: could not decrypt {failed} cookie(s) for {domain}");
    }
    Ok(cookies)
}

fn derive_key(password: &[u8]) -> [u8; 16] {
    pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, b"saltysalt", 1)
}

fn decrypt(key: &[u8; 16], ciphertext: &[u8], host: &str, db_version: i64) -> Result<String> {
    let iv = [b' '; 16];
    let mut buf = ciphertext.to_vec();
    let plain = Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("bad padding (wrong key?)"))?;

    // Since cookie DB v24, the plaintext is prefixed with SHA256(host_key).
    let plain = if db_version >= 24 && plain.len() >= 32 && plain[..32] == Sha256::digest(host)[..] {
        &plain[32..]
    } else {
        plain
    };
    Ok(String::from_utf8(plain.to_vec())?)
}

async fn keyring_password(browser: Browser) -> Result<Vec<u8>> {
    use secret_service::{EncryptionType, SecretService};

    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let attrs = HashMap::from([("application", browser.keyring_application())]);
    let found = ss.search_items(attrs).await?;
    let item = match (found.unlocked.first(), found.locked.first()) {
        (Some(item), _) => item,
        (None, Some(item)) => {
            item.unlock().await?;
            item
        }
        (None, None) => bail!("no '{}' secret in keyring", browser.keyring_application()),
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
        let include_subdomains = if c.host.starts_with('.') { "TRUE" } else { "FALSE" };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.host, include_subdomains, c.path, secure, c.expires, c.name, c.value
        ));
    }
    out
}
