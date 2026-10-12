mod browser;
mod db;
mod download;
mod headless;
mod tiktok;
mod youtube;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use browser::Browser;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "joyscroller", about = "Archive the videos you liked, forever.")]
struct Cli {
    /// With no subcommand, start the app. The subcommands are developer tools.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Platform {
    Youtube,
    Tiktok,
}

impl Platform {
    /// The name stored in the `platform` column.
    fn name(self) -> &'static str {
        match self {
            Platform::Youtube => "youtube",
            Platform::Tiktok => "tiktok",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Export the browser's cookies for a domain to a Netscape cookies.txt.
    Cookies {
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        #[arg(long, default_value = "youtube.com")]
        domain: String,
        /// Where to write cookies.txt (default: stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// List the videos you've liked.
    Likes {
        #[arg(long, value_enum, default_value = "youtube")]
        platform: Platform,
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        /// How many to list (0 = all).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Save your likes on one platform to the local database.
    Sync {
        #[arg(long, value_enum, default_value = "youtube")]
        platform: Platform,
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        /// Database file (default: ~/.local/share/joyscroller/joyscroller.db).
        #[arg(long)]
        db: Option<std::path::PathBuf>,
        /// Read the whole liked list instead of stopping once we reach likes we
        /// already have. Slower, but refreshes `available` for every video.
        #[arg(long)]
        full: bool,
    },
    /// Download the media of the next few liked videos that don't have it yet.
    Download {
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        /// Database file (default: ~/.local/share/joyscroller/joyscroller.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Where media goes (default: ~/.local/share/joyscroller/media).
        #[arg(long)]
        media: Option<PathBuf>,
        /// How many videos to download.
        #[arg(long, default_value = "1")]
        limit: usize,
    },
}

/// How many already-stored likes in a row end a sync (about one page).
const STOP_AFTER_KNOWN: usize = 100;

/// How often the app pulls new likes while it's running.
const SYNC_EVERY: Duration = Duration::from_mins(5);

/// Queued downloads are read from the database this many at a time, and
/// cookies are re-read once per batch.
const DOWNLOAD_BATCH: usize = 10;

/// The pause after each download. A backlog runs to thousands of videos, and
/// YouTube answers bursts of requests with HTTP 429.
const DOWNLOAD_PAUSE: Duration = Duration::from_secs(5);

/// The pause after a failed download, which is often a sign of rate limiting.
const DOWNLOAD_FAILURE_PAUSE: Duration = Duration::from_mins(1);

/// How long the downloader waits before checking an empty queue again.
const DOWNLOAD_IDLE: Duration = Duration::from_mins(1);

/// This many failed downloads in a row looks like a problem on our side or the
/// platform's (yt-dlp broken, login expired, offline, rate limited) rather than
/// with the videos, so the downloader stops for `DOWNLOAD_OUTAGE_PAUSE` instead
/// of failing its way through the queue.
const FAILURES_BEFORE_PAUSE: usize = 3;
const DOWNLOAD_OUTAGE_PAUSE: Duration = Duration::from_mins(30);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return run_app(
            Browser::Brave,
            "Default",
            &db::default_path()?,
            &download::default_root()?,
        )
        .await;
    };
    match command {
        Command::Cookies {
            browser,
            profile,
            domain,
            out,
        } => {
            let cookies = browser::cookies_for(browser, &profile, &domain).await?;
            let txt = browser::to_netscape(&cookies);
            match out {
                Some(path) => {
                    std::fs::write(&path, txt)?;
                    eprintln!("wrote {} cookies to {}", cookies.len(), path.display());
                }
                None => print!("{txt}"),
            }
        }
        Command::Likes {
            platform,
            browser,
            profile,
            limit,
        } => match platform {
            Platform::Youtube => print_youtube_likes(browser, &profile, limit).await?,
            Platform::Tiktok => print_tiktok_likes(browser, &profile, limit).await?,
        },
        Command::Sync {
            platform,
            browser,
            profile,
            db,
            full,
        } => {
            let db_path = match db {
                Some(path) => path,
                None => db::default_path()?,
            };
            let mut conn = db::open(&db_path)?;
            let (listed, added) = sync(platform, browser, &profile, &mut conn, full).await?;
            eprintln!(
                "{}: listed {listed}, added {added} new → {}",
                platform.name(),
                db_path.display()
            );
        }
        Command::Download {
            browser,
            profile,
            db,
            media,
            limit,
        } => {
            let db_path = match db {
                Some(path) => path,
                None => db::default_path()?,
            };
            let media_root = match media {
                Some(path) => path,
                None => download::default_root()?,
            };
            let mut conn = db::open(&db_path)?;
            let attempted =
                download_batch(browser, &profile, &mut conn, &media_root, limit).await?;
            eprintln!("attempted {attempted} → {}", media_root.display());
        }
    }
    Ok(())
}

/// The `likes` command for YouTube.
async fn print_youtube_likes(browser: Browser, profile: &str, limit: usize) -> Result<()> {
    let cookies = browser::cookies_for(browser, profile, "youtube.com").await?;
    eprintln!("extracted {} youtube.com cookies", cookies.len());

    // yt-dlp needs the cookies on disk; keep the temp file for the call's lifetime.
    let tmp = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(tmp.path(), browser::to_netscape(&cookies))?;

    let videos = youtube::liked_videos(tmp.path(), limit, |_| true).await?;
    eprintln!("\n{} liked videos:\n", videos.len());
    for (i, v) in videos.iter().enumerate() {
        let dur = v
            .duration_seconds
            .map_or_else(|| "--:--".into(), |s| format!("{}:{:02}", s / 60, s % 60));
        let channel = v.channel.as_deref().unwrap_or("?");
        println!("{:>3}. [{dur}] {} — {}", i + 1, v.title, channel);
        println!("     {}", v.url());
    }
    Ok(())
}

/// The `likes` command for TikTok.
async fn print_tiktok_likes(browser: Browser, profile: &str, limit: usize) -> Result<()> {
    let cookies = browser::cookies_for(browser, profile, "tiktok.com").await?;
    eprintln!("extracted {} tiktok.com cookies", cookies.len());
    let videos = tiktok::liked_videos(browser, &cookies, limit, |_| true).await?;
    eprintln!("\n{} liked videos:\n", videos.len());
    for (i, v) in videos.iter().enumerate() {
        let dur = v
            .duration_seconds
            .map_or_else(|| "--:--".into(), |s| format!("{}:{:02}", s / 60, s % 60));
        let author = v.author_handle.as_deref().unwrap_or("?");
        // Descriptions run long with hashtags; the first line is enough here.
        let desc = v.desc.lines().next().unwrap_or("");
        println!("{:>3}. [{dur}] @{author} — {desc}", i + 1);
        println!("     {}", v.url());
    }
    Ok(())
}

/// The app: keeps running, syncing new likes every `SYNC_EVERY` while a
/// background task downloads their media.
async fn run_app(browser: Browser, profile: &str, db_path: &Path, media_root: &Path) -> Result<()> {
    let mut conn = db::open(db_path)?;
    eprintln!(
        "joyscroller: syncing into {}, media into {} (Ctrl-C to stop)",
        db_path.display(),
        media_root.display()
    );
    tokio::spawn(download_loop(
        browser,
        profile.to_string(),
        db_path.to_path_buf(),
        media_root.to_path_buf(),
    ));

    // The first tick fires immediately. If a sync overruns the interval, wait a
    // full interval after it rather than firing back-to-back.
    let mut ticks = tokio::time::interval(SYNC_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        // A failed sync (offline, logged out) is logged, not fatal: the next
        // tick tries again, and one platform failing doesn't skip the others.
        for platform in [Platform::Youtube, Platform::Tiktok] {
            let name = platform.name();
            match sync(platform, browser, profile, &mut conn, false).await {
                Ok((listed, added)) => eprintln!("{name}: listed {listed}, added {added} new"),
                Err(e) => eprintln!("{name}: sync failed: {e:#}"),
            }
        }
    }
}

/// Download queued media forever, one video at a time. Errors are logged and
/// retried after a pause; like a failed sync, they never stop the app.
async fn download_loop(browser: Browser, profile: String, db_path: PathBuf, media_root: PathBuf) {
    // A separate connection from the syncs', so neither waits on the other
    // longer than a single write.
    let mut conn = loop {
        match db::open(&db_path) {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("download: can't open the database: {e:#}");
                tokio::time::sleep(DOWNLOAD_IDLE).await;
            }
        }
    };
    // Nothing is downloading yet, so any partial folders are from a run that
    // was interrupted.
    if let Err(e) = download::sweep_leftovers(&media_root) {
        eprintln!("download: couldn't clear interrupted downloads: {e:#}");
    }
    loop {
        match download_batch(browser, &profile, &mut conn, &media_root, DOWNLOAD_BATCH).await {
            Ok(0) => tokio::time::sleep(DOWNLOAD_IDLE).await,
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "download: {e:#}; pausing for {} min",
                    DOWNLOAD_OUTAGE_PAUSE.as_secs() / 60
                );
                tokio::time::sleep(DOWNLOAD_OUTAGE_PAUSE).await;
            }
        }
    }
}

/// Download up to `limit` queued videos. Returns how many were attempted.
/// A failed video is recorded (so it's retried later) and doesn't stop the
/// batch; database errors and `FAILURES_BEFORE_PAUSE` failures in a row do.
async fn download_batch(
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    media_root: &Path,
    limit: usize,
) -> Result<usize> {
    let batch = db::pending_downloads(conn, limit)?;
    // yt-dlp reads cookies from a file; one per platform, for this batch.
    let mut cookie_files: HashMap<String, tempfile::NamedTempFile> = HashMap::new();
    let mut failures_in_a_row = 0;
    for item in &batch {
        let started = std::time::Instant::now();
        let name = &item.platform;
        let result = async {
            if !cookie_files.contains_key(&item.platform) {
                let domain = match item.platform.as_str() {
                    "youtube" => "youtube.com",
                    "tiktok" => "tiktok.com",
                    other => anyhow::bail!("can't download from platform {other}"),
                };
                let cookies = browser::cookies_for(browser, profile, domain).await?;
                let file = tempfile::Builder::new().suffix(".txt").tempfile()?;
                std::fs::write(file.path(), browser::to_netscape(&cookies))?;
                cookie_files.insert(item.platform.clone(), file);
            }
            download::download(item, cookie_files[&item.platform].path(), media_root).await
        }
        .await;
        match result {
            Ok(saved) => {
                db::record_download(
                    conn,
                    &item.platform,
                    &item.video_id,
                    &db::Downloaded {
                        dir: &saved.dir,
                        files: &saved.files,
                        bytes: saved.bytes,
                        format: Some(&saved.format),
                        info_json: saved.info_json.as_deref(),
                        posted_at: saved.posted_at,
                    },
                )?;
                #[expect(clippy::cast_precision_loss, reason = "a size for a log line")]
                let megabytes = saved.bytes as f64 / 1e6;
                eprintln!(
                    "{name}: downloaded {} ({megabytes:.1} MB, format {}) in {}s",
                    item.video_id,
                    saved.format,
                    started.elapsed().as_secs()
                );
                failures_in_a_row = 0;
                tokio::time::sleep(DOWNLOAD_PAUSE).await;
            }
            Err(e) => {
                db::record_download_failure(
                    conn,
                    &item.platform,
                    &item.video_id,
                    &format!("{e:#}"),
                )?;
                eprintln!("{name}: download of {} failed: {e:#}", item.video_id);
                failures_in_a_row += 1;
                if failures_in_a_row == FAILURES_BEFORE_PAUSE {
                    anyhow::bail!("{failures_in_a_row} downloads failed in a row");
                }
                tokio::time::sleep(DOWNLOAD_FAILURE_PAUSE).await;
            }
        }
    }
    Ok(batch.len())
}

/// Pull new likes on `platform` into the database. Returns (listed, added).
async fn sync(
    platform: Platform,
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    full: bool,
) -> Result<(usize, usize)> {
    match platform {
        Platform::Youtube => sync_youtube(browser, profile, conn, full).await,
        Platform::Tiktok => sync_tiktok(browser, profile, conn, full).await,
    }
}

/// The `keep_going` callback for a listing of likes, newest first: stop once
/// `STOP_AFTER_KNOWN` videos in a row are already stored, unless `full`.
///
/// A page's worth of known videos in a row means everything after it is older
/// and stored too. One known video alone isn't enough: re-liking an old video
/// moves it to the top, possibly above new likes we haven't stored yet.
fn until_known(known: &HashSet<String>, full: bool) -> impl FnMut(&str) -> bool {
    let mut known_in_a_row = 0;
    move |id| {
        if known.contains(id) {
            known_in_a_row += 1;
        } else {
            known_in_a_row = 0;
        }
        full || known_in_a_row < STOP_AFTER_KNOWN
    }
}

/// Pull new YouTube likes into the database. Returns (listed, added).
async fn sync_youtube(
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    full: bool,
) -> Result<(usize, usize)> {
    let sync_id = db::start_sync(conn, "youtube")?;

    // Re-read cookies on every sync: the browser rotates them while it runs.
    let cookies = browser::cookies_for(browser, profile, "youtube.com").await?;
    let tmp = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(tmp.path(), browser::to_netscape(&cookies))?;

    let known = db::video_ids(conn, "youtube")?;
    let mut keep_going = until_known(&known, full);
    let videos = youtube::liked_videos(tmp.path(), 0, |v| keep_going(&v.id)).await?;

    let urls: Vec<String> = videos.iter().map(youtube::Video::url).collect();
    let raws: Vec<String> = videos.iter().map(|v| v.raw.to_string()).collect();
    // yt-dlp lists newest first; insert oldest first so rowid follows like order.
    let rows: Vec<db::NewVideo> = videos
        .iter()
        .zip(&urls)
        .zip(&raws)
        .rev()
        .map(|((v, url), raw)| db::NewVideo {
            video_id: &v.id,
            url,
            title: v.available().then_some(v.title.as_str()),
            author_id: v.channel_id.as_deref(),
            author_name: v.channel.as_deref(),
            duration_seconds: v.duration_seconds,
            available: v.available(),
            raw_json: raw,
            kind: "video",
            // YouTube's liked list doesn't say; the download fills it in.
            posted_at: None,
        })
        .collect();
    let added = db::insert_new(conn, "youtube", &rows)?;
    db::finish_sync(conn, sync_id, added)?;
    Ok((videos.len(), added))
}

/// Pull new TikTok likes into the database. Returns (listed, added).
async fn sync_tiktok(
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    full: bool,
) -> Result<(usize, usize)> {
    let sync_id = db::start_sync(conn, "tiktok")?;

    // Re-read cookies on every sync: the browser rotates them while it runs.
    let cookies = browser::cookies_for(browser, profile, "tiktok.com").await?;
    let known = db::video_ids(conn, "tiktok")?;
    let mut keep_going = until_known(&known, full);
    let videos = tiktok::liked_videos(browser, &cookies, 0, |v| keep_going(&v.id)).await?;

    let urls: Vec<String> = videos.iter().map(tiktok::Video::url).collect();
    let raws: Vec<String> = videos.iter().map(|v| v.raw.to_string()).collect();
    // Listed newest like first; insert oldest first so rowid follows like order.
    let rows: Vec<db::NewVideo> = videos
        .iter()
        .zip(&urls)
        .zip(&raws)
        .rev()
        .map(|((v, url), raw)| db::NewVideo {
            video_id: &v.id,
            url,
            // Many TikToks have no caption at all.
            title: (!v.desc.is_empty()).then_some(v.desc.as_str()),
            author_id: v.author_id.as_deref(),
            author_name: v.author_handle.as_deref(),
            duration_seconds: v.duration_seconds,
            // TikTok leaves deleted and private videos out of the liked list
            // rather than listing them stripped down, as YouTube does.
            available: true,
            raw_json: raw,
            kind: if v.is_photo { "photo" } else { "video" },
            posted_at: v.posted_at,
        })
        .collect();
    let added = db::insert_new(conn, "tiktok", &rows)?;
    db::finish_sync(conn, sync_id, added)?;
    Ok((videos.len(), added))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_after_a_run_of_known_videos() {
        let known: HashSet<String> = (0..STOP_AFTER_KNOWN).map(|i| i.to_string()).collect();
        let mut keep_going = until_known(&known, false);
        assert!(keep_going("new"));
        // A known video followed by a new one resets the run.
        assert!(keep_going("0"));
        assert!(keep_going("newer"));
        let results: Vec<bool> = known.iter().map(|id| keep_going(id)).collect();
        assert!(results[..STOP_AFTER_KNOWN - 1].iter().all(|&go| go));
        assert!(!results[STOP_AFTER_KNOWN - 1]);
    }

    #[test]
    fn full_never_stops() {
        let known: HashSet<String> = (0..STOP_AFTER_KNOWN).map(|i| i.to_string()).collect();
        let mut keep_going = until_known(&known, true);
        assert!(known.iter().all(|id| keep_going(id)));
    }
}
