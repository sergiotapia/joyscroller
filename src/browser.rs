//! Reads (and decrypts) cookies straight out of a Chromium-family browser's
//! on-disk cookie store, so joyscroller can act as the logged-in user.
//!
//! Linux only for now. Chromium encrypts cookie values with AES-128-CBC using a
//! key derived from a password that lives in the desktop keyring
//! ("<Browser> Safe Storage" in gnome-keyring / KWallet).

use std::collections::HashMap;
use std::fmt::Write;
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
    fn config_dir(self) -> Result<PathBuf> {
        let base = dirs::config_dir().context("no XDG config dir")?;
        Ok(base.join(match self {
            Browser::Brave => "BraveSoftware/Brave-Browser",
            Browser::Chrome => "google-chrome",
            Browser::Chromium => "chromium",
            Browser::Vivaldi => "vivaldi",
            Browser::Edge => "microsoft-edge",
        }))
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
    let profile_dir = browser.config_dir()?.join(profile);
    let db_path = [
        profile_dir.join("Network/Cookies"),
        profile_dir.join("Cookies"),
    ]
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
        .map_or(0, |v| v.parse().unwrap_or(0));

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

fn derive_key(password: &[u8]) -> [u8; 16] {
    pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, b"saltysalt", 1)
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
        let key = derive_key(b"peanuts");
        let ct = encrypt(&key, b"hello");
        assert_eq!(decrypt(&key, &ct, ".youtube.com", 23).unwrap(), "hello");
    }

    #[test]
    fn strips_host_hash_from_v24_value() {
        let key = derive_key(b"peanuts");
        let mut plain = Sha256::digest(".youtube.com").to_vec();
        plain.extend_from_slice(b"hello");
        let ct = encrypt(&key, &plain);
        assert_eq!(decrypt(&key, &ct, ".youtube.com", 24).unwrap(), "hello");
    }

    #[test]
    fn wrong_key_fails() {
        let ct = encrypt(&derive_key(b"peanuts"), b"hello");
        assert!(decrypt(&derive_key(b"walnuts"), &ct, ".youtube.com", 23).is_err());
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
}
