mod browser;
mod db;
mod download;
mod filename;
mod headless;
mod pipeline;
mod player;
mod tiktok;
mod web;
mod youtube;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
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
        /// Database file (default: joyscroller.db next to this executable, or in
        /// data/ when run from the repo's target/).
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
        /// Database file (default: joyscroller.db next to this executable, or in
        /// data/ when run from the repo's target/).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Where media goes (default: downloads/ next to the database).
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

/// The pause after each download. A backlog runs to thousands of videos, and
/// YouTube answers bursts of requests with HTTP 429.
const DOWNLOAD_PAUSE: Duration = Duration::from_secs(5);

/// The pause after a failed download, which is often a sign of rate limiting.
const DOWNLOAD_FAILURE_PAUSE: Duration = Duration::from_mins(1);

/// This many failed downloads in a row looks like a problem on our side or the
/// platform's (yt-dlp broken, login expired, offline, rate limited) rather than
/// with the videos, so the `download` command stops instead of failing its way
/// through the queue.
const FAILURES_BEFORE_PAUSE: usize = 3;

/// Where the database and the downloads live: the folder the app's
/// executable is in, where people will look for them (if they move the app,
/// moving those along is up to them). A build running from this repo's own
/// `target/` uses `data/` in the repo instead, so `cargo clean` can't delete
/// the archive. A dev binary copied elsewhere counts as an install.
fn app_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the app's executable")?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    if exe.starts_with(repo.join("target")) {
        return Ok(repo.join("data"));
    }
    Ok(exe
        .parent()
        .context("the app's executable has no folder")?
        .to_owned())
}

fn default_db() -> Result<PathBuf> {
    Ok(app_dir()?.join("joyscroller.db"))
}

fn default_media() -> Result<PathBuf> {
    Ok(app_dir()?.join("downloads"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return run_app(Browser::Brave, "Default", &default_db()?).await;
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
                None => default_db()?,
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
                None => default_db()?,
            };
            let media_root = match media {
                Some(path) => path,
                None => default_media()?,
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

/// The app: serves the web UI and keeps running, syncing new likes every
/// `SYNC_EVERY`. The pipeline's lanes download every liked video in the
/// background, and the UI can ask it for one right away.
async fn run_app(browser: Browser, profile: &str, db_path: &Path) -> Result<()> {
    let mut conn = db::open(db_path)?;
    // Bind before anything else: a second copy of the app fails here instead
    // of syncing alongside the first.
    let listener = tokio::net::TcpListener::bind(web::ADDR)
        .await
        .with_context(|| {
            format!(
                "listening on {} (is joyscroller already running?)",
                web::ADDR
            )
        })?;
    let (status, status_rx) = tokio::sync::watch::channel(web::SyncStatus::default());
    // Made up front, so the folder is there to find before anything downloads.
    let media_root = default_media()?;
    std::fs::create_dir_all(&media_root)
        .with_context(|| format!("creating {}", media_root.display()))?;
    let pipeline = pipeline::Pipeline::start(
        &[Platform::Youtube.name(), Platform::Tiktok.name()],
        browser,
        profile,
        db_path,
        &media_root,
    );
    let web_conn = db::open(db_path)?;
    let web_media = media_root.clone();
    tokio::spawn(async move {
        if let Err(e) = web::serve(listener, web_conn, status_rx, pipeline, web_media).await {
            eprintln!("web: stopped: {e:#}");
        }
    });
    eprintln!(
        "joyscroller: open http://{}, syncing into {}, downloading into {} (Ctrl-C to stop)",
        web::ADDR,
        db_path.display(),
        media_root.display()
    );

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
            status.send_modify(|s| s.set(name, web::SyncState::Running));
            let state = match sync(platform, browser, profile, &mut conn, false).await {
                Ok((listed, added)) => {
                    eprintln!("{name}: listed {listed}, added {added} new");
                    web::SyncState::Done {
                        at: db::now(),
                        added,
                    }
                }
                Err(e) => {
                    eprintln!("{name}: sync failed: {e:#}");
                    web::SyncState::Failed {
                        at: db::now(),
                        error: format!("{e:#}"),
                    }
                }
            };
            status.send_modify(|s| s.set(name, state));
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
    let batch = db::pending_downloads(conn, None, limit)?;
    // The login per platform, read once for this batch.
    let mut logins: HashMap<String, download::Credentials> = HashMap::new();
    let mut failures_in_a_row = 0;
    for item in &batch {
        let started = std::time::Instant::now();
        let result = async {
            if !logins.contains_key(&item.platform) {
                let credentials =
                    download::Credentials::read(browser, profile, &item.platform).await?;
                logins.insert(item.platform.clone(), credentials);
            }
            let login = logins[&item.platform].login();
            download::download(item, &login, media_root, &|_| {}).await
        }
        .await;
        pipeline::record(conn, item, &result, started.elapsed())?;
        if result.is_ok() {
            failures_in_a_row = 0;
            tokio::time::sleep(DOWNLOAD_PAUSE).await;
        } else {
            failures_in_a_row += 1;
            if failures_in_a_row == FAILURES_BEFORE_PAUSE {
                anyhow::bail!("{failures_in_a_row} downloads failed in a row");
            }
            tokio::time::sleep(DOWNLOAD_FAILURE_PAUSE).await;
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
