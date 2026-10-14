//! How the app fits into each desktop: where its data goes, opening the UI in
//! the browser, and the macOS app bundle's quirks.
//!
//! Everywhere but a macOS `.app`, the database, tools and downloads sit next
//! to the executable, where people will look for them. A bundle can't hold
//! them (writing inside it breaks its signature), so there the downloads go in
//! `~/Movies/Joyscroller` and everything else in
//! `~/Library/Application Support/Joyscroller`. A bundled app also has no
//! terminal, so it opens the UI itself and logs to a file, and it runs detached
//! from the process macOS launched, so opening the app again reaches it.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// The folder name under Application Support and Movies.
const APP_NAME: &str = "Joyscroller";

/// Set on the copy of a bundled app that does the work, so it doesn't relaunch
/// itself again.
const LOGGING_ENV: &str = "JOYSCROLLER_LOGGING";

/// A log past this size is moved aside to `joyscroller.old.log` at launch.
const LOG_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

/// The `.app` folder the running executable is inside, on macOS.
pub fn bundle() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    bundle_of(&std::env::current_exe().ok()?)
}

/// The `.app` folder `exe` is the main executable of: `X.app/Contents/MacOS/x`.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    is_bundle.then(|| app.to_owned())
}

/// Where the database, backups, tools and log go when bundled.
pub fn bundled_data_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("finding Application Support")?
        .join(APP_NAME))
}

/// Where downloads go when bundled.
pub fn bundled_media_dir() -> Result<PathBuf> {
    Ok(dirs::video_dir()
        .context("finding the Movies folder")?
        .join(APP_NAME))
}

/// Whether launching should open the UI: on Windows and in a macOS bundle,
/// where the app is started by double-clicking and nothing else shows it.
/// On Linux it's started from a terminal, which prints the address.
pub fn opens_ui_on_launch() -> bool {
    cfg!(windows) || bundle().is_some()
}

/// Open `url` in the default browser. Failing to is only logged: the address
/// is in the log too.
pub fn open_in_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        // Explorer hands a URL to the default browser, without `cmd`'s quoting.
        std::process::Command::new("explorer")
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url);
    if let Err(e) = cmd.spawn() {
        eprintln!("warning: couldn't open {url} in the browser: {e}");
    }
}

/// Whether the joyscroller UI answers at `addr`, so a launch that finds the
/// port taken can tell "already running" from another program on it.
pub async fn already_running(addr: &str) -> bool {
    let url = format!("http://{addr}/");
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        agent
            .get(&url)
            .call()
            .ok()
            .and_then(|mut response| response.body_mut().read_to_string().ok())
            .is_some_and(|page| page.contains("– joyscroller</title>"))
    })
    .await
    .unwrap_or(false)
}

/// In a macOS bundle, run the app as a child process whose output goes to
/// `joyscroller.log` (a bundle's own output goes nowhere anyone can read) and
/// return the exit code for this process to exit with. Elsewhere, or in that
/// child, returns `None` and the app runs here.
///
/// The app itself (`detach`) is left running on its own and this process
/// exits at once. While a bundle's first process runs, macOS counts the app
/// as open, and opening it again only re-activates it, which with no window
/// does nothing. Exiting means every double-click starts a new copy, which
/// finds the running one and opens its UI. A subcommand is waited for, so its
/// output is in the log when it returns.
pub fn run_logged_if_bundled(detach: bool) -> Result<Option<i32>> {
    if bundle().is_none() || std::env::var_os(LOGGING_ENV).is_some() {
        return Ok(None);
    }
    let dir = bundled_data_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = open_log(&dir.join("joyscroller.log"))?;
    let exe = std::env::current_exe().context("finding the app's executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env(LOGGING_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().context("opening the log")?)
        .stderr(log);
    if !detach {
        let status = cmd.status().context("starting the app")?;
        return Ok(Some(status.code().unwrap_or(1)));
    }
    // Its own process group, so nothing aimed at this one's (a terminal's
    // Ctrl-C, when started from one) reaches it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().context("starting the app")?;
    Ok(Some(0))
}

/// The log, appended to, after moving a log that's grown too big aside.
fn open_log(path: &Path) -> Result<File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_LIMIT_BYTES) {
        let old = path.with_extension("old.log");
        std::fs::rename(path, &old).with_context(|| format!("moving {} aside", path.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_app_bundle() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/Joyscroller.app/Contents/MacOS/joyscroller"
            )),
            Some(PathBuf::from("/Applications/Joyscroller.app"))
        );
    }

    #[test]
    fn a_plain_folder_is_not_a_bundle() {
        assert_eq!(
            bundle_of(Path::new("/home/me/joyscroller/joyscroller")),
            None
        );
        assert_eq!(
            bundle_of(Path::new("/Users/me/Contents/MacOS/joyscroller")),
            None
        );
        assert_eq!(bundle_of(Path::new("joyscroller")), None);
    }

    #[test]
    fn moves_a_big_log_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("joyscroller.log");
        std::fs::write(
            &path,
            vec![b'x'; usize::try_from(LOG_LIMIT_BYTES).unwrap() + 1],
        )
        .unwrap();
        drop(open_log(&path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(dir.path().join("joyscroller.old.log").is_file());
    }

    #[test]
    fn keeps_appending_to_a_small_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("joyscroller.log");
        std::fs::write(&path, "earlier\n").unwrap();
        drop(open_log(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "earlier\n");
    }
}
