//! Downloads a liked video's media at the best quality on offer, into a
//! folder of its own under the media root, by platform and the day it was
//! liked: `<platform>/<YYYY-MM-DD>/<title> [<video id>]/`.
//! - Videos (YouTube, TikTok, Instagram), through yt-dlp: `video.<ext>` and
//!   `thumbnail.<ext>`.
//! - TikTok photo posts, which yt-dlp can't fetch, through `tiktok.rs`:
//!   `image-01.<ext>`, `image-02.<ext>`, … and `sound.<ext>`. The first image
//!   is the cover.
//! - Instagram photo posts and carousels, which yt-dlp can't fetch either,
//!   through `instagram.rs`: the same, except that a carousel's videos are
//!   `video-NN.<ext>`, numbered along with its pictures (with the first
//!   video's cover as `thumbnail.<ext>` when one comes first), and
//!   `sound.<ext>` is the part of the post's music it plays.
//!
//! The metadata goes back to the caller (for the database) rather than being
//! kept as a file. yt-dlp's progress is passed to a callback as it comes, for
//! the web UI's downloads page.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::browser::{Browser, Cookie};
use crate::db::PendingDownload;
use crate::filename::safe_name;
use crate::{instagram, tiktok};

/// For TikTok: the best video stream, and separately the best file that has
/// both picture and sound. TikTok's highest qualities are video-only, and the
/// audio-only format yt-dlp would pair them with is the post's *sound* (the
/// song as a standalone track), not the video's own audio, so the real audio
/// has to come from a combined file. yt-dlp can't merge two files that both
/// have video (it drops the second one silently), so `mux` does it instead.
const TIKTOK_FORMATS: &str = "bv*,b";

/// For YouTube and Instagram: yt-dlp's default, the best video and best audio,
/// merged. Instagram's best qualities come as DASH streams, the picture and
/// the video's own audio apart.
const MERGED_FORMATS: &str = "bv*+ba/b";

/// Keys dropped from yt-dlp's metadata before storing it:
/// - `cookies`: yt-dlp copies the request cookies (the user's login) into it.
/// - Format, thumbnail and caption lists, and signed stream URLs: most of its
///   size, and they expire. The files themselves are what we keep.
/// - The fields describing one downloaded stream. When TikTok's picture and
///   sound come from two files, they describe whichever came last, so they'd
///   be wrong for the saved video; the `format` column and the file say it right.
const INFO_DROP: &[&str] = &[
    "cookies",
    "formats",
    "requested_formats",
    "requested_downloads",
    "thumbnails",
    "thumbnail",
    "subtitles",
    "automatic_captions",
    "requested_subtitles",
    "http_headers",
    "url",
    "manifest_url",
    "fragments",
    "_format_sort_fields",
    "filepath",
    "filename",
    "_filename",
    "__files_to_move",
    "format",
    "format_id",
    "format_note",
    "ext",
    "video_ext",
    "audio_ext",
    "protocol",
    "width",
    "height",
    "resolution",
    "aspect_ratio",
    "fps",
    "dynamic_range",
    "vcodec",
    "acodec",
    "asr",
    "audio_channels",
    "tbr",
    "vbr",
    "abr",
    "filesize",
    "filesize_approx",
    "preference",
    "quality",
];

/// How much of a folder name the title may take, in bytes. Names can be 255;
/// this leaves room for the id, and for Windows' 260-character paths.
const TITLE_BYTES: usize = 100;

/// Marks yt-dlp's progress lines on stdout, apart from its `--print` lines.
const PROGRESS_MARK: &str = "joyscroller-progress";

/// Delete the hidden `.partial` and `.old` folders an interrupted download
/// leaves under `root` (a crash or Ctrl-C mid-download). Partials can be
/// gigabytes, and hold yt-dlp's raw metadata, cookies included. Only call this
/// when no download is running.
pub fn sweep_leftovers(root: &Path) -> Result<()> {
    let Ok(platforms) = std::fs::read_dir(root) else {
        // No media root yet, so nothing to sweep.
        return Ok(());
    };
    for platform in platforms {
        let platform = platform?.path();
        if !platform.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&platform)? {
            let path = entry?.path();
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            let leftover = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("partial" | "old")
            );
            if hidden && leftover {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// What a finished download saved.
#[derive(Debug)]
pub struct Saved {
    /// The video's folder, relative to the media root.
    pub dir: String,
    /// File names inside `dir`, sorted.
    pub files: Vec<String>,
    pub bytes: u64,
    /// The yt-dlp format ids used, e.g. `399+251`.
    pub format: String,
    /// yt-dlp's metadata, minus `INFO_DROP`.
    pub info_json: Option<String>,
    pub posted_at: Option<i64>,
}

impl Saved {
    /// The file to show as the video's picture: its thumbnail, or a photo
    /// post's first image. `db/videos.rs` picks the same file in SQL for the list.
    pub fn picture(&self) -> Option<&str> {
        let named = |prefix| {
            self.files
                .iter()
                .map(String::as_str)
                .find(|file| file.starts_with(prefix))
        };
        named("thumbnail.").or_else(|| named("image-"))
    }
}

/// How far yt-dlp is through the file it's downloading. TikTok videos come as
/// two files, one after the other, each reported from zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub downloaded: u64,
    /// Bytes in the file: exact, or yt-dlp's estimate for streams sent in
    /// fragments. None when it doesn't know.
    pub total: Option<f64>,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Seconds left.
    pub eta: Option<u64>,
    pub format_id: String,
}

/// One file yt-dlp reported writing.
struct Written {
    format_id: String,
    /// False only when yt-dlp knows there's no audio (acodec `none`).
    has_audio: bool,
    /// Picture height; 0 when unknown or audio-only.
    height: u64,
    path: PathBuf,
}

/// The user's login, in the two forms the downloads need: a cookies.txt for
/// yt-dlp, and the cookies themselves for a headless browser.
pub struct Login<'a> {
    pub browser: Browser,
    pub cookies: &'a [Cookie],
    pub cookies_txt: &'a Path,
}

/// A platform's login read from the browser, which a `Login` borrows. The
/// cookies.txt is a temp file, deleted when this is dropped.
pub struct Credentials {
    browser: Browser,
    cookies: Vec<Cookie>,
    cookies_txt: tempfile::NamedTempFile,
}

impl Credentials {
    pub async fn read(browser: Browser, profile: &str, platform: &str) -> Result<Self> {
        let domain = match platform {
            "youtube" => "youtube.com",
            "tiktok" => "tiktok.com",
            "instagram" => "instagram.com",
            other => bail!("can't download from platform {other}"),
        };
        let cookies = crate::browser::cookies_for(browser, profile, domain).await?;
        let cookies_txt = tempfile::Builder::new().suffix(".txt").tempfile()?;
        std::fs::write(cookies_txt.path(), crate::browser::to_netscape(&cookies))?;
        Ok(Credentials {
            browser,
            cookies,
            cookies_txt,
        })
    }

    pub fn login(&self) -> Login<'_> {
        Login {
            browser: self.browser,
            cookies: &self.cookies,
            cookies_txt: self.cookies_txt.path(),
        }
    }
}

/// Download `item` into its folder under `root`, replacing what's there.
/// The files are assembled in a hidden sibling folder that's renamed into
/// place at the end, so an interrupted download never looks complete.
/// `progress` hears from yt-dlp about once a second; photo posts, which
/// don't go through yt-dlp, don't report any.
pub async fn download(
    item: &PendingDownload,
    login: &Login<'_>,
    root: &Path,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Saved> {
    // The id becomes a path component, so it must not be able to escape.
    if item.video_id.is_empty()
        || !item
            .video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("refusing unusual video id {:?}", item.video_id);
    }
    if !item
        .added_on
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-')
    {
        bail!("refusing unusual date {:?}", item.added_on);
    }
    let rel = format!("{}/{}/{}", item.platform, item.added_on, folder_name(item));
    let final_dir = root.join(&rel);
    let work = root
        .join(&item.platform)
        .join(format!(".{}.partial", item.video_id));
    if work.exists() {
        std::fs::remove_dir_all(&work).with_context(|| format!("clearing {}", work.display()))?;
    }
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let result = match (item.kind.as_str(), item.platform.as_str()) {
        ("photo", "instagram") => fetch_instagram_post(item, login, &work).await,
        ("photo", _) => fetch_photo_post(item, login, &work).await,
        _ => fetch(item, login.cookies_txt, &work, progress).await,
    };
    let (format, info) = match result {
        Ok(fetched) => fetched,
        Err(e) => {
            // A failed attempt leaves nothing behind; the retry starts clean
            // either way, so a failed cleanup is only worth a warning.
            if let Err(cleanup) = std::fs::remove_dir_all(&work) {
                eprintln!("warning: couldn't remove {}: {cleanup}", work.display());
            }
            return Err(e);
        }
    };

    // Replacing an earlier download: move the old folder aside first and only
    // delete it once the new one is in place, so a failed rename can't leave
    // neither.
    let old = root
        .join(&item.platform)
        .join(format!(".{}.old", item.video_id));
    let replacing = final_dir.exists();
    if replacing {
        if old.exists() {
            std::fs::remove_dir_all(&old)?;
        }
        std::fs::rename(&final_dir, &old)
            .with_context(|| format!("moving aside {}", final_dir.display()))?;
    }
    if let Some(day) = final_dir.parent() {
        std::fs::create_dir_all(day).with_context(|| format!("creating {}", day.display()))?;
    }
    if let Err(e) = std::fs::rename(&work, &final_dir) {
        if replacing {
            std::fs::rename(&old, &final_dir)?;
        }
        return Err(e).with_context(|| format!("moving download into {}", final_dir.display()));
    }
    if replacing {
        std::fs::remove_dir_all(&old)?;
    }

    let mut files = Vec::new();
    let mut bytes = 0;
    for entry in std::fs::read_dir(&final_dir)? {
        let entry = entry?;
        bytes += entry.metadata()?.len();
        files.push(entry.file_name().to_string_lossy().into_owned());
    }
    files.sort();
    let posted_at = info.as_ref().and_then(posted_at);
    Ok(Saved {
        dir: rel,
        files,
        bytes,
        format,
        info_json: info.map(|i| i.to_string()),
        posted_at,
    })
}

/// A video's folder name: the first line of its title, or for an untitled
/// one its author, then its id, which keeps names unique when titles repeat
/// or differ only in case (the same name to Windows and macOS). For example
/// `Never Gonna Give You Up [dQw4w9WgXcQ]`.
fn folder_name(item: &PendingDownload) -> String {
    let title = item
        .title
        .as_deref()
        .and_then(|title| title.lines().map(str::trim).find(|line| !line.is_empty()));
    let label = match (title, item.author_name.as_deref()) {
        (Some(title), _) => safe_name(title, TITLE_BYTES),
        (None, Some(handle)) if matches!(item.platform.as_str(), "tiktok" | "instagram") => {
            safe_name(&format!("@{handle}"), TITLE_BYTES)
        }
        (None, Some(author)) => safe_name(author, TITLE_BYTES),
        (None, None) => String::new(),
    };
    if label.is_empty() {
        format!("[{}]", item.video_id)
    } else {
        format!("{label} [{}]", item.video_id)
    }
}

/// Run yt-dlp (and for TikTok, the mux) inside `work`. Leaves `video.<ext>`
/// and `thumbnail.<ext>` there; returns the format ids and trimmed metadata.
async fn fetch(
    item: &PendingDownload,
    cookies_txt: &Path,
    work: &Path,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<(String, Option<Value>)> {
    let tiktok = item.platform == "tiktok";
    let media_name = if tiktok { "%(format_id)s" } else { "video" };
    let mut cmd = crate::tools::yt_dlp()?;
    cmd.arg("--cookies")
        .arg(cookies_txt)
        .arg("--no-playlist")
        // --print below makes yt-dlp quiet, which hides progress unless asked
        // for. It goes to stdout, one line per update, at most once a second.
        .arg("--progress")
        .arg("--newline")
        .arg("--progress-delta")
        .arg("1")
        .arg("--progress-template")
        .arg(format!(
            "download:{PROGRESS_MARK}\t%(progress.downloaded_bytes)s\t%(progress.total_bytes)s\t\
             %(progress.total_bytes_estimate)s\t%(progress.speed)s\t%(progress.eta)s\t%(info.format_id)s"
        ))
        .arg("--socket-timeout")
        .arg("30")
        .arg("-f")
        .arg(if tiktok {
            TIKTOK_FORMATS
        } else {
            MERGED_FORMATS
        })
        .arg("-o")
        .arg(work.join(format!("{media_name}.%(ext)s")))
        // yt-dlp can't tell which of TikTok's covers is biggest (it doesn't
        // know their sizes, and picks a half-size one), so for TikTok take
        // them all and let `keep_largest_thumbnail` choose. YouTube's are
        // ranked properly, and it has dozens.
        .arg(if tiktok {
            "--write-all-thumbnails"
        } else {
            "--write-thumbnail"
        })
        .arg("-o")
        .arg(format!(
            "thumbnail:{}",
            work.join("thumbnail.%(ext)s").display()
        ))
        .arg("--write-info-json")
        .arg("-o")
        .arg(format!("infojson:{}", work.join("info").display()))
        // One line per file written, once it's in its final place. --print
        // normally implies --simulate, but not for this late stage.
        .arg("--print")
        .arg("after_move:%(format_id)s\t%(acodec)s\t%(height)s\t%(filepath)s")
        .arg(&item.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let printed = run_yt_dlp(&mut cmd, progress).await?;

    let written = parse_written(&printed);
    let format = match written.as_slice() {
        [] => bail!("yt-dlp reported no downloaded file"),
        [only] => {
            if tiktok {
                rename_to_video(&only.path, work)?;
            }
            only.format_id.clone()
        }
        [a, b] if tiktok => {
            // The picture comes from the taller file (`bv*`), the sound from
            // the other (`b`).
            let (video, sound) = if a.height >= b.height { (a, b) } else { (b, a) };
            if !sound.has_audio {
                bail!(
                    "neither of TikTok's files {} and {} has audio to keep",
                    a.format_id,
                    b.format_id
                );
            }
            mux(&video.path, &sound.path, work).await?;
            format!("{}+{}", video.format_id, sound.format_id)
        }
        _ => bail!("yt-dlp wrote {} media files; expected one", written.len()),
    };
    if tiktok {
        keep_largest_thumbnail(work).await?;
    }

    let info_path = work.join("info.info.json");
    let info = if info_path.exists() {
        let text = std::fs::read_to_string(&info_path)?;
        std::fs::remove_file(&info_path)?;
        Some(trim_info(
            serde_json::from_str(&text).context("yt-dlp metadata isn't JSON")?,
        ))
    } else {
        None
    };
    Ok((format, info))
}

/// Run yt-dlp, passing its progress lines to `progress` as they come.
/// Returns the rest of what it printed.
async fn run_yt_dlp(cmd: &mut Command, progress: &(dyn Fn(Progress) + Sync)) -> Result<String> {
    let mut child = cmd
        .spawn()
        .context("failed to run yt-dlp; is it installed?")?;
    let stdout = child.stdout.take().context("no stdout from yt-dlp")?;
    let mut stderr = child.stderr.take().context("no stderr from yt-dlp")?;
    // Read both pipes at once: yt-dlp blocks if either fills up.
    let read_stdout = async {
        let mut printed = String::new();
        let mut lines = BufReader::new(stdout).split(b'\n');
        while let Some(line) = lines.next_segment().await? {
            let line = String::from_utf8_lossy(&line);
            if line.starts_with(PROGRESS_MARK) {
                // A file yt-dlp already has (TikTok's best video, when it
                // has sound too, is also the best combined file) reports
                // progress with no byte counts. It's still not a file line.
                if let Some(update) = parse_progress(&line) {
                    progress(update);
                }
            } else {
                printed.push_str(&line);
                printed.push('\n');
            }
        }
        Ok::<_, std::io::Error>(printed)
    };
    let read_stderr = async {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    };
    let (printed, errors) =
        tokio::try_join!(read_stdout, read_stderr).context("reading yt-dlp's output")?;
    let status = child.wait().await.context("waiting for yt-dlp")?;
    if !status.success() {
        bail!(
            "yt-dlp failed: {}",
            error_lines(&String::from_utf8_lossy(&errors))
        );
    }
    Ok(printed)
}

/// Save a TikTok photo post's images and sound into `work`. Returns the
/// format label and the item as metadata.
async fn fetch_photo_post(
    item: &PendingDownload,
    login: &Login<'_>,
    work: &Path,
) -> Result<(String, Option<Value>)> {
    let post = tiktok::photo_post(login.browser, login.cookies, &item.url).await?;
    for (i, image) in post.images.iter().enumerate() {
        let name = format!(
            "image-{:02}.{}",
            i + 1,
            extension_for(&image.bytes, &image.mime)
        );
        std::fs::write(work.join(name), &image.bytes)?;
    }
    if let Some(sound) = &post.sound {
        std::fs::write(
            work.join(format!(
                "sound.{}",
                extension_for(&sound.bytes, &sound.mime)
            )),
            &sound.bytes,
        )?;
    }
    Ok(("photo".to_string(), Some(post.item)))
}

/// Save an Instagram photo post or carousel into `work`. Returns the format
/// label and the post as metadata.
async fn fetch_instagram_post(
    item: &PendingDownload,
    login: &Login<'_>,
    work: &Path,
) -> Result<(String, Option<Value>)> {
    let post =
        instagram::photo_post(login.browser, login.cookies, &item.url, &item.video_id).await?;
    for (i, slide) in post.slides.iter().enumerate() {
        let kind = if slide.is_video { "video" } else { "image" };
        fetch_file(&slide.url, work, &format!("{kind}-{:02}", i + 1)).await?;
    }
    if let Some(cover) = &post.cover {
        fetch_file(cover, work, "thumbnail").await?;
    }
    if let Some(sound) = &post.sound {
        let track = fetch_file(&sound.url, work, "track").await?;
        keep_clip(&track, sound.clip, work).await?;
    }
    Ok(("photo".to_string(), Some(post.item)))
}

/// Download `url` into `work` as `<stem>.<ext>`, the extension from what the
/// file turns out to be. Returns its path. For Instagram's media links, which
/// are signed and need no login.
async fn fetch_file(url: &str, work: &Path, stem: &str) -> Result<PathBuf> {
    let url = url.to_string();
    let work = work.to_path_buf();
    let stem = stem.to_string();
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_mins(1)))
            // Only stops a download that has stalled; a carousel's videos
            // run to tens of megabytes.
            .timeout_recv_body(Some(Duration::from_mins(10)))
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .call()
            .with_context(|| format!("downloading {stem}"))?;
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let partial = work.join(format!("{stem}.partial"));
        let mut file = std::fs::File::create(&partial)
            .with_context(|| format!("creating {}", partial.display()))?;
        std::io::copy(&mut response.body_mut().as_reader(), &mut file)
            .with_context(|| format!("downloading {stem}"))?;
        drop(file);
        let mut head = Vec::with_capacity(16);
        std::io::Read::read_to_end(
            &mut std::io::Read::take(std::fs::File::open(&partial)?, 16),
            &mut head,
        )?;
        let path = work.join(format!("{stem}.{}", extension_for(&head, &mime)));
        std::fs::rename(&partial, &path)?;
        Ok(path)
    })
    .await
    .context("the download stopped")?
}

/// Keep the part of a post's music the post plays, cut from the whole
/// `track`, as `sound.<ext>`, or the whole track when the post doesn't say.
/// Instagram plays that part over the pictures, over and over.
async fn keep_clip(track: &Path, clip: Option<(u64, u64)>, work: &Path) -> Result<()> {
    // Instagram serves the music as `video/mp4`, but there's no picture in it.
    let ext = match track.extension().and_then(|e| e.to_str()) {
        Some("mp4") | None => "m4a",
        Some(ext) => ext,
    };
    let out_path = work.join(format!("sound.{ext}"));
    let Some((start, length)) = clip else {
        std::fs::rename(track, &out_path)?;
        return Ok(());
    };
    let seconds = |ms: u64| format!("{}.{:03}", ms / 1000, ms % 1000);
    let out = crate::tools::ffmpeg()?
        .args(["-v", "error", "-y", "-ss"])
        .arg(seconds(start))
        .arg("-t")
        .arg(seconds(length))
        .arg("-i")
        .arg(track)
        .args(["-map", "0:a:0", "-c", "copy"])
        .arg(&out_path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run ffmpeg; is it installed?")?;
    if !out.status.success() {
        bail!(
            "ffmpeg couldn't cut the post's music: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::remove_file(track)?;
    Ok(())
}

/// A file extension for fetched bytes, from their leading magic bytes, or
/// failing that their Content-Type. The bytes come first because TikTok's
/// headers aren't reliable: slideshow sounds are MP3s served as `video/mp4`.
fn extension_for(bytes: &[u8], mime: &str) -> &'static str {
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => return "jpg",
        [0x89, b'P', b'N', b'G', ..] => return "png",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => return "webp",
        [b'I', b'D', b'3', ..] => return "mp3",
        // An MPEG audio frame header without an ID3 tag in front.
        [0xFF, second, ..] if second & 0xE0 == 0xE0 => return "mp3",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => {
            return if mime.starts_with("video/") {
                "mp4"
            } else {
                "m4a"
            };
        }
        _ => {}
    }
    match mime.split(';').next().unwrap_or("").trim() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

/// The files yt-dlp reports in its `--print after_move` lines, each once.
fn parse_written(stdout: &str) -> Vec<Written> {
    let mut written: Vec<Written> = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(4, '\t');
        let (Some(format_id), Some(acodec), Some(height), Some(path)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = PathBuf::from(path);
        // A format chosen twice (TikTok's best video already has sound) is
        // written once and reported twice.
        if written.iter().any(|w| w.path == path) {
            continue;
        }
        written.push(Written {
            format_id: format_id.to_string(),
            has_audio: acodec != "none",
            // yt-dlp prints NA for a missing field.
            height: height.parse().unwrap_or(0),
            path,
        });
    }

    written
}

/// A progress line from `--progress-template`, or None for any other line.
/// yt-dlp prints NA for a field it doesn't know.
fn parse_progress(line: &str) -> Option<Progress> {
    let fields: Vec<&str> = line.strip_prefix(PROGRESS_MARK)?.split('\t').collect();
    let ["", downloaded, total, estimate, speed, eta, format_id] = fields.as_slice() else {
        return None;
    };
    let number = |text: &str| text.parse::<f64>().ok().filter(|n| n.is_finite());
    Some(Progress {
        downloaded: downloaded.parse().ok()?,
        total: number(total).or_else(|| number(estimate)),
        speed: number(speed),
        eta: eta.parse().ok(),
        format_id: (*format_id).to_string(),
    })
}

/// Combine `video`'s picture with `sound`'s audio into `video.<ext>`, copying
/// both streams as they are (no re-encode), and delete the two inputs.
async fn mux(video: &Path, sound: &Path, work: &Path) -> Result<()> {
    let ext = video.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let out_path = work.join(format!("video.{ext}"));
    let out = crate::tools::ffmpeg()?
        .args(["-v", "error", "-y", "-i"])
        .arg(video)
        .arg("-i")
        .arg(sound)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"])
        .arg(&out_path)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run ffmpeg; is it installed?")?;
    if !out.status.success() {
        bail!(
            "ffmpeg mux failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::remove_file(video)?;
    std::fs::remove_file(sound)?;
    Ok(())
}

/// Of the `thumbnail.*` files in `work`, keep the still image with the most
/// pixels as `thumbnail.<ext>`, named for its real format (TikTok serves
/// JPEGs under an `.image` extension), and delete the rest.
async fn keep_largest_thumbnail(work: &Path) -> Result<()> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(work)? {
        let path = entry?.path();
        let is_thumbnail = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("thumbnail."));
        if is_thumbnail {
            candidates.push(path);
        }
    }
    let mut best: Option<(u64, &'static str, &PathBuf)> = None;
    for path in &candidates {
        let Some((pixels, ext)) = probe_image(path).await? else {
            continue;
        };
        if best.is_none_or(|(most, _, _)| pixels > most) {
            best = Some((pixels, ext, path));
        }
    }
    let Some((_, ext, best)) = best else {
        // No usable image; keep nothing rather than an animated preview.
        for path in &candidates {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    };
    let kept = work.join(format!("thumbnail.{ext}.keep"));
    std::fs::rename(best, &kept)?;
    for path in &candidates {
        if path != best {
            std::fs::remove_file(path)?;
        }
    }
    std::fs::rename(&kept, work.join(format!("thumbnail.{ext}")))?;
    Ok(())
}

/// A still image's pixel count and file extension, via ffprobe. None for
/// anything else, including animated images.
async fn probe_image(path: &Path) -> Result<Option<(u64, &'static str)>> {
    let out = crate::tools::ffprobe()?
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run ffprobe; is ffmpeg installed?")?;
    Ok(parse_probe(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse ffprobe's `codec_name,width,height` line for an image.
fn parse_probe(line: &str) -> Option<(u64, &'static str)> {
    let mut parts = line.trim().split(',');
    let ext = match parts.next()? {
        "mjpeg" => "jpg",
        "png" => "png",
        // ffprobe calls an animated WebP "webp_anim"; it's a moving preview.
        "webp" => "webp",
        _ => return None,
    };
    let width: u64 = parts.next()?.parse().ok()?;
    let height: u64 = parts.next()?.parse().ok()?;
    Some((width * height, ext))
}

fn rename_to_video(path: &Path, work: &Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    std::fs::rename(path, work.join(format!("video.{ext}")))?;
    Ok(())
}

fn trim_info(mut info: Value) -> Value {
    if let Some(fields) = info.as_object_mut() {
        for key in INFO_DROP {
            fields.remove(*key);
        }
    }
    info
}

fn posted_at(info: &Value) -> Option<i64> {
    ["timestamp", "release_timestamp"]
        .iter()
        .find_map(|key| info.get(key).and_then(Value::as_i64))
}

/// The useful part of yt-dlp's stderr: its ERROR lines, or failing that the
/// last few lines.
fn error_lines(stderr: &str) -> String {
    let errors: Vec<&str> = stderr.lines().filter(|l| l.starts_with("ERROR:")).collect();
    if errors.is_empty() {
        let lines: Vec<&str> = stderr.lines().collect();
        lines[lines.len().saturating_sub(5)..].join("\n")
    } else {
        errors.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_thumbnail_or_first_image_as_the_picture() {
        let saved = |files: &[&str]| Saved {
            dir: String::new(),
            files: files.iter().map(|&f| f.into()).collect(),
            bytes: 0,
            format: String::new(),
            info_json: None,
            posted_at: None,
        };
        let picture = |files| saved(files).picture().map(String::from);
        assert_eq!(
            picture(&["thumbnail.jpg", "video.mp4"]).as_deref(),
            Some("thumbnail.jpg")
        );
        assert_eq!(
            picture(&["image-01.webp", "image-02.webp", "sound.mp3"]).as_deref(),
            Some("image-01.webp")
        );
        assert_eq!(picture(&["video.mp4"]), None);
    }

    #[test]
    fn trims_formats_and_urls_from_metadata() {
        let info = serde_json::json!({
            "id": "a", "title": "t", "timestamp": 1_700_000_000,
            "formats": [{"url": "https://x"}], "thumbnails": [], "url": "https://x",
        });
        let trimmed = trim_info(info);
        assert_eq!(
            trimmed,
            serde_json::json!({"id": "a", "title": "t", "timestamp": 1_700_000_000})
        );
        assert_eq!(posted_at(&trimmed), Some(1_700_000_000));
    }

    #[test]
    fn falls_back_to_release_timestamp() {
        let info = serde_json::json!({"timestamp": null, "release_timestamp": 5});
        assert_eq!(posted_at(&info), Some(5));
        assert_eq!(posted_at(&serde_json::json!({})), None);
    }

    #[test]
    fn sweeps_only_leftover_folders() {
        let root = tempfile::tempdir().unwrap();
        let tiktok = root.path().join("tiktok");
        for dir in [".1.partial", ".2.old", "3", ".hidden"] {
            std::fs::create_dir_all(tiktok.join(dir)).unwrap();
        }
        sweep_leftovers(root.path()).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&tiktok)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [".hidden", "3"]);
        // A missing media root is fine.
        sweep_leftovers(&root.path().join("nope")).unwrap();
    }

    #[test]
    fn parses_written_files_once_each() {
        let written = parse_written(
            "bytevc1_1080p-1\tnone\t1920\t/w/a.mp4\n\
             h264_720p-1\taac\t1280\t/w/b.mp4\n\
             h264_720p-1\taac\t1280\t/w/b.mp4\n\
             audio\tmp3\tNA\t/w/c\td.mp3\n\
             noise\n",
        );
        let summary: Vec<_> = written
            .iter()
            .map(|w| {
                (
                    w.format_id.as_str(),
                    w.has_audio,
                    w.height,
                    w.path.to_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("bytevc1_1080p-1", false, 1920, "/w/a.mp4"),
                ("h264_720p-1", true, 1280, "/w/b.mp4"),
                // A tab in the path survives; NA height reads as 0.
                ("audio", true, 0, "/w/c\td.mp3"),
            ]
        );
    }

    fn pending(platform: &str, title: Option<&str>, author_name: Option<&str>) -> PendingDownload {
        PendingDownload {
            platform: platform.into(),
            video_id: "abc".into(),
            url: "https://example.com".into(),
            kind: "video".into(),
            title: title.map(Into::into),
            author_name: author_name.map(Into::into),
            added_on: "2026-10-12".into(),
        }
    }

    #[test]
    fn names_folders_after_the_title_and_id() {
        let name = |platform, title, author| folder_name(&pending(platform, title, author));
        assert_eq!(
            name("youtube", Some("Who? Me: yes"), Some("chan")),
            "Who？ Me： yes [abc]"
        );
        // A caption's first line; blank lines before it don't count.
        assert_eq!(
            name("tiktok", Some("\n  first line #fyp\nmore"), None),
            "first line #fyp [abc]"
        );
        assert_eq!(name("tiktok", Some("  "), Some("cat")), "@cat [abc]");
        assert_eq!(name("instagram", None, Some("cat")), "@cat [abc]");
        assert_eq!(name("youtube", None, Some("chan")), "chan [abc]");
        assert_eq!(name("tiktok", Some("..."), None), "[abc]");
        let long = name("youtube", Some(&"word ".repeat(100)), None);
        assert!(long.len() <= TITLE_BYTES + " [abc]".len());
        assert!(long.ends_with("… [abc]"));
    }

    #[tokio::test]
    async fn rejects_dates_that_could_escape_the_media_root() {
        let item = PendingDownload {
            added_on: "../..".into(),
            ..pending("youtube", None, None)
        };
        let root = tempfile::tempdir().unwrap();
        let login = Login {
            browser: Browser::Brave,
            cookies: &[],
            cookies_txt: Path::new("/nonexistent"),
        };
        let err = download(&item, &login, root.path(), &|_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unusual date"));
    }

    #[test]
    fn parses_progress_lines() {
        assert_eq!(
            parse_progress("joyscroller-progress\t1024\t223779\tNA\t222283.78\t1\t395"),
            Some(Progress {
                downloaded: 1024,
                total: Some(223_779.0),
                speed: Some(222_283.78),
                eta: Some(1),
                format_id: "395".into(),
            })
        );
        // Fragmented streams only have an estimate; a stalled one no speed.
        assert_eq!(
            parse_progress("joyscroller-progress\t5\tNA\t99.5\tNA\tNA\thls-1"),
            Some(Progress {
                downloaded: 5,
                total: Some(99.5),
                speed: None,
                eta: None,
                format_id: "hls-1".into(),
            })
        );
        assert_eq!(parse_progress("395+251\topus\t240\t/w/video.webm"), None);
        assert_eq!(parse_progress("joyscroller-progress\tNA\tNA"), None);
    }

    #[test]
    fn names_extensions_by_content_first() {
        // An MP3 served as video/mp4, as TikTok does for slideshow sounds.
        assert_eq!(extension_for(b"ID3\x04\x00", "video/mp4"), "mp3");
        assert_eq!(extension_for(&[0xFF, 0xFB, 0x90], ""), "mp3");
        assert_eq!(
            extension_for(&[0xFF, 0xD8, 0xFF, 0xE0], "image/webp"),
            "jpg"
        );
        assert_eq!(extension_for(b"RIFF\x00\x00\x00\x00WEBPVP8", ""), "webp");
        assert_eq!(
            extension_for(b"\x00\x00\x00\x20ftypM4A ", "audio/mp4"),
            "m4a"
        );
        // Unrecognized bytes fall back to the Content-Type.
        assert_eq!(extension_for(b"????", "image/heic; x=y"), "heic");
        assert_eq!(extension_for(b"", "application/x-what"), "bin");
    }

    #[test]
    fn parses_image_probes() {
        assert_eq!(parse_probe("mjpeg,1080,1920\n"), Some((1080 * 1920, "jpg")));
        assert_eq!(parse_probe("webp,10,10"), Some((100, "webp")));
        assert_eq!(parse_probe("webp_anim,270,480"), None);
        assert_eq!(parse_probe(""), None);
    }

    #[test]
    fn drops_cookies_and_stream_fields_from_metadata() {
        let info = serde_json::json!({"id": "a", "cookies": "sessionid=x", "width": 720});
        assert_eq!(trim_info(info), serde_json::json!({"id": "a"}));
    }

    #[test]
    fn picks_error_lines_from_stderr() {
        let stderr =
            "[youtube] abc: Downloading\nWARNING: meh\nERROR: [youtube] abc: Private video\n";
        assert_eq!(error_lines(stderr), "ERROR: [youtube] abc: Private video");
        assert_eq!(error_lines("a\nb\nc\nd\ne\nf\ng"), "c\nd\ne\nf\ng");
    }

    #[tokio::test]
    async fn rejects_ids_that_could_escape_the_media_root() {
        let item = PendingDownload {
            video_id: "../../etc".into(),
            ..pending("youtube", None, None)
        };
        let root = tempfile::tempdir().unwrap();
        let login = Login {
            browser: Browser::Brave,
            cookies: &[],
            cookies_txt: Path::new("/nonexistent"),
        };
        let err = download(&item, &login, root.path(), &|_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unusual video id"));
    }
}
