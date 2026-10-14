//! The programs joyscroller runs: yt-dlp (lists YouTube likes and downloads
//! videos), ffmpeg and ffprobe (mux and probe media), and bun, the JavaScript
//! runtime yt-dlp needs to solve YouTube's signature and n-parameter challenges. The app fetches its own
//! copies into `tools/` next to its data, so users don't install anything.
//!
//! yt-dlp is always our own copy, kept up to date with `yt-dlp -U`: sites
//! change often and a stale yt-dlp is what usually breaks downloads, while
//! distro packages lag weeks behind. ffmpeg and bun don't go stale that way,
//! so copies already on `PATH` are used as they are, which spares most users
//! a ~150 MB download. Where no prebuilt copies exist (`ASSETS` has no entry
//! for the platform), everything comes from `PATH`.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::process::Command;

const YT_DLP_RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
/// yt-dlp's own ffmpeg builds, the ones its README recommends. Rebuilt daily
/// under the same `latest` tag and file names.
const FFMPEG_RELEASE: &str = "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest";
/// Bun rather than deno: Cloudflare bought Deno in October 2026 and will stop
/// developing the standalone runtime after a year of maintenance releases.
const BUN_RELEASE: &str = "https://github.com/oven-sh/bun/releases/latest/download";

/// The release files to fetch on one platform.
struct Assets {
    os: &'static str,
    arch: &'static str,
    /// A standalone yt-dlp executable.
    yt_dlp: &'static str,
    /// A `.tar.xz` with `bin/ffmpeg` and `bin/ffprobe` inside.
    ffmpeg: &'static str,
    /// A `.zip` with the `bun` executable in a folder named like the zip.
    bun: &'static str,
}

const ASSETS: &[Assets] = &[
    Assets {
        os: "linux",
        arch: "x86_64",
        yt_dlp: "yt-dlp_linux",
        ffmpeg: "ffmpeg-master-latest-linux64-gpl.tar.xz",
        // The default x86_64 build needs AVX2; baseline runs on any x86_64 CPU,
        // and solving YouTube's challenges is too little work to tell them apart.
        bun: "bun-linux-x64-baseline.zip",
    },
    Assets {
        os: "linux",
        arch: "aarch64",
        yt_dlp: "yt-dlp_linux_aarch64",
        ffmpeg: "ffmpeg-master-latest-linuxarm64-gpl.tar.xz",
        bun: "bun-linux-aarch64.zip",
    },
];

fn assets() -> Option<&'static Assets> {
    assets_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn assets_for(os: &str, arch: &str) -> Option<&'static Assets> {
    ASSETS.iter().find(|a| a.os == os && a.arch == arch)
}

/// Where our own copies live: `tools/` beside the database and downloads.
fn dir() -> Result<PathBuf> {
    Ok(crate::app_dir()?.join("tools"))
}

/// The first of `names` found in a `PATH` directory.
pub fn on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Our own copy of `name` if we have one, or else the bare name, for `PATH`.
fn program(name: &str) -> Result<PathBuf> {
    let own = dir()?.join(name);
    Ok(if own.is_file() { own } else { name.into() })
}

/// A yt-dlp command, pointed at our own ffmpeg and bun when we have them.
pub fn yt_dlp() -> Result<Command> {
    let dir = dir()?;
    let mut cmd = Command::new(program("yt-dlp")?);
    if dir.join("ffmpeg").is_file() && dir.join("ffprobe").is_file() {
        cmd.arg("--ffmpeg-location").arg(&dir);
    }
    // yt-dlp only turns on deno by itself; bun has to be asked for, even
    // from `PATH`.
    let bun = dir.join("bun");
    if bun.is_file() {
        cmd.arg("--js-runtimes")
            .arg([OsStr::new("bun:"), bun.as_os_str()].join(OsStr::new("")));
    } else if on_path(&["bun"]).is_some() {
        cmd.arg("--js-runtimes").arg("bun");
    }
    Ok(cmd)
}

pub fn ffmpeg() -> Result<Command> {
    Ok(Command::new(program("ffmpeg")?))
}

/// An ffmpeg command at low CPU priority, for long work in the background
/// (making playback copies) that mustn't make the computer sluggish. `nice`
/// comes with every Linux and macOS; without it, ffmpeg runs as usual.
pub fn ffmpeg_in_background() -> Result<Command> {
    let ffmpeg = program("ffmpeg")?;
    Ok(match on_path(&["nice"]) {
        Some(nice) => {
            let mut cmd = Command::new(nice);
            cmd.arg("-n").arg("10").arg(ffmpeg);
            cmd
        }
        None => Command::new(ffmpeg),
    })
}

pub fn ffprobe() -> Result<Command> {
    Ok(Command::new(program("ffprobe")?))
}

/// Whether everything downloads need is here, our own copy or on `PATH`.
pub fn ready() -> Result<bool> {
    have_all(&["yt-dlp", "ffmpeg", "ffprobe", "bun"])
}

/// Whether ffmpeg and ffprobe are here, all that playback copies need.
pub fn ffmpeg_ready() -> Result<bool> {
    have_all(&["ffmpeg", "ffprobe"])
}

fn have_all(names: &[&str]) -> Result<bool> {
    let dir = dir()?;
    Ok(names
        .iter()
        .all(|name| dir.join(name).is_file() || on_path(&[name]).is_some()))
}

/// Fetch yt-dlp if we don't have our own copy yet. Syncing YouTube needs it.
pub async fn ensure_yt_dlp() -> Result<()> {
    let Some(assets) = assets() else {
        return Ok(());
    };
    let dir = dir()?;
    if dir.join("yt-dlp").is_file() {
        return Ok(());
    }
    eprintln!("tools: downloading yt-dlp");
    blocking(move || fetch_yt_dlp(&dir, assets.yt_dlp)).await?;
    eprintln!("tools: yt-dlp ready");
    Ok(())
}

/// Fetch ffmpeg, ffprobe and bun, unless they're here or on `PATH`.
/// Downloading videos needs them; syncing doesn't.
pub async fn ensure_helpers() -> Result<()> {
    let dir = dir()?;
    // Older versions fetched deno; nothing runs it now.
    let old_deno = dir.join("deno");
    match std::fs::remove_file(&old_deno) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", old_deno.display()));
        }
        _ => {}
    }
    let Some(assets) = assets() else {
        return Ok(());
    };
    let have = |name: &str| dir.join(name).is_file() || on_path(&[name]).is_some();
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("tools: downloading ffmpeg (about 150 MB)");
        let dir = dir.clone();
        blocking(move || fetch_ffmpeg(&dir, assets.ffmpeg)).await?;
        eprintln!("tools: ffmpeg ready");
    }
    if !have("bun") {
        eprintln!("tools: downloading bun");
        let dir = dir.clone();
        blocking(move || fetch_bun(&dir, assets.bun)).await?;
        eprintln!("tools: bun ready");
    }
    Ok(())
}

/// Update our own yt-dlp to its latest release. A yt-dlp from `PATH` is left
/// to whoever installed it.
pub async fn update_yt_dlp() -> Result<()> {
    let own = dir()?.join("yt-dlp");
    if !own.is_file() {
        return Ok(());
    }
    // Replacing the file is a rename, so a yt-dlp already running (a download
    // in progress) keeps its old copy and finishes normally.
    let out = Command::new(&own)
        .arg("--update")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running yt-dlp --update")?;
    if !out.status.success() {
        bail!(
            "yt-dlp --update failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // It prints "yt-dlp is up to date (...)" or "Updated yt-dlp to ...".
    let said = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = said.lines().rfind(|l| l.contains("Updated yt-dlp")) {
        eprintln!("tools: {}", line.trim());
    }
    Ok(())
}

async fn blocking(fetch: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    tokio::task::spawn_blocking(fetch)
        .await
        .context("tool download task panicked")?
}

fn fetch_yt_dlp(dir: &Path, asset: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let sums = fetch_text(&format!("{YT_DLP_RELEASE}/SHA2-256SUMS"))?;
    let mut out = NamedTempFile::new_in(dir)?;
    download_verified(
        &format!("{YT_DLP_RELEASE}/{asset}"),
        &sums,
        asset,
        out.as_file_mut(),
    )?;
    install(out, &dir.join("yt-dlp"))
}

fn fetch_ffmpeg(dir: &Path, asset: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let sums = fetch_text(&format!("{FFMPEG_RELEASE}/checksums.sha256"))?;
    // Unnamed, so it's gone however this ends.
    let mut archive = tempfile::tempfile_in(dir)?;
    download_verified(
        &format!("{FFMPEG_RELEASE}/{asset}"),
        &sums,
        asset,
        &mut archive,
    )?;
    archive.rewind()?;
    let xz = liblzma::read::XzDecoder::new(BufReader::new(archive));
    let mut tar = tar::Archive::new(xz);
    let (mut ffmpeg, mut ffprobe) = (None, None);
    for entry in tar.entries().context("reading the ffmpeg archive")? {
        let mut entry = entry.context("reading the ffmpeg archive")?;
        let path = entry.path()?.into_owned();
        let in_bin = path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|d| d == "bin");
        let slot = match path.file_name().and_then(OsStr::to_str) {
            Some("ffmpeg") if in_bin => &mut ffmpeg,
            Some("ffprobe") if in_bin => &mut ffprobe,
            _ => continue,
        };
        let mut out = NamedTempFile::new_in(dir)?;
        std::io::copy(&mut entry, &mut out).context("unpacking ffmpeg")?;
        *slot = Some(out);
    }
    let (Some(ffmpeg), Some(ffprobe)) = (ffmpeg, ffprobe) else {
        bail!("{asset} has no bin/ffmpeg and bin/ffprobe");
    };
    install(ffprobe, &dir.join("ffprobe"))?;
    install(ffmpeg, &dir.join("ffmpeg"))
}

fn fetch_bun(dir: &Path, asset: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let sums = fetch_text(&format!("{BUN_RELEASE}/SHASUMS256.txt"))?;
    let mut archive = tempfile::tempfile_in(dir)?;
    download_verified(
        &format!("{BUN_RELEASE}/{asset}"),
        &sums,
        asset,
        &mut archive,
    )?;
    let inside = bun_in_zip(asset);
    let mut zip = zip::ZipArchive::new(archive).context("reading the bun archive")?;
    let mut entry = zip
        .by_name(&inside)
        .with_context(|| format!("the bun archive has no {inside}"))?;
    let mut out = NamedTempFile::new_in(dir)?;
    std::io::copy(&mut entry, &mut out).context("unpacking bun")?;
    install(out, &dir.join("bun"))
}

/// Where the executable sits in bun's zip: `bun-linux-x64.zip` holds
/// `bun-linux-x64/bun`.
fn bun_in_zip(asset: &str) -> String {
    format!("{}/bun", asset.strip_suffix(".zip").unwrap_or(asset))
}

/// Make `file` executable and move it to `dest` in one rename, so nothing
/// ever runs a half-written copy.
fn install(file: NamedTempFile, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    file.persist(dest)
        .with_context(|| format!("installing {}", dest.display()))?;
    Ok(())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // ffmpeg is ~150 MB; this only stops a download that has stalled.
        .timeout_recv_body(Some(Duration::from_mins(30)))
        .build()
        .into()
}

fn fetch_text(url: &str) -> Result<String> {
    agent()
        .get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

/// Download `url` into `out`, checking it against `name`'s line in `sums`.
fn download_verified(url: &str, sums: &str, name: &str, out: &mut File) -> Result<()> {
    let want = checksum(sums, name).with_context(|| format!("no checksum listed for {name}"))?;
    let mut response = agent()
        .get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    let mut body = response.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = body
            .read(&mut buf)
            .with_context(|| format!("downloading {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    let got = hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    if got != want {
        bail!("{name} doesn't match its checksum (got {got}, want {want})");
    }
    Ok(())
}

/// `name`'s SHA-256 from a `sha256sum`-style list ("<hex>  <name>" lines;
/// a `*` before the name marks binary mode).
fn checksum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name).then(|| hex.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_checksum_by_exact_name() {
        let sums = "aaa  yt-dlp_linux.zip\nbbb  yt-dlp_linux\nccc *yt-dlp_macos\n";
        assert_eq!(checksum(sums, "yt-dlp_linux").as_deref(), Some("bbb"));
        assert_eq!(checksum(sums, "yt-dlp_macos").as_deref(), Some("ccc"));
        assert_eq!(checksum(sums, "yt-dlp"), None);
    }

    #[test]
    fn finds_bun_inside_its_zip() {
        assert_eq!(
            bun_in_zip("bun-linux-x64-baseline.zip"),
            "bun-linux-x64-baseline/bun"
        );
        assert_eq!(bun_in_zip("bun-linux-aarch64.zip"), "bun-linux-aarch64/bun");
    }

    #[test]
    fn has_assets_for_linux_only() {
        assert!(assets_for("linux", "x86_64").is_some());
        assert!(assets_for("linux", "aarch64").is_some());
        assert!(assets_for("macos", "aarch64").is_none());
        assert!(assets_for("windows", "x86_64").is_none());
    }
}
