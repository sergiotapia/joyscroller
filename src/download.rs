//! Downloads a liked video's media with yt-dlp, at the best quality on offer,
//! into a folder of its own under the media root: `<platform>/<video id>/`
//! holding `video.<ext>` and `thumbnail.<ext>`. yt-dlp's metadata from the
//! download comes back to the caller (for the database) rather than being
//! kept as a file.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::Command;

use crate::db::PendingDownload;

/// For TikTok: the best video stream, and separately the best file that has
/// both picture and sound. TikTok's highest qualities are video-only, and the
/// audio-only format yt-dlp would pair them with is the post's *sound* (the
/// song as a standalone track), not the video's own audio, so the real audio
/// has to come from a combined file. yt-dlp can't merge two files that both
/// have video (it drops the second one silently), so `mux` does it instead.
const TIKTOK_FORMATS: &str = "bv*,b";

/// For YouTube: yt-dlp's default, the best video and best audio, merged.
const YOUTUBE_FORMATS: &str = "bv*+ba/b";

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

/// `~/.local/share/joyscroller/media`
pub fn default_root() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("no XDG data dir")?
        .join("joyscroller/media"))
}

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

/// One file yt-dlp reported writing.
struct Written {
    format_id: String,
    /// False only when yt-dlp knows there's no audio (acodec `none`).
    has_audio: bool,
    /// Picture height; 0 when unknown or audio-only.
    height: u64,
    path: PathBuf,
}

/// Download `item` into its folder under `root`, replacing what's there.
/// The files are assembled in a hidden sibling folder that's renamed into
/// place at the end, so an interrupted download never looks complete.
pub async fn download(item: &PendingDownload, cookies_txt: &Path, root: &Path) -> Result<Saved> {
    // The id becomes a path component, so it must not be able to escape.
    if item.video_id.is_empty()
        || !item
            .video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("refusing unusual video id {:?}", item.video_id);
    }
    let rel = format!("{}/{}", item.platform, item.video_id);
    let final_dir = root.join(&rel);
    let work = root
        .join(&item.platform)
        .join(format!(".{}.partial", item.video_id));
    if work.exists() {
        std::fs::remove_dir_all(&work).with_context(|| format!("clearing {}", work.display()))?;
    }
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let result = fetch(item, cookies_txt, &work).await;
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

/// Run yt-dlp (and for TikTok, the mux) inside `work`. Leaves `video.<ext>`
/// and `thumbnail.<ext>` there; returns the format ids and trimmed metadata.
async fn fetch(
    item: &PendingDownload,
    cookies_txt: &Path,
    work: &Path,
) -> Result<(String, Option<Value>)> {
    let tiktok = item.platform == "tiktok";
    let media_name = if tiktok { "%(format_id)s" } else { "video" };
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--cookies")
        .arg(cookies_txt)
        .arg("--no-playlist")
        .arg("--no-progress")
        .arg("--socket-timeout")
        .arg("30")
        .arg("-f")
        .arg(if tiktok {
            TIKTOK_FORMATS
        } else {
            YOUTUBE_FORMATS
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
        .kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .context("failed to run yt-dlp; is it installed?")?;
    if !out.status.success() {
        bail!(
            "yt-dlp failed: {}",
            error_lines(&String::from_utf8_lossy(&out.stderr))
        );
    }

    let written = parse_written(&String::from_utf8_lossy(&out.stdout));
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

/// Combine `video`'s picture with `sound`'s audio into `video.<ext>`, copying
/// both streams as they are (no re-encode), and delete the two inputs.
async fn mux(video: &Path, sound: &Path, work: &Path) -> Result<()> {
    let ext = video.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let out_path = work.join(format!("video.{ext}"));
    let out = Command::new("ffmpeg")
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
    let out = Command::new("ffprobe")
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
            platform: "youtube".into(),
            video_id: "../../etc".into(),
            url: "https://example.com".into(),
            kind: "video".into(),
        };
        let root = tempfile::tempdir().unwrap();
        let err = download(&item, Path::new("/nonexistent"), root.path())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unusual video id"));
    }
}
