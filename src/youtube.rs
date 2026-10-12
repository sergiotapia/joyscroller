//! Lists a user's liked YouTube videos by driving `yt-dlp` with cookies that
//! `browser.rs` exported. We don't speak YouTube's API ourselves — yt-dlp is
//! the maintained engine for that, and we just parse its JSON output.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::process::Command;

/// The "Liked videos" playlist.
const LIKED_PLAYLIST: &str = "https://www.youtube.com/playlist?list=LL";

#[derive(Debug, Clone)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: Option<String>,
    pub duration_seconds: Option<u64>,
}

impl Video {
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

/// List liked videos, newest first. `limit` of 0 means all of them.
///
/// `--flat-playlist` keeps this to a single request: it lists entries without
/// resolving each video, so it's fast and light on YouTube.
pub async fn liked_videos(cookies_txt: &Path, limit: usize) -> Result<Vec<Video>> {
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--cookies")
        .arg(cookies_txt)
        .arg("--flat-playlist")
        .arg("--dump-json")
        .arg("--ignore-errors");
    if limit > 0 {
        cmd.arg("--playlist-end").arg(limit.to_string());
    }
    cmd.arg(LIKED_PLAYLIST);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().await.context("failed to run yt-dlp; is it installed?")?;

    // yt-dlp emits one JSON object per line (JSON Lines), one per video.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut videos = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(id) = v.get("id").and_then(Value::as_str) else { continue };
        videos.push(Video {
            id: id.to_string(),
            title: v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
            channel: v
                .get("channel")
                .or_else(|| v.get("uploader"))
                .and_then(Value::as_str)
                .map(str::to_string),
            duration_seconds: v.get("duration").and_then(Value::as_u64),
        });
    }

    if videos.is_empty() && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("yt-dlp produced no videos. stderr:\n{}", stderr.trim());
    }
    Ok(videos)
}
