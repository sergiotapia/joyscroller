mod browser;
mod db;
mod youtube;

use anyhow::Result;
use browser::Browser;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "joyscroller", about = "Archive the videos you liked, forever.")]
struct Cli {
    /// With no subcommand, start the app. The subcommands are developer tools.
    #[command(subcommand)]
    command: Option<Command>,
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
    /// List the videos you've liked on YouTube.
    Likes {
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        /// How many to list (0 = all).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Save your YouTube likes to the local database.
    Sync {
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
}

/// How many already-stored likes in a row end a sync (about one page).
const STOP_AFTER_KNOWN: usize = 100;

/// How often the app pulls new likes while it's running.
const SYNC_EVERY: std::time::Duration = std::time::Duration::from_mins(5);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return run_app(Browser::Brave, "Default", &db::default_path()?).await;
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
            browser,
            profile,
            limit,
        } => {
            let cookies = browser::cookies_for(browser, &profile, "youtube.com").await?;
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
        }
        Command::Sync {
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
            let (listed, added) = sync_youtube(browser, &profile, &mut conn, full).await?;
            eprintln!(
                "youtube: listed {listed}, added {added} new → {}",
                db_path.display()
            );
        }
    }
    Ok(())
}

/// The app: keeps running, syncing new likes every `SYNC_EVERY`.
async fn run_app(browser: Browser, profile: &str, db_path: &std::path::Path) -> Result<()> {
    let mut conn = db::open(db_path)?;
    eprintln!(
        "joyscroller: syncing into {} (Ctrl-C to stop)",
        db_path.display()
    );

    // The first tick fires immediately. If a sync overruns the interval, wait a
    // full interval after it rather than firing back-to-back.
    let mut ticks = tokio::time::interval(SYNC_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        // A failed sync (offline, logged out) is logged, not fatal: the next
        // tick tries again.
        match sync_youtube(browser, profile, &mut conn, false).await {
            Ok((listed, added)) => eprintln!("youtube: listed {listed}, added {added} new"),
            Err(e) => eprintln!("youtube: sync failed: {e:#}"),
        }
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

    // Likes are listed newest first, so a page's worth of videos we already
    // have in a row means everything after it is older and stored too. One
    // known video alone isn't enough: re-liking an old video moves it to
    // the top, possibly above new likes we haven't stored yet.
    let known = db::video_ids(conn, "youtube")?;
    let mut known_in_a_row = 0;
    let videos = youtube::liked_videos(tmp.path(), 0, |v| {
        if known.contains(&v.id) {
            known_in_a_row += 1;
        } else {
            known_in_a_row = 0;
        }
        full || known_in_a_row < STOP_AFTER_KNOWN
    })
    .await?;

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
        })
        .collect();
    let added = db::insert_new(conn, "youtube", &rows)?;
    db::finish_sync(conn, sync_id, added)?;
    Ok((videos.len(), added))
}
