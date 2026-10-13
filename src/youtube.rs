//! Lists a user's liked YouTube videos by driving `yt-dlp` with cookies that
//! `browser.rs` exported. We don't speak YouTube's API ourselves — yt-dlp is
//! the maintained engine for that, and we just parse its JSON output.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// The "Liked videos" playlist.
const LIKED_PLAYLIST: &str = "https://www.youtube.com/playlist?list=LL";

#[derive(Debug, Clone)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel_id: Option<String>,
    pub channel: Option<String>,
    pub duration_seconds: Option<u64>,
    /// The full yt-dlp entry, kept so fields we don't parse yet aren't lost.
    pub raw: Value,
}

impl Video {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }

    /// Private and deleted videos stay in the liked list, but YouTube strips
    /// them down to an id and thumbnails — no title.
    pub fn available(&self) -> bool {
        !self.title.is_empty()
    }
}

/// List liked videos, newest first. `limit` of 0 means all of them.
/// `keep_going` sees each video as it arrives; returning false stops the
/// listing there (that video is still included), so callers can quit once
/// they reach likes they already have.
///
/// `--flat-playlist` lists entries without resolving each video, so the cost is
/// one request per ~100-video page (~47 for a full 5,000-like list) rather
/// than one per video. `--lazy-playlist` makes yt-dlp print each page as it
/// arrives instead of after fetching them all, which is what lets us stop early.
pub async fn liked_videos(
    cookies_txt: &Path,
    limit: usize,
    mut keep_going: impl FnMut(&Video) -> bool,
) -> Result<Vec<Video>> {
    let mut cmd = crate::tools::yt_dlp()?;
    cmd.arg("--cookies")
        .arg(cookies_txt)
        .arg("--flat-playlist")
        .arg("--lazy-playlist")
        .arg("--dump-json")
        .arg("--ignore-errors");
    if limit > 0 {
        cmd.arg("--playlist-end").arg(limit.to_string());
    }
    cmd.arg(LIKED_PLAYLIST);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .context("failed to run yt-dlp; is it installed?")?;
    // Drain stderr concurrently so a chatty yt-dlp can't block on a full pipe.
    let mut stderr_pipe = child.stderr.take().context("yt-dlp stderr not piped")?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });

    // yt-dlp emits one JSON object per line (JSON Lines), one per video.
    let mut lines = BufReader::new(child.stdout.take().context("yt-dlp stdout not piped")?).lines();
    let mut videos = Vec::new();
    let mut stopped = false;
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = v.get("id").and_then(Value::as_str) else {
            continue;
        };
        let video = Video {
            id: id.to_string(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            channel_id: v
                .get("channel_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            channel: v
                .get("channel")
                .or_else(|| v.get("uploader"))
                .and_then(Value::as_str)
                .map(str::to_string),
            duration_seconds: v.get("duration").and_then(Value::as_u64),
            raw: v,
        };
        let go_on = keep_going(&video);
        videos.push(video);
        if !go_on {
            stopped = true;
            break;
        }
    }

    if stopped {
        // yt-dlp ignores a closed pipe and would keep paging, so stop it outright.
        child.kill().await?;
        return Ok(videos);
    }
    let status = child.wait().await?;
    if videos.is_empty() && !status.success() {
        let stderr = stderr_task.await.unwrap_or_default();
        bail!("yt-dlp produced no videos. stderr:\n{}", stderr.trim());
    }
    Ok(videos)
}
