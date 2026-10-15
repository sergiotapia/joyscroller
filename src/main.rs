mod browser;
mod db;
mod desktop;
mod download;
mod filename;
mod headless;
mod instagram;
mod pipeline;
mod player;
mod query;
mod tiktok;
mod tools;
mod transcode;
mod web;
mod youtube;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use browser::Browser;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "joyscroller",
    version,
    about = "Archive the videos you liked, forever."
)]
struct Cli {
    /// With no subcommand, start the app. The subcommands are developer tools.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Platform {
    Youtube,
    Tiktok,
    Instagram,
}

impl Platform {
    /// The name stored in the `platform` column.
    fn name(self) -> &'static str {
        match self {
            Platform::Youtube => "youtube",
            Platform::Tiktok => "tiktok",
            Platform::Instagram => "instagram",
        }
    }

    /// The name people know it by.
    fn label(self) -> &'static str {
        match self {
            Platform::Youtube => "YouTube",
            Platform::Tiktok => "TikTok",
            Platform::Instagram => "Instagram",
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Export the browser's cookies for a domain to a Netscape cookies.txt.
    Cookies {
        /// The browser to read logins from (default: the one logged in to the
        /// most of YouTube, TikTok and Instagram).
        #[arg(long, value_enum)]
        browser: Option<Browser>,
        #[arg(long, default_value = "Default")]
        profile: String,
        #[arg(long, default_value = "youtube.com")]
        domain: String,
        /// Where to write cookies.txt (default: stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// List the videos you've liked (on Instagram, the posts you've saved, then the ones you've liked).
    Likes {
        #[arg(long, value_enum, default_value = "youtube")]
        platform: Platform,
        /// The browser to read logins from (default: the one logged in to the
        /// most of YouTube, TikTok and Instagram).
        #[arg(long, value_enum)]
        browser: Option<Browser>,
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
        /// The browser to read logins from (default: the one logged in to the
        /// most of YouTube, TikTok and Instagram).
        #[arg(long, value_enum)]
        browser: Option<Browser>,
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
    /// Fetch the app's own yt-dlp, ffmpeg, ffprobe and bun into tools/ (as the
    /// app does on launch) and print each one's version, to check a build's
    /// tools work on this OS.
    Tools,
    /// Download the media of the next few liked videos that don't have it yet.
    Download {
        /// The browser to read logins from (default: the one logged in to the
        /// most of YouTube, TikTok and Instagram).
        #[arg(long, value_enum)]
        browser: Option<Browser>,
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

/// How long a list sits out after the platform tells us to slow down. It
/// doubles each time that happens again before a read gets through, up to
/// `MAX_PAUSE`: Instagram's blocks last hours to a day, and asking during
/// one only makes it last longer, or gets the account flagged.
const FIRST_PAUSE: Duration = Duration::from_hours(3);
const MAX_PAUSE: Duration = Duration::from_hours(24);

/// How often the app updates its own yt-dlp. Fixes for site changes ship in
/// yt-dlp releases every week or two.
const UPDATE_YT_DLP_EVERY: Duration = Duration::from_hours(24);

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
/// the archive. A dev binary copied elsewhere counts as an install. A macOS
/// app bundle can't hold them, so it uses Application Support (`desktop.rs`).
fn app_dir() -> Result<PathBuf> {
    if desktop::bundle().is_some() {
        return desktop::bundled_data_dir();
    }
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

/// The downloads folder: beside the database, or `~/Movies/Joyscroller` for
/// a macOS app bundle, where Finder users look for videos.
fn default_media() -> Result<PathBuf> {
    if desktop::bundle().is_some() {
        return desktop::bundled_media_dir();
    }
    Ok(app_dir()?.join("downloads"))
}

/// How long quitting waits for work in blocking threads (a tool download)
/// before exiting anyway.
const QUIT_GRACE: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(code) = desktop::run_logged_if_bundled(cli.command.is_none())? {
        std::process::exit(code);
    }
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let result = runtime.block_on(run(cli));
    // Shutting the runtime down drops every task, which kills the yt-dlp and
    // ffmpeg processes they started (`kill_on_drop`) rather than leaving them
    // running after the app has quit.
    runtime.shutdown_timeout(QUIT_GRACE);
    result
}

async fn run(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        return run_app("Default", &default_db()?).await;
    };
    // Only the subcommands that read logins ask for a browser.
    let sniff = |browser: Option<Browser>, profile: &str| {
        browser.unwrap_or_else(|| browser::sniff(profile))
    };
    match command {
        Command::Cookies {
            browser,
            profile,
            domain,
            out,
        } => {
            let browser = sniff(browser, &profile);
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
        } => {
            let browser = sniff(browser, &profile);
            match platform {
                Platform::Youtube => print_youtube_likes(browser, &profile, limit).await?,
                Platform::Tiktok => print_tiktok_likes(browser, &profile, limit).await?,
                Platform::Instagram => print_instagram_lists(browser, &profile, limit).await?,
            }
        }
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
            let browser = sniff(browser, &profile);
            let mut pauses = Pauses::default();
            let (listed, added) =
                sync(platform, browser, &profile, &mut conn, full, &mut pauses).await?;
            eprintln!(
                "{}: listed {listed}, added {added} new → {}",
                platform.name(),
                db_path.display()
            );
        }
        Command::Tools => {
            tools::ensure_yt_dlp().await?;
            tools::ensure_helpers().await?;
            tools::headless_chrome().await?;
            print!("{}", tools::versions().await?);
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
            let browser = sniff(browser, &profile);
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
    let mut videos = Vec::new();
    tiktok::liked_videos(browser, &cookies, None, |page, _| {
        Ok(collect_up_to(&mut videos, page, limit))
    })
    .await?;
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

/// The `likes` command for Instagram, where the lists are the saved posts
/// and the liked ones, `limit` of each.
async fn print_instagram_lists(browser: Browser, profile: &str, limit: usize) -> Result<()> {
    let cookies = browser::cookies_for(browser, profile, "instagram.com").await?;
    eprintln!("extracted {} instagram.com cookies", cookies.len());
    for (name, list) in [
        ("saved", instagram::List::Saved),
        ("liked", instagram::List::Liked),
    ] {
        let mut posts = Vec::new();
        instagram::posts(list, browser, &cookies, None, |page, _| {
            Ok(collect_up_to(&mut posts, page, limit))
        })
        .await?;
        print_instagram_posts(name, &posts);
    }
    Ok(())
}

/// Add a listed page to `items`, up to `limit` of them (0 for no limit).
/// Returns whether to list more.
fn collect_up_to<T>(items: &mut Vec<T>, page: Vec<T>, limit: usize) -> bool {
    items.extend(page);
    if limit > 0 && items.len() >= limit {
        items.truncate(limit);
        return false;
    }
    true
}

fn print_instagram_posts(list: &str, posts: &[instagram::Post]) {
    eprintln!("\n{} {list} posts:\n", posts.len());
    for (i, p) in posts.iter().enumerate() {
        let length = if p.is_photo {
            "photo".into()
        } else {
            p.duration_seconds
                .map_or_else(|| "--:--".into(), |s| format!("{}:{:02}", s / 60, s % 60))
        };
        let author = p.author_handle.as_deref().unwrap_or("?");
        // Captions run long with hashtags; the first line is enough here.
        let caption = p.caption.lines().next().unwrap_or("");
        println!("{:>3}. [{length}] @{author} — {caption}", i + 1);
        println!("     {}", p.url());
    }
}

/// The app: serves the web UI and keeps running, syncing new likes every
/// `SYNC_EVERY`. The pipeline's lanes download every liked video in the
/// background, and the UI can ask it for one right away.
async fn run_app(profile: &str, db_path: &Path) -> Result<()> {
    let url = format!("http://{}", web::ADDR);
    // Bind before anything else: a second copy of the app stops here instead
    // of syncing alongside the first, and shows the first one's UI instead,
    // which is what launching the app again means on a desktop.
    let listener = match tokio::net::TcpListener::bind(web::ADDR).await {
        Ok(listener) => listener,
        Err(e) if desktop::already_running(web::ADDR).await => {
            eprintln!("joyscroller is already running ({e}); opening {url}");
            desktop::open_in_browser(&url);
            return Ok(());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("listening on {}", web::ADDR));
        }
    };
    // Picked on every launch, which takes a few milliseconds, so switching
    // browsers only takes a restart.
    let browser = browser::sniff(profile);
    let mut conn = db::open(db_path)?;
    let (status, status_rx) = tokio::sync::watch::channel(web::SyncStatus::default());
    // Set by the UI's Turn off button.
    let (quit, mut quitting) = tokio::sync::watch::channel(false);
    // Made up front, so the folder is there to find before anything downloads.
    let media_root = default_media()?;
    std::fs::create_dir_all(&media_root)
        .with_context(|| format!("creating {}", media_root.display()))?;
    let pipeline = pipeline::Pipeline::start(
        &[
            Platform::Youtube.name(),
            Platform::Tiktok.name(),
            Platform::Instagram.name(),
        ],
        browser,
        profile,
        db_path,
        &media_root,
    );
    let web_conn = db::open(db_path)?;
    let web_media = media_root.clone();
    tokio::spawn(async move {
        if let Err(e) = web::serve(listener, web_conn, status_rx, pipeline, web_media, quit).await {
            eprintln!("web: stopped: {e:#}");
        }
    });
    eprintln!(
        "joyscroller v{}: open {url}, syncing into {}, downloading into {} (Ctrl-C to stop)",
        env!("CARGO_PKG_VERSION"),
        db_path.display(),
        media_root.display()
    );
    if desktop::opens_ui_on_launch() {
        desktop::open_in_browser(&url);
    }

    // Only downloads need ffmpeg and bun, so fetching them (ffmpeg is large)
    // doesn't hold up the first sync.
    tokio::spawn(async {
        while let Err(e) = tools::ensure_helpers().await {
            eprintln!("tools: {e:#}");
            tokio::time::sleep(SYNC_EVERY).await;
        }
    });
    // Each platform syncs on its own schedule, with its own connection (WAL
    // lets them write in turn), so a slow one, such as a first full read of
    // a long list, doesn't hold up the others.
    let mut tiktok_conn = db::open(db_path)?;
    let mut instagram_conn = db::open(db_path)?;
    // Turning off from the UI ends the loops at once, mid-sync too: dropping
    // a sync stops its yt-dlp or headless browser, and the next launch syncs
    // again from where the database left off.
    let run = async {
        // Boxed: each sync's future holds a whole listing's state, and three
        // side by side would make this function's future very large.
        let youtube = Box::pin(sync_loop(
            Platform::Youtube,
            browser,
            profile,
            &mut conn,
            &status,
        ));
        let tiktok = Box::pin(sync_loop(
            Platform::Tiktok,
            browser,
            profile,
            &mut tiktok_conn,
            &status,
        ));
        let instagram = Box::pin(sync_loop(
            Platform::Instagram,
            browser,
            profile,
            &mut instagram_conn,
            &status,
        ));
        tokio::join!(youtube, tiktok, instagram);
    };
    tokio::select! {
        () = run => {}
        _ = quitting.wait_for(|quit| *quit) => {}
    }
    eprintln!("joyscroller: turned off from the UI");
    Ok(())
}

/// One platform's syncs: one right away, then every `SYNC_EVERY`. If a sync
/// overruns the interval, the next waits a full interval after it rather
/// than firing straight away.
async fn sync_loop(
    platform: Platform,
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    status: &tokio::sync::watch::Sender<web::SyncStatus>,
) {
    let mut ticks = tokio::time::interval(SYNC_EVERY);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut yt_dlp_updated: Option<tokio::time::Instant> = None;
    let mut pauses = Pauses::default();
    loop {
        ticks.tick().await;
        if matches!(platform, Platform::Youtube) {
            prepare_yt_dlp(&mut yt_dlp_updated).await;
        }
        sync_and_report(platform, browser, profile, conn, status, &mut pauses).await;
    }
}

/// Make sure yt-dlp is here, and current once a day. Listing YouTube likes
/// needs it. If fetching it fails (offline), the YouTube sync fails too, and
/// both are tried again next time.
async fn prepare_yt_dlp(updated: &mut Option<tokio::time::Instant>) {
    if let Err(e) = tools::ensure_yt_dlp().await {
        eprintln!("tools: {e:#}");
    }
    if updated.is_none_or(|at| at.elapsed() >= UPDATE_YT_DLP_EVERY) {
        match tools::update_yt_dlp().await {
            Ok(()) => *updated = Some(tokio::time::Instant::now()),
            Err(e) => eprintln!("tools: {e:#}"),
        }
    }
}

/// Pull new likes on `platform`, telling the UI as it goes. A failed sync
/// (offline, logged out) is logged, not fatal: the next one tries again.
async fn sync_and_report(
    platform: Platform,
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    status: &tokio::sync::watch::Sender<web::SyncStatus>,
    pauses: &mut Pauses,
) {
    let name = platform.name();
    status.send_modify(|s| s.set(name, web::SyncState::Running));
    let result = sync(platform, browser, profile, conn, false, pauses).await;
    let at = db::now();
    let paused = pauses.current(at);
    let state = match result {
        Ok((listed, added)) => {
            eprintln!("{name}: listed {listed}, added {added} new");
            web::SyncState::Done { at, added, paused }
        }
        Err(e) => {
            eprintln!("{name}: sync failed: {e:#}");
            web::SyncState::Failed {
                at,
                advice: advice(platform, browser, &e),
                error: format!("{e:#}"),
                paused,
            }
        }
    };
    status.send_modify(|s| s.set(name, state));
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
        // A video the platform says is gone is no sign of trouble.
        if result.as_ref().is_ok() || result.as_ref().is_err_and(download::is_unavailable) {
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
    pauses: &mut Pauses,
) -> Result<(usize, usize)> {
    match platform {
        Platform::Youtube => sync_youtube(browser, profile, conn, full).await,
        Platform::Tiktok => sync_tiktok(browser, profile, conn, full).await,
        Platform::Instagram => sync_instagram(browser, profile, conn, full, pauses).await,
    }
}

/// What the UI says about a failed sync, in words for the user: what to do
/// about it where there's something to do, else that it'll try again. The
/// error itself is there for whoever wants the details.
fn advice(platform: Platform, browser: Browser, e: &anyhow::Error) -> String {
    let site = platform.label();
    let browser = browser.name();
    if let Some(e) = e.downcast_ref::<browser::NoFullDiskAccess>() {
        return e.to_string();
    }
    if e.downcast_ref::<browser::LoggedOut>().is_some()
        || instagram::refusal(e) == Some(instagram::Refusal::LoggedOut)
    {
        return format!(
            "you're not logged in to {site} in {browser}. Log in there and the next sync picks it up"
        );
    }
    if instagram::refusal(e) == Some(instagram::Refusal::Checkpoint) {
        return format!(
            "Instagram wants to check it's you. Open instagram.com in {browser} and do what it asks"
        );
    }
    format!("trying again in {} minutes", SYNC_EVERY.as_secs() / 60)
}

/// The lists a platform has told us to slow down on, by listing name, each
/// skipped until its pause is over. They last as long as the run: a restart
/// asks once more.
#[derive(Default)]
struct Pauses(HashMap<&'static str, Pause>);

struct Pause {
    /// When the list may be read again (unix seconds).
    until: i64,
    /// Slow-downs since a read of the list last got through.
    in_a_row: u32,
}

impl Pauses {
    /// When `list` may be read again, if it's sitting out at `now`.
    fn until(&self, list: &str, now: i64) -> Option<i64> {
        self.0
            .get(list)
            .map(|p| p.until)
            .filter(|&until| until > now)
    }

    /// Note how a read of `list` went: being told to slow down starts its
    /// pause, or a longer one, and a read that gets through ends it.
    fn record(&mut self, list: &'static str, result: &Result<()>, now: i64) {
        match result {
            Ok(()) => {
                self.0.remove(list);
            }
            Err(e) if instagram::is_throttled(e) => {
                let in_a_row = self.0.get(list).map_or(0, |p| p.in_a_row) + 1;
                let pause = FIRST_PAUSE
                    .saturating_mul(2_u32.saturating_pow(in_a_row - 1))
                    .min(MAX_PAUSE);
                let until = now.saturating_add(i64::try_from(pause.as_secs()).unwrap_or(i64::MAX));
                eprintln!(
                    "{list}: told to slow down, pausing it for {}h",
                    pause.as_secs() / 3600
                );
                self.0.insert(list, Pause { until, in_a_row });
            }
            // Other failures say nothing about our pace.
            Err(_) => {}
        }
    }

    /// The lists sitting out at `now`, for the UI.
    fn current(&self, now: i64) -> Vec<web::Paused> {
        let mut paused: Vec<web::Paused> = self
            .0
            .iter()
            .filter(|(_, p)| p.until > now)
            .map(|(&list, p)| web::Paused {
                // "instagram liked" is the liked list.
                list: list.split_once(' ').map_or(list, |(_, which)| which),
                until: p.until,
            })
            .collect();
        paused.sort_by_key(|p| p.list);
        paused
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
    // Stored as listed, newest first, all at once: yt-dlp can't carry on
    // from where a listing was cut short, and YouTube's is quick to read again.
    let rows: Vec<db::NewVideo> = videos
        .iter()
        .zip(&urls)
        .zip(&raws)
        .map(|((v, url), raw)| db::NewVideo {
            video_id: &v.id,
            url,
            title: v.available().then_some(v.title.as_str()),
            author_id: v.channel_id.as_deref(),
            author_name: v.channel.as_deref(),
            duration_seconds: v.duration_seconds,
            available: v.available(),
            listing_json: raw,
            kind: "video",
            // YouTube's liked list doesn't say; the download fills it in.
            posted_at: None,
        })
        .collect();
    let added = db::insert_new(conn, "youtube", &rows)?;
    db::finish_sync(conn, sync_id, added)?;
    Ok((videos.len(), added))
}

/// The lists read page by page through a listing (`db/listings.rs`), as
/// `listings` and `full_syncs_due` name them.
const TIKTOK_LIKED: &str = "tiktok liked";
const INSTAGRAM_SAVED: &str = "instagram saved";
const INSTAGRAM_LIKED: &str = "instagram liked";

/// The part of a listed page to store: up to and including the item where
/// `keep_going` says to stop. Also returns whether to go on.
fn until_stop<T>(items: Vec<T>, mut keep_going: impl FnMut(&T) -> bool) -> (Vec<T>, bool) {
    let mut kept = Vec::with_capacity(items.len());
    for item in items {
        let go_on = keep_going(&item);
        kept.push(item);
        if !go_on {
            return (kept, false);
        }
    }
    (kept, true)
}

/// Pull new TikTok likes into the database. Returns (listed, added). Each
/// page is stored as it comes, and a listing cut short carries on where it
/// got to on the next sync.
async fn sync_tiktok(
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    full: bool,
) -> Result<(usize, usize)> {
    let sync_id = db::start_sync(conn, "tiktok")?;

    // Re-read cookies on every sync: the browser rotates them while it runs.
    let cookies = browser::cookies_for(browser, profile, "tiktok.com").await?;
    let mut listing = db::open_listing(conn, TIKTOK_LIKED, full)?;
    let known = db::video_ids(conn, "tiktok")?;
    let mut keep_going = until_known(&known, listing.full);
    let (mut listed, mut added) = (0, 0);
    let cursor = listing.cursor.clone();
    let read = tiktok::liked_videos(browser, &cookies, cursor.as_deref(), |videos, next| {
        let (videos, go_on) = until_stop(videos, |v| keep_going(&v.id));
        listed += videos.len();
        added += store_tiktok_page(conn, &mut listing, &videos, next)?;
        Ok(go_on)
    })
    .await;
    if read.is_ok() {
        db::finish_listing(conn, &listing)?;
    }
    db::finish_sync(conn, sync_id, added)?;
    read?;
    Ok((listed, added))
}

/// Store a page of TikTok likes through `listing`. Returns how many were new.
fn store_tiktok_page(
    conn: &mut rusqlite::Connection,
    listing: &mut db::Listing,
    videos: &[tiktok::Video],
    next: Option<&str>,
) -> Result<usize> {
    let urls: Vec<String> = videos.iter().map(tiktok::Video::url).collect();
    let raws: Vec<String> = videos.iter().map(|v| v.raw.to_string()).collect();
    let rows: Vec<db::NewVideo> = videos
        .iter()
        .zip(&urls)
        .zip(&raws)
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
            listing_json: raw,
            kind: if v.is_photo { "photo" } else { "video" },
            posted_at: v.posted_at,
        })
        .collect();
    db::store_page(conn, "tiktok", listing, &rows, next)
}

/// Pull newly saved and liked Instagram posts into the database. Returns
/// (listed, added). Instagram has two ways of keeping a post, saving it and
/// liking it, and the archive holds both, as one row per post.
///
/// The two lists are read one after the other, each stored page by page,
/// so one failing (the liked list is read from the phone app's API, which
/// Instagram could close to us) doesn't hold up the other.
async fn sync_instagram(
    browser: Browser,
    profile: &str,
    conn: &mut rusqlite::Connection,
    full: bool,
    pauses: &mut Pauses,
) -> Result<(usize, usize)> {
    // Re-read cookies on every sync: the browser rotates them while it runs.
    let cookies = browser::cookies_for(browser, profile, "instagram.com").await?;
    instagram::check_logged_in(&cookies)?;
    let sync_id = db::start_sync(conn, "instagram")?;
    let (mut listed, mut added) = (0, 0);
    let saved = sync_instagram_list(
        instagram::List::Saved,
        browser,
        &cookies,
        conn,
        full,
        (&mut listed, &mut added),
        pauses,
    )
    .await;
    let liked = sync_instagram_list(
        instagram::List::Liked,
        browser,
        &cookies,
        conn,
        full,
        (&mut listed, &mut added),
        pauses,
    )
    .await;
    db::finish_sync(conn, sync_id, added)?;
    match (saved, liked) {
        (Ok(()), Ok(())) => Ok((listed, added)),
        (Err(e), Ok(())) => Err(e.context("reading the saved list")),
        (Ok(()), Err(e)) => Err(e.context("reading the liked list")),
        (Err(saved), Err(liked)) => Err(saved.context(format!(
            "reading the saved list (and the liked list: {liked:#})"
        ))),
    }
}

/// Read one of Instagram's lists into the database, page by page, adding to
/// `counts` (listed, added) as it goes, unless Instagram told us to slow
/// down on it and its pause isn't over. A list in `full_syncs_due` is read
/// in full, and taken off once a read gets to its end.
async fn sync_instagram_list(
    list: instagram::List,
    browser: Browser,
    cookies: &[browser::Cookie],
    conn: &mut rusqlite::Connection,
    full: bool,
    counts: (&mut usize, &mut usize),
    pauses: &mut Pauses,
) -> Result<()> {
    let name = match list {
        instagram::List::Saved => INSTAGRAM_SAVED,
        instagram::List::Liked => INSTAGRAM_LIKED,
    };
    if pauses.until(name, db::now()).is_some() {
        return Ok(());
    }
    let result = read_instagram_list(list, name, browser, cookies, conn, full, counts).await;
    pauses.record(name, &result, db::now());
    // Being told to slow down isn't a failure: the list sits out its pause,
    // which the UI shows, and the sync carries on.
    match result {
        Err(e) if instagram::is_throttled(&e) => Ok(()),
        result => result,
    }
}

/// Read Instagram's `list` (the listing `name`) into the database.
async fn read_instagram_list(
    list: instagram::List,
    name: &str,
    browser: Browser,
    cookies: &[browser::Cookie],
    conn: &mut rusqlite::Connection,
    full: bool,
    counts: (&mut usize, &mut usize),
) -> Result<()> {
    let due = db::full_sync_due(conn, name)?;
    let mut listing = db::open_listing(conn, name, full || due)?;
    let known = db::video_ids(conn, "instagram")?;
    let mut keep_going = until_known(&known, listing.full);
    let (listed, added) = counts;
    let cursor = listing.cursor.clone();
    instagram::posts(list, browser, cookies, cursor.as_deref(), |posts, next| {
        let (posts, go_on) = until_stop(posts, |p| keep_going(&p.code));
        *listed += posts.len();
        *added += store_instagram_page(conn, &mut listing, &posts, next)?;
        Ok(go_on)
    })
    .await?;
    db::finish_listing(conn, &listing)?;
    if due {
        db::full_sync_done(conn, name)?;
    }
    Ok(())
}

/// Store a page of Instagram posts through `listing`. Returns how many were new.
fn store_instagram_page(
    conn: &mut rusqlite::Connection,
    listing: &mut db::Listing,
    posts: &[instagram::Post],
    next: Option<&str>,
) -> Result<usize> {
    let urls: Vec<String> = posts.iter().map(instagram::Post::url).collect();
    let raws: Vec<String> = posts.iter().map(|p| p.raw.to_string()).collect();
    let rows: Vec<db::NewVideo> = posts
        .iter()
        .zip(&urls)
        .zip(&raws)
        .map(|((p, url), raw)| db::NewVideo {
            video_id: &p.code,
            url,
            title: (!p.caption.is_empty()).then_some(p.caption.as_str()),
            author_id: p.author_id.as_deref(),
            author_name: p.author_handle.as_deref(),
            duration_seconds: p.duration_seconds,
            // Like TikTok, Instagram leaves deleted and private posts out of
            // its lists.
            available: true,
            listing_json: raw,
            kind: if p.is_photo { "photo" } else { "video" },
            posted_at: p.posted_at,
        })
        .collect();
    db::store_page(conn, "instagram", listing, &rows, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_syncs_are_explained_in_plain_words() {
        let logged_out = anyhow::Error::from(browser::LoggedOut("TikTok")).context("listing");
        assert_eq!(
            advice(Platform::Tiktok, Browser::Brave, &logged_out),
            "you're not logged in to TikTok in Brave. Log in there and the next sync picks it up"
        );
        let other = anyhow::anyhow!("yt-dlp produced no videos").context("listing");
        assert_eq!(
            advice(Platform::Youtube, Browser::Chrome, &other),
            "trying again in 5 minutes"
        );
    }

    #[test]
    fn a_list_told_to_slow_down_sits_out_longer_each_time() {
        let throttled = || -> Result<()> {
            Err(instagram::slow_down_for_tests().context("reading the liked list"))
        };
        let mut pauses = Pauses::default();
        let hour = 3600;
        pauses.record(INSTAGRAM_LIKED, &throttled(), 0);
        assert_eq!(pauses.until(INSTAGRAM_LIKED, 0), Some(3 * hour));
        assert_eq!(pauses.until(INSTAGRAM_SAVED, 0), None);
        assert_eq!(pauses.until(INSTAGRAM_LIKED, 3 * hour), None);
        let paused = pauses.current(hour);
        assert_eq!(paused.len(), 1);
        assert_eq!((paused[0].list, paused[0].until), ("liked", 3 * hour));
        pauses.record(INSTAGRAM_LIKED, &throttled(), 3 * hour);
        assert_eq!(pauses.until(INSTAGRAM_LIKED, 3 * hour), Some(9 * hour));
        pauses.record(INSTAGRAM_LIKED, &throttled(), 9 * hour);
        pauses.record(INSTAGRAM_LIKED, &throttled(), 21 * hour);
        assert_eq!(pauses.until(INSTAGRAM_LIKED, 21 * hour), Some(45 * hour));
        // Other failures neither start nor end a pause.
        pauses.record(INSTAGRAM_LIKED, &Err(anyhow::anyhow!("offline")), 45 * hour);
        assert_eq!(pauses.0[INSTAGRAM_LIKED].in_a_row, 4);
        pauses.record(INSTAGRAM_SAVED, &Err(anyhow::anyhow!("offline")), 0);
        assert!(!pauses.0.contains_key(INSTAGRAM_SAVED));
        pauses.record(INSTAGRAM_LIKED, &Ok(()), 45 * hour);
        assert!(pauses.current(0).is_empty());
    }

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
    fn a_page_is_kept_up_to_where_the_listing_stops() {
        let (kept, go_on) = until_stop(vec![1, 2, 3, 4], |&n| n != 2);
        assert_eq!((kept, go_on), (vec![1, 2], false));
        let (kept, go_on) = until_stop(vec![1, 3], |&n| n != 2);
        assert_eq!((kept, go_on), (vec![1, 3], true));
    }

    #[test]
    fn collects_up_to_the_limit() {
        let mut items = Vec::new();
        assert!(collect_up_to(&mut items, vec![1, 2], 3));
        assert!(!collect_up_to(&mut items, vec![3, 4], 3));
        assert_eq!(items, [1, 2, 3]);
        assert!(collect_up_to(&mut items, vec![5], 0));
    }

    #[test]
    fn full_never_stops() {
        let known: HashSet<String> = (0..STOP_AFTER_KNOWN).map(|i| i.to_string()).collect();
        let mut keep_going = until_known(&known, true);
        assert!(known.iter().all(|id| keep_going(id)));
    }
}
