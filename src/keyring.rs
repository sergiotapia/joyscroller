//! The PDS account's app password, kept in the OS keyring (the Secret
//! Service on Linux, the login Keychain on macOS), never in the database or
//! a file. It's the one secret Joyscroller keeps of its own; the platforms'
//! logins are read from the browser each time. Windows has no keyring code
//! yet (#2), so connecting a PDS there says so.
//!
//! Try it: connect an account in Settings, then look for "Joyscroller" in
//! Seahorse (Linux) or Keychain Access (macOS).

use anyhow::Result;

/// The keyring item's service name; each account is an item named by DID.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    expect(dead_code, reason = "no keyring on this OS yet")
)]
const SERVICE: &str = "Joyscroller Personal Data Server";

/// Keep `did`'s app password, replacing any kept before.
pub async fn save(did: &str, handle: &str, app_password: &str) -> Result<()> {
    imp::save(did, handle, app_password).await
}

/// `did`'s app password, if one is kept.
pub async fn load(did: &str) -> Result<Option<String>> {
    imp::load(did).await
}

/// Forget `did`'s app password, if one is kept.
pub async fn forget(did: &str) -> Result<()> {
    imp::forget(did).await
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use anyhow::{Context, Result};
    use secret_service::{EncryptionType, SecretService};

    use super::SERVICE;

    fn attributes(did: &str) -> HashMap<&str, &str> {
        HashMap::from([("service", SERVICE), ("did", did)])
    }

    pub async fn save(did: &str, handle: &str, app_password: &str) -> Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to the keyring")?;
        let collection = ss
            .get_default_collection()
            .await
            .context("opening the keyring's default collection")?;
        if collection.is_locked().await? {
            collection.unlock().await.context("unlocking the keyring")?;
        }
        collection
            .create_item(
                &format!("Joyscroller: app password for {handle}"),
                attributes(did),
                app_password.as_bytes(),
                true,
                "text/plain",
            )
            .await
            .context("saving the app password in the keyring")?;
        Ok(())
    }

    pub async fn load(did: &str) -> Result<Option<String>> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to the keyring")?;
        let found = ss.search_items(attributes(did)).await?;
        let item = match (found.unlocked.first(), found.locked.first()) {
            (Some(item), _) => item,
            (None, Some(item)) => {
                item.unlock().await.context("unlocking the keyring")?;
                item
            }
            (None, None) => return Ok(None),
        };
        let secret = item.get_secret().await?;
        Ok(Some(
            String::from_utf8(secret).context("the kept app password isn't text")?,
        ))
    }

    pub async fn forget(did: &str) -> Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .await
            .context("connecting to the keyring")?;
        let found = ss.search_items(attributes(did)).await?;
        for item in found.unlocked.iter().chain(&found.locked) {
            item.delete()
                .await
                .context("deleting the app password from the keyring")?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Context, Result};
    use security_framework::passwords;

    use super::SERVICE;

    /// The Keychain's "item not found".
    const NOT_FOUND: i32 = -25300;

    pub async fn save(did: &str, _handle: &str, app_password: &str) -> Result<()> {
        let (did, app_password) = (did.to_owned(), app_password.to_owned());
        tokio::task::spawn_blocking(move || {
            passwords::set_generic_password(SERVICE, &did, app_password.as_bytes())
                .context("saving the app password in the Keychain")
        })
        .await?
    }

    pub async fn load(did: &str) -> Result<Option<String>> {
        let did = did.to_owned();
        tokio::task::spawn_blocking(
            move || match passwords::get_generic_password(SERVICE, &did) {
                Ok(secret) => Ok(Some(
                    String::from_utf8(secret).context("the kept app password isn't text")?,
                )),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(e).context("reading the app password from the Keychain"),
            },
        )
        .await?
    }

    pub async fn forget(did: &str) -> Result<()> {
        let did = did.to_owned();
        tokio::task::spawn_blocking(move || {
            match passwords::delete_generic_password(SERVICE, &did) {
                Err(e) if e.code() != NOT_FOUND => {
                    Err(e).context("deleting the app password from the Keychain")
                }
                _ => Ok(()),
            }
        })
        .await?
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[expect(
    clippy::unused_async,
    reason = "the same signatures as the keyrings on Linux and macOS"
)]
mod imp {
    use anyhow::{Result, bail};

    pub async fn save(_did: &str, _handle: &str, _app_password: &str) -> Result<()> {
        bail!("Joyscroller can't keep an app password on this system yet")
    }

    pub async fn load(_did: &str) -> Result<Option<String>> {
        Ok(None)
    }

    pub async fn forget(_did: &str) -> Result<()> {
        Ok(())
    }
}
