//! The programs joyscroller runs: yt-dlp (lists YouTube likes and downloads
//! videos), ffmpeg and ffprobe (mux and probe media), and bun, the JavaScript
//! runtime yt-dlp needs to solve YouTube's signature and n-parameter challenges. The app fetches its own
//! copies into `tools/` next to its data, so users don't install anything.
//!
//! yt-dlp is always our own copy, kept up to date with `yt-dlp -U`: sites
//! change often and a stale yt-dlp is what usually breaks downloads, while
//! distro packages lag weeks behind. ffmpeg and bun don't go stale that way,
//! so copies already on `PATH` are used as they are, which spares most users
//! a ~150 MB download. Prebuilt copies exist for Linux, macOS and Windows on
//! `x86_64` and `aarch64` (`ASSETS`); anywhere else, everything comes from `PATH`.
//! Tool files carry the platform's executable suffix (`.exe` on Windows).
//!
//! Start them with `Running::spawn` or `output`, which put each in a
//! process group of its own and kill the whole group if it's dropped
//! before it finishes, so nothing a tool started outlives it.
//!
//! Chrome's headless shell is fetched only when it's needed: when the user's
//! browser can't run TikTok, Instagram and X headless (Firefox, Arc) and no
//! other installed browser can either (`headless_chrome`).

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use ureq::ResponseExt as _;

const YT_DLP_RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
/// yt-dlp's own ffmpeg builds, the ones its README recommends. Rebuilt daily
/// under the same `latest` tag and file names.
const FFMPEG_RELEASE: &str = "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest";
/// Bun rather than deno: Cloudflare bought Deno in October 2026 and will stop
/// developing the standalone runtime after a year of maintenance releases.
const BUN_RELEASE: &str = "https://github.com/oven-sh/bun/releases/latest/download";
/// ffmpeg release builds for macOS, which yt-dlp's builds don't cover.
const MARTIN_RIEDL: &str = "https://ffmpeg.martin-riedl.de";
/// Chrome for Testing's builds, in folders by version and platform.
const CHROME_FOR_TESTING: &str = "https://storage.googleapis.com/chrome-for-testing-public";
/// The Chrome headless shell we fetch. Google publishes no checksums for these
/// builds, so the version is pinned and each platform's zip has its SHA-256
/// recorded in `ASSETS`. To move to a newer one, take the Stable version from
/// <https://googlechromelabs.github.io/chrome-for-testing/>, run
/// `just chrome-hashes <version>`, and update both.
const CHROME_VERSION: &str = "155.0.8059.79";

/// What the programs in yt-dlp's Windows ffmpeg zips end in. Fixed rather than
/// the running platform's suffix so the matching can be tested anywhere.
const WINDOWS_ZIP_SUFFIX: &str = ".exe";

/// The release files to fetch on one platform.
struct Assets {
    os: &'static str,
    arch: &'static str,
    /// A standalone yt-dlp executable.
    yt_dlp: &'static str,
    ffmpeg: FfmpegBuild,
    /// A `.zip` with the `bun` executable in a folder named like the zip.
    bun: &'static str,
    /// Chrome's headless shell, where Chrome for Testing builds it.
    chrome: Option<ChromeBuild>,
}

/// One platform's Chrome for Testing headless shell: `platform` is its folder
/// and name there (`chrome-headless-shell-<platform>.zip`, which holds a
/// folder of the same name), and `sha256` the zip's at `CHROME_VERSION`.
struct ChromeBuild {
    platform: &'static str,
    sha256: &'static str,
}

/// Where a platform's ffmpeg and ffprobe come from.
enum FfmpegBuild {
    /// yt-dlp's builds as a `.tar.xz` with `bin/ffmpeg` and `bin/ffprobe`.
    TarXz(&'static str),
    /// yt-dlp's builds as a `.zip` with `bin/ffmpeg.exe` and `bin/ffprobe.exe`.
    Zip(&'static str),
    /// martin-riedl.de's builds: one `.zip` per program, each holding just
    /// that program. The value is the platform's folder on the site, such as
    /// `macos/arm64`.
    MartinRiedl(&'static str),
}

const ASSETS: &[Assets] = &[
    Assets {
        os: "linux",
        arch: "x86_64",
        yt_dlp: "yt-dlp_linux",
        ffmpeg: FfmpegBuild::TarXz("ffmpeg-master-latest-linux64-gpl.tar.xz"),
        // The default x86_64 build needs AVX2; baseline runs on any x86_64 CPU,
        // and solving YouTube's challenges is too little work to tell them apart.
        bun: "bun-linux-x64-baseline.zip",
        chrome: Some(ChromeBuild {
            platform: "linux64",
            sha256: "4cea4c32b501aa43cc2b81eb45734af991a6a82feded48380d6dfaf96642a1c1",
        }),
    },
    Assets {
        os: "linux",
        arch: "aarch64",
        yt_dlp: "yt-dlp_linux_aarch64",
        ffmpeg: FfmpegBuild::TarXz("ffmpeg-master-latest-linuxarm64-gpl.tar.xz"),
        bun: "bun-linux-aarch64.zip",
        chrome: Some(ChromeBuild {
            platform: "linux-arm64",
            sha256: "72d938cabad9674610939c72ef2d3faf20214a4418c3b4e94c1cebca683e767b",
        }),
    },
    // The same universal yt-dlp binary runs on both Macs.
    Assets {
        os: "macos",
        arch: "aarch64",
        yt_dlp: "yt-dlp_macos",
        ffmpeg: FfmpegBuild::MartinRiedl("macos/arm64"),
        bun: "bun-darwin-aarch64.zip",
        chrome: Some(ChromeBuild {
            platform: "mac-arm64",
            sha256: "5f51504ead3d41815111cd73e79c4979df3a4e6aedc7cf6fe2b042ae037363b5",
        }),
    },
    Assets {
        os: "macos",
        arch: "x86_64",
        yt_dlp: "yt-dlp_macos",
        ffmpeg: FfmpegBuild::MartinRiedl("macos/amd64"),
        // Baseline for the same reason as on Linux.
        bun: "bun-darwin-x64-baseline.zip",
        chrome: Some(ChromeBuild {
            platform: "mac-x64",
            sha256: "36a1da728fc936770a0edc52989b74d8d3a194e8747f490e372a210853ad831c",
        }),
    },
    Assets {
        os: "windows",
        arch: "x86_64",
        yt_dlp: "yt-dlp.exe",
        ffmpeg: FfmpegBuild::Zip("ffmpeg-master-latest-win64-gpl.zip"),
        bun: "bun-windows-x64-baseline.zip",
        chrome: Some(ChromeBuild {
            platform: "win64",
            sha256: "c4cc49fa7908cf018600b04c95918d336565d90f9c72241fd83dacd9ec41855c",
        }),
    },
    Assets {
        os: "windows",
        arch: "aarch64",
        yt_dlp: "yt-dlp_arm64.exe",
        ffmpeg: FfmpegBuild::Zip("ffmpeg-master-latest-winarm64-gpl.zip"),
        bun: "bun-windows-aarch64.zip",
        // Chrome for Testing has no Windows on Arm build.
        chrome: None,
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

/// `name` as a file name on this platform.
fn exe(name: &str) -> String {
    exe_name(name, std::env::consts::EXE_SUFFIX)
}

/// `name` with `suffix` appended, unless it already has an extension.
fn exe_name(name: &str, suffix: &str) -> String {
    if Path::new(name).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}{suffix}")
    }
}

/// The first of `names` found in a `PATH` directory. Windows only runs files
/// ending in `.exe`, so a name without an extension is tried with one too.
pub fn on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| {
            names
                .iter()
                .flat_map(|name| [exe(name), (*name).to_owned()])
                .map(move |file| dir.join(file))
        })
        .find(|candidate| candidate.is_file())
}

/// Our own copy of `name` if we have one, or else the bare name, for `PATH`.
fn program(name: &str) -> Result<PathBuf> {
    let own = dir()?.join(exe(name));
    Ok(if own.is_file() { own } else { name.into() })
}

/// A yt-dlp command, pointed at our own ffmpeg and bun when we have them.
pub fn yt_dlp() -> Result<Command> {
    let dir = dir()?;
    let mut cmd = Command::new(program("yt-dlp")?);
    if dir.join(exe("ffmpeg")).is_file() && dir.join(exe("ffprobe")).is_file() {
        cmd.arg("--ffmpeg-location").arg(&dir);
    }
    // yt-dlp only turns on deno by itself; bun has to be asked for, even
    // from `PATH`.
    let bun = dir.join(exe("bun"));
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
/// Windows has no `nice`, so the process starts in the below-normal priority
/// class instead.
pub fn ffmpeg_in_background() -> Result<Command> {
    let ffmpeg = program("ffmpeg")?;
    #[cfg(windows)]
    {
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let mut cmd = Command::new(ffmpeg);
        cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
        Ok(cmd)
    }
    #[cfg(not(windows))]
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

/// A tool that's running, in a process group of its own with whatever it
/// starts in turn. Dropped before it has finished (a listing stopped early,
/// a task dropped as the app quits), the whole group is killed. Killing the
/// tool alone, as `kill_on_drop` does, isn't enough: yt-dlp's standalone
/// build is a launcher that runs the real yt-dlp as its child, which ran on
/// for most of a minute after every listing stopped early. Its stdin is
/// empty, since a process outside the terminal's foreground group that
/// reads the terminal is stopped.
pub struct Running(Child);

impl Running {
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Running> {
        #[cfg(unix)]
        cmd.process_group(0);
        Ok(Running(
            cmd.stdin(Stdio::null()).kill_on_drop(true).spawn()?,
        ))
    }
}

impl std::ops::Deref for Running {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Running {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Only while the tool is still running: once it has exited and been
        // waited for, its id (the group's) may be some other process's.
        #[cfg(unix)]
        if matches!(self.0.try_wait(), Ok(None))
            && let Some(id) = self.0.id()
            && let Some(pid) = i32::try_from(id)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            && let Err(e) = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)
        {
            eprintln!("warning: couldn't stop process group {id}: {e}");
        }
    }
}

/// Run `cmd` to the end and collect what it printed, as `Command::output`
/// does, in a process group of its own (see `Running`).
pub async fn output(cmd: &mut Command) -> std::io::Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut running = Running::spawn(cmd)?;
    let stdout = running.stdout.take();
    let stderr = running.stderr.take();
    let (stdout, stderr, status) =
        tokio::try_join!(read_all(stdout), read_all(stderr), running.wait())?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
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
        .all(|name| dir.join(exe(name)).is_file() || on_path(&[name]).is_some()))
}

/// Fetch yt-dlp if we don't have our own copy yet. Syncing YouTube needs it.
pub async fn ensure_yt_dlp() -> Result<()> {
    let Some(assets) = assets() else {
        return Ok(());
    };
    let dir = dir()?;
    if dir.join(exe("yt-dlp")).is_file() {
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
    let old_deno = dir.join(exe("deno"));
    match std::fs::remove_file(&old_deno) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", old_deno.display()));
        }
        _ => {}
    }
    let Some(assets) = assets() else {
        return Ok(());
    };
    let have = |name: &str| dir.join(exe(name)).is_file() || on_path(&[name]).is_some();
    if !(have("ffmpeg") && have("ffprobe")) {
        eprintln!("tools: downloading ffmpeg (about 150 MB)");
        let dir = dir.clone();
        blocking(move || fetch_ffmpeg(&dir, &assets.ffmpeg)).await?;
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

/// Chrome's headless shell, fetched the first time it's asked for: it runs
/// TikTok, Instagram and X when no installed browser can
/// (`Browser::installed_headless`). About 110-125 MB.
pub async fn headless_chrome() -> Result<PathBuf> {
    // TikTok's sync and a photo post's download can ask at once; one fetches.
    static FETCHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Some(build) = assets().and_then(|assets| assets.chrome.as_ref()) else {
        bail!("there's no Chrome headless shell for this computer; install Chrome");
    };
    let dir = dir()?;
    let program = chrome_folder(&dir).join(chrome_in_zip(build, std::env::consts::EXE_SUFFIX));
    let _one_at_a_time = FETCHING.lock().await;
    if !program.is_file() {
        eprintln!("tools: downloading Chrome's headless shell (about 125 MB)");
        blocking(move || fetch_chrome(&dir, build)).await?;
        eprintln!("tools: Chrome's headless shell ready");
    }
    Ok(program)
}

/// Where the headless shell at `CHROME_VERSION` is unpacked.
fn chrome_folder(dir: &Path) -> PathBuf {
    dir.join(format!("chrome-headless-shell-{CHROME_VERSION}"))
}

/// The program in a headless shell zip, which holds everything in a folder
/// named like the zip.
fn chrome_in_zip(build: &ChromeBuild, exe_suffix: &str) -> String {
    format!(
        "chrome-headless-shell-{}/chrome-headless-shell{exe_suffix}",
        build.platform
    )
}

/// Download the headless shell, unpack it beside its final folder, then move
/// it into place in one rename, so nothing ever runs a half-unpacked copy.
/// Copies of other versions are deleted after.
fn fetch_chrome(dir: &Path, build: &ChromeBuild) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = format!("chrome-headless-shell-{}.zip", build.platform);
    let url = format!(
        "{CHROME_FOR_TESTING}/{CHROME_VERSION}/{}/{name}",
        build.platform
    );
    let mut archive = tempfile::tempfile_in(dir)?;
    download_verified(
        &url,
        &format!("{}  {name}", build.sha256),
        &name,
        &mut archive,
    )?;
    let unpacking = tempfile::Builder::new()
        .prefix(".chrome-unpacking-")
        .tempdir_in(dir)?;
    zip::ZipArchive::new(archive)
        .and_then(|mut zip| zip.extract(unpacking.path()))
        .context("unpacking Chrome's headless shell")?;
    let folder = chrome_folder(dir);
    // A folder already there is missing its program (we wouldn't be fetching
    // otherwise), and a folder can't be renamed onto one that isn't empty.
    if folder.exists() {
        std::fs::remove_dir_all(&folder)
            .with_context(|| format!("removing the broken {}", folder.display()))?;
    }
    std::fs::rename(unpacking.path(), &folder)
        .with_context(|| format!("installing {}", folder.display()))?;
    // Other versions, and what an unpacking cut short by a crash left behind.
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let old = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                (name.starts_with("chrome-headless-shell-")
                    || name.starts_with(".chrome-unpacking-"))
                    && path != folder
            });
        if old && let Err(e) = std::fs::remove_dir_all(&path) {
            eprintln!("warning: couldn't remove {}: {e}", path.display());
        }
    }
    Ok(())
}

/// Update our own yt-dlp to its latest release. A yt-dlp from `PATH` is left
/// to whoever installed it.
pub async fn update_yt_dlp() -> Result<()> {
    let own = dir()?.join(exe("yt-dlp"));
    if !own.is_file() {
        return Ok(());
    }
    // yt-dlp swaps itself by renaming files, not by overwriting the running
    // one: on Linux and macOS a yt-dlp already running (a download in
    // progress) keeps its old copy, and on Windows, which can't overwrite a
    // running .exe, it moves the running exe aside first. Either way
    // `--update` is safe while downloads run.
    let mut cmd = Command::new(&own);
    cmd.arg("--update");
    let out = output(&mut cmd).await.context("running yt-dlp --update")?;
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
    install(out, &dir.join(exe("yt-dlp")))
}

fn fetch_ffmpeg(dir: &Path, build: &FfmpegBuild) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    match build {
        FfmpegBuild::TarXz(asset) => fetch_ffmpeg_tar(dir, asset),
        FfmpegBuild::Zip(asset) => fetch_ffmpeg_zip(dir, asset),
        FfmpegBuild::MartinRiedl(platform) => {
            fetch_martin_riedl(dir, platform, "ffprobe")?;
            fetch_martin_riedl(dir, platform, "ffmpeg")
        }
    }
}

/// Download one of yt-dlp's ffmpeg builds into an unnamed temporary file, so
/// it's gone however this ends.
fn download_ffmpeg_build(dir: &Path, asset: &str) -> Result<File> {
    let sums = fetch_text(&format!("{FFMPEG_RELEASE}/checksums.sha256"))?;
    let mut archive = tempfile::tempfile_in(dir)?;
    download_verified(
        &format!("{FFMPEG_RELEASE}/{asset}"),
        &sums,
        asset,
        &mut archive,
    )?;
    Ok(archive)
}

fn fetch_ffmpeg_tar(dir: &Path, asset: &str) -> Result<()> {
    let mut archive = download_ffmpeg_build(dir, asset)?;
    archive.rewind()?;
    let xz = liblzma::read::XzDecoder::new(BufReader::new(archive));
    let mut tar = tar::Archive::new(xz);
    let (mut ffmpeg, mut ffprobe) = (None, None);
    for entry in tar.entries().context("reading the ffmpeg archive")? {
        let mut entry = entry.context("reading the ffmpeg archive")?;
        let path = entry.path()?.into_owned();
        let slot = match ffmpeg_tool(&path, "") {
            Some(FfmpegTool::Ffmpeg) => &mut ffmpeg,
            Some(FfmpegTool::Ffprobe) => &mut ffprobe,
            None => continue,
        };
        let mut out = NamedTempFile::new_in(dir)?;
        std::io::copy(&mut entry, &mut out).context("unpacking ffmpeg")?;
        *slot = Some(out);
    }
    install_ffmpeg(dir, ffmpeg, ffprobe, asset)
}

fn fetch_ffmpeg_zip(dir: &Path, asset: &str) -> Result<()> {
    let archive = download_ffmpeg_build(dir, asset)?;
    let mut zip = zip::ZipArchive::new(archive).context("reading the ffmpeg archive")?;
    let (mut ffmpeg, mut ffprobe) = (None, None);
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("reading the ffmpeg archive")?;
        let name = entry
            .name()
            .context("reading the ffmpeg archive")?
            .into_owned();
        let slot = match ffmpeg_tool(Path::new(&name), WINDOWS_ZIP_SUFFIX) {
            Some(FfmpegTool::Ffmpeg) => &mut ffmpeg,
            Some(FfmpegTool::Ffprobe) => &mut ffprobe,
            None => continue,
        };
        let mut out = NamedTempFile::new_in(dir)?;
        std::io::copy(&mut entry, &mut out).context("unpacking ffmpeg")?;
        *slot = Some(out);
    }
    install_ffmpeg(dir, ffmpeg, ffprobe, asset)
}

fn install_ffmpeg(
    dir: &Path,
    ffmpeg: Option<NamedTempFile>,
    ffprobe: Option<NamedTempFile>,
    asset: &str,
) -> Result<()> {
    let (Some(ffmpeg), Some(ffprobe)) = (ffmpeg, ffprobe) else {
        bail!("{asset} has no bin/ffmpeg and bin/ffprobe");
    };
    install(ffprobe, &dir.join(exe("ffprobe")))?;
    install(ffmpeg, &dir.join(exe("ffmpeg")))
}

/// Fetch `tool` (`ffmpeg` or `ffprobe`) from martin-riedl.de. Its `latest`
/// address redirects to a build's folder, and only the build's own address has
/// a `.sha256` beside it, so follow the redirect first and take the zip and
/// its checksum from that one folder, which keeps them matching even if a
/// new build lands in between.
fn fetch_martin_riedl(dir: &Path, platform: &str, tool: &str) -> Result<()> {
    let latest = format!("{MARTIN_RIEDL}/redirect/latest/{platform}/release/{tool}.zip");
    let url = resolve_redirect(&latest)?;
    let name = format!("{tool}.zip");
    let sums = fetch_text(&format!("{url}.sha256"))?;
    let mut archive = tempfile::tempfile_in(dir)?;
    download_verified(&url, &sums, &name, &mut archive)?;
    let out = unpack_zip_entry(archive, tool, dir, tool)?;
    install(out, &dir.join(exe(tool)))
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
    let out = unpack_zip_entry(
        archive,
        &bun_in_zip(asset, std::env::consts::EXE_SUFFIX),
        dir,
        "bun",
    )?;
    install(out, &dir.join(exe("bun")))
}

/// Copy the entry `inside` a zip into a temporary file in `dir`.
fn unpack_zip_entry(archive: File, inside: &str, dir: &Path, what: &str) -> Result<NamedTempFile> {
    let mut zip =
        zip::ZipArchive::new(archive).with_context(|| format!("reading the {what} archive"))?;
    let mut entry = zip
        .by_name(inside)
        .with_context(|| format!("the {what} archive has no {inside}"))?;
    let mut out = NamedTempFile::new_in(dir)?;
    std::io::copy(&mut entry, &mut out).with_context(|| format!("unpacking {what}"))?;
    Ok(out)
}

/// Where the executable sits in bun's zip: `bun-linux-x64.zip` holds
/// `bun-linux-x64/bun`, and the Windows zips `bun-windows-x64/bun.exe`.
fn bun_in_zip(asset: &str, exe_suffix: &str) -> String {
    format!(
        "{}/bun{exe_suffix}",
        asset.strip_suffix(".zip").unwrap_or(asset)
    )
}

enum FfmpegTool {
    Ffmpeg,
    Ffprobe,
}

/// Which program an archive entry is, if it's `bin/ffmpeg` or `bin/ffprobe`
/// (with `exe_suffix` after the name). The builds keep them in a `bin` folder
/// next to docs and other files we don't want.
fn ffmpeg_tool(path: &Path, exe_suffix: &str) -> Option<FfmpegTool> {
    let in_bin = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|d| d == "bin");
    if !in_bin {
        return None;
    }
    match path.file_name()?.to_str()?.strip_suffix(exe_suffix)? {
        "ffmpeg" => Some(FfmpegTool::Ffmpeg),
        "ffprobe" => Some(FfmpegTool::Ffprobe),
        _ => None,
    }
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

/// Where `url` ends up after its redirects.
fn resolve_redirect(url: &str) -> Result<String> {
    let response = agent()
        .get(url)
        .call()
        .with_context(|| format!("fetching {url}"))?;
    // The body is never read; dropping the response closes it.
    Ok(response.get_uri().to_string())
}

/// What each of our tools prints with its version flag, one line per tool
/// with the path that ran, for checking that a fetched set works.
pub async fn versions() -> Result<String> {
    let mut lines = String::new();
    for (name, flag) in [
        ("yt-dlp", "--version"),
        ("ffmpeg", "-version"),
        ("ffprobe", "-version"),
        ("bun", "--version"),
    ] {
        let program = program(name)?;
        let path = if program.is_file() {
            program.clone()
        } else {
            on_path(&[name]).unwrap_or_else(|| program.clone())
        };
        let mut cmd = Command::new(&program);
        cmd.arg(flag);
        let out = output(&mut cmd)
            .await
            .with_context(|| format!("running {} {flag}", path.display()))?;
        if !out.status.success() {
            bail!(
                "{} {flag} failed: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first = stdout.lines().next().unwrap_or_default().trim();
        writeln!(lines, "{name}: {first} ({})", path.display())?;
    }
    if let Some(build) = assets().and_then(|assets| assets.chrome.as_ref()) {
        let program =
            chrome_folder(&dir()?).join(chrome_in_zip(build, std::env::consts::EXE_SUFFIX));
        if program.is_file() {
            let mut cmd = Command::new(&program);
            cmd.arg("--version");
            let out = output(&mut cmd)
                .await
                .with_context(|| format!("running {} --version", program.display()))?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            writeln!(
                lines,
                "chrome-headless-shell: {} ({})",
                stdout.trim(),
                program.display()
            )?;
        }
    }
    Ok(lines)
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

    /// A shell that starts a `sleep` and waits for it, as yt-dlp's launcher
    /// runs the real yt-dlp, and prints the sleep's pid.
    #[cfg(unix)]
    fn launcher() -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped());
        cmd
    }

    #[cfg(unix)]
    async fn started_pid(running: &mut Running) -> rustix::process::Pid {
        use tokio::io::AsyncBufReadExt;
        let stdout = running.stdout.take().unwrap();
        let line = tokio::io::BufReader::new(stdout)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        rustix::process::Pid::from_raw(line.trim().parse().unwrap()).unwrap()
    }

    #[cfg(unix)]
    async fn gone(pid: rustix::process::Pid) -> bool {
        // Killed, it's reaped by init a moment later.
        for _ in 0..50 {
            if rustix::process::test_kill_process(pid).is_err() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_tool_stops_what_it_started() {
        let mut running = Running::spawn(&mut launcher()).unwrap();
        let sleep = started_pid(&mut running).await;
        drop(running);
        assert!(gone(sleep).await, "the launcher's child outlived it");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_collects_what_a_tool_prints() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = output(&mut cmd).await.unwrap();
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.status.code(), Some(3));
    }

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
            bun_in_zip("bun-linux-x64-baseline.zip", ""),
            "bun-linux-x64-baseline/bun"
        );
        assert_eq!(
            bun_in_zip("bun-darwin-aarch64.zip", ""),
            "bun-darwin-aarch64/bun"
        );
        assert_eq!(
            bun_in_zip("bun-windows-x64-baseline.zip", ".exe"),
            "bun-windows-x64-baseline/bun.exe"
        );
    }

    #[test]
    fn has_assets_for_every_supported_platform() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
        ] {
            assert!(assets_for(os, arch).is_some(), "{os} {arch}");
        }
        assert!(assets_for("freebsd", "x86_64").is_none());
        assert!(assets_for("linux", "riscv64").is_none());
    }

    #[test]
    fn picks_ffmpeg_out_of_the_windows_zip() {
        let tool = |path: &str| ffmpeg_tool(Path::new(path), WINDOWS_ZIP_SUFFIX);
        let dir = "ffmpeg-master-latest-win64-gpl";
        assert!(matches!(
            tool(&format!("{dir}/bin/ffmpeg.exe")),
            Some(FfmpegTool::Ffmpeg)
        ));
        assert!(matches!(
            tool(&format!("{dir}/bin/ffprobe.exe")),
            Some(FfmpegTool::Ffprobe)
        ));
        assert!(tool(&format!("{dir}/bin/ffplay.exe")).is_none());
        assert!(tool(&format!("{dir}/bin/")).is_none());
        assert!(tool(&format!("{dir}/ffmpeg.exe")).is_none());
        assert!(tool(&format!("{dir}/doc/ffmpeg.exe")).is_none());
        assert!(tool(&format!("{dir}/bin/ffmpeg")).is_none());
    }

    #[test]
    fn picks_ffmpeg_out_of_the_tarball() {
        let tool = |path: &str| ffmpeg_tool(Path::new(path), "");
        let dir = "ffmpeg-master-latest-linux64-gpl";
        assert!(matches!(
            tool(&format!("{dir}/bin/ffmpeg")),
            Some(FfmpegTool::Ffmpeg)
        ));
        assert!(matches!(
            tool(&format!("{dir}/bin/ffprobe")),
            Some(FfmpegTool::Ffprobe)
        ));
        assert!(tool(&format!("{dir}/bin/ffplay")).is_none());
        assert!(tool(&format!("{dir}/LICENSE.txt")).is_none());
    }

    #[test]
    fn finds_chrome_inside_its_zip() {
        let build = |platform| ChromeBuild {
            platform,
            sha256: "",
        };
        assert_eq!(
            chrome_in_zip(&build("linux64"), ""),
            "chrome-headless-shell-linux64/chrome-headless-shell"
        );
        assert_eq!(
            chrome_in_zip(&build("win64"), ".exe"),
            "chrome-headless-shell-win64/chrome-headless-shell.exe"
        );
    }

    #[test]
    fn names_executables_by_platform() {
        assert_eq!(exe_name("yt-dlp", ".exe"), "yt-dlp.exe");
        assert_eq!(exe_name("yt-dlp", ""), "yt-dlp");
        assert_eq!(exe_name("bun.exe", ".exe"), "bun.exe");
        assert_eq!(exe_name("tool.sh", ".exe"), "tool.sh");
        assert_eq!(
            exe("ffmpeg"),
            format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)
        );
    }
}
