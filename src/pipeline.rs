//! The download pipeline. Every platform has a lane (unless `AUTOMATIC` is
//! off) that works through its backlog on its own: each liked video that isn't
//! downloaded yet, newest first, one at a time, pausing between videos (`pace`) so a backlog of
//! thousands doesn't look like a bot to the platform. A video someone asks
//! for in the UI doesn't wait for its lane: it downloads right away, on a
//! track of its own, alongside the lanes and any other requested videos.
//!
//! A video goes through each `Stage` in turn; for now the only one is the
//! download itself. What the lanes and tracks are doing is kept on a `Board`,
//! which the web UI watches. Nothing about the pipeline is stored: the lanes
//! pick up from the database when the app starts again, and a requested
//! download that was cut off needs asking for again.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::watch;

use crate::browser::Browser;
use crate::db::{self, PendingDownload};
use crate::download::{self, Credentials, Progress, Saved};

/// Whether the lanes download the backlog, every liked video (thousands of
/// them) with the user's logins. When off, videos download only when asked
/// for in the UI.
const AUTOMATIC: bool = true;

/// How often a lane checks whether the tools it downloads with have arrived.
const TOOLS_RETRY: Duration = Duration::from_secs(10);

/// The pause between a lane's videos, as the shortest and longest: a random
/// length in between, so the requests don't come at a machine's regular beat.
/// yt-dlp's wiki puts YouTube's limit for a logged-in account at about 2,000
/// videos an hour; 30 s keeps us at about 120. TikTok publishes no limits.
/// Neither does Instagram, but it's the quickest of the three to lock an
/// account out for "suspicious activity", so it gets the slowest pace.
fn pace(platform: &str) -> (Duration, Duration) {
    match platform {
        "tiktok" => (Duration::from_secs(15), Duration::from_secs(30)),
        "instagram" => (Duration::from_secs(30), Duration::from_secs(60)),
        _ => (Duration::from_secs(30), Duration::from_secs(30)),
    }
}

/// The pause after a lane's download fails, which is often a sign of rate
/// limiting. The video itself waits out its own retry delay in the database.
const FAILURE_PAUSE: Duration = Duration::from_mins(1);

/// This many failed downloads in a row looks like a problem on our side or the
/// platform's (yt-dlp broken, login expired, offline, rate limited) rather than
/// with the videos, so the lane stops for `OUTAGE_PAUSE` instead of failing
/// its way through the backlog.
const FAILURES_BEFORE_OUTAGE: usize = 3;
const OUTAGE_PAUSE: Duration = Duration::from_mins(30);

/// How long a lane with nothing to download waits before looking again. New
/// likes arrive with the syncs, every few minutes.
const IDLE: Duration = Duration::from_mins(1);

/// How many of the backlog's next videos a lane looks at, to find one that
/// isn't already downloading on a requested track.
const LOOKAHEAD: usize = 10;

/// How long a lane waits before trying to open the database again.
const DB_RETRY: Duration = Duration::from_mins(1);

/// How many finished videos the board remembers.
const RECENT: usize = 20;

/// A step a video goes through, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Download,
}

impl Stage {
    /// What a video in this stage is doing, for the UI.
    pub fn doing(self) -> &'static str {
        match self {
            Stage::Download => "downloading",
        }
    }
}

/// A video being downloaded.
#[derive(Debug, Clone)]
pub struct Job {
    pub item: PendingDownload,
    /// How the UI names it.
    pub title: String,
}

impl Job {
    /// A job for `item`, named after the first line of its title, or else its
    /// author or id.
    pub fn new(item: PendingDownload) -> Job {
        let title = item
            .title
            .as_deref()
            .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
            .or(item.author_name.as_deref())
            .unwrap_or(&item.video_id)
            .to_string();
        Job { item, title }
    }

    fn is(&self, platform: &str, video_id: &str) -> bool {
        self.item.platform == platform && self.item.video_id == video_id
    }
}

/// A video being worked on.
#[derive(Debug, Clone)]
pub struct Active {
    pub job: Job,
    /// Asked for in the UI, so on a track of its own; otherwise its lane's.
    pub requested: bool,
    pub stage: Stage,
    pub started_at: i64,
    /// yt-dlp's latest progress report, once there is one.
    pub progress: Option<Progress>,
}

/// Why a lane is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// The pause between videos.
    Pace,
    /// The pause after a failed download.
    Failed,
    /// The longer pause after `FAILURES_BEFORE_OUTAGE` failures in a row.
    Outage,
    /// Nothing to download; it looks again then.
    Empty,
}

/// What a lane is doing when it isn't downloading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub platform: &'static str,
    /// Videos left in its backlog, counting the one it's on, as of the last
    /// time it looked. None until it first looks.
    pub left: Option<usize>,
    /// Why it's waiting, and until when.
    pub waiting: Option<(Wait, i64)>,
}

/// A video that came out of the pipeline, either way.
#[derive(Debug, Clone)]
pub struct Finished {
    pub job: Job,
    pub at: i64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Downloaded { bytes: u64, seconds: u64 },
    Failed(String),
}

/// What the pipeline is doing.
#[derive(Debug, Clone, Default)]
pub struct Board {
    /// One per platform.
    pub lanes: Vec<Lane>,
    /// At most one video per lane, plus every requested one.
    pub active: Vec<Active>,
    /// Since the app started, newest first, up to `RECENT`.
    pub recent: VecDeque<Finished>,
}

impl Board {
    /// The video `platform`'s lane is downloading.
    pub fn lane_download(&self, platform: &str) -> Option<&Active> {
        self.active
            .iter()
            .find(|a| !a.requested && a.job.item.platform == platform)
    }

    pub fn find_active(&self, platform: &str, video_id: &str) -> Option<&Active> {
        self.active.iter().find(|a| a.job.is(platform, video_id))
    }

    fn lane_mut(&mut self, platform: &str) -> Option<&mut Lane> {
        self.lanes.iter_mut().find(|l| l.platform == platform)
    }

    fn start(&mut self, job: &Job, requested: bool) {
        self.active.push(Active {
            job: job.clone(),
            requested,
            stage: Stage::Download,
            started_at: db::now(),
            progress: None,
        });
    }

    fn progress(&mut self, job: &Job, progress: Progress) {
        if let Some(active) = self
            .active
            .iter_mut()
            .find(|a| a.job.is(&job.item.platform, &job.item.video_id))
        {
            active.progress = Some(progress);
        }
    }

    fn finish(&mut self, job: &Job, outcome: Outcome) {
        self.active
            .retain(|a| !a.job.is(&job.item.platform, &job.item.video_id));
        self.recent.push_front(Finished {
            job: job.clone(),
            at: db::now(),
            outcome,
        });
        self.recent.truncate(RECENT);
    }
}

/// What every lane and track needs to download.
struct Shared {
    browser: Browser,
    profile: String,
    db_path: PathBuf,
    media_root: PathBuf,
    board: watch::Sender<Board>,
}

/// The handle the web UI asks for downloads through.
#[derive(Clone)]
pub struct Pipeline {
    platforms: Vec<&'static str>,
    shared: Arc<Shared>,
}

impl Pipeline {
    /// Start a lane for each of `platforms`, if `AUTOMATIC`. Call it before
    /// anything else downloads: it clears what interrupted downloads left
    /// behind.
    pub fn start(
        platforms: &[&'static str],
        browser: Browser,
        profile: &str,
        db_path: &Path,
        media_root: &Path,
    ) -> Pipeline {
        if let Err(e) = download::sweep_leftovers(media_root) {
            eprintln!("download: couldn't clear interrupted downloads: {e:#}");
        }
        let pipeline = Pipeline::new(
            platforms,
            AUTOMATIC,
            Shared {
                browser,
                profile: profile.to_owned(),
                db_path: db_path.to_owned(),
                media_root: media_root.to_owned(),
                board: watch::Sender::new(Board::default()),
            },
        );
        if AUTOMATIC {
            for &platform in platforms {
                tokio::spawn(run_lane(platform, Arc::clone(&pipeline.shared)));
            }
        }
        pipeline
    }

    /// A pipeline for `platforms`, with nothing running yet. With `lanes`,
    /// the board has an empty lane for each; without, none, so the UI says
    /// automatic downloads are off.
    fn new(platforms: &[&'static str], lanes: bool, shared: Shared) -> Pipeline {
        if lanes {
            shared.board.send_modify(|board| {
                board.lanes = platforms
                    .iter()
                    .map(|&platform| Lane {
                        platform,
                        left: None,
                        waiting: None,
                    })
                    .collect();
            });
        }
        Pipeline {
            platforms: platforms.to_vec(),
            shared: Arc::new(shared),
        }
    }

    /// Download `job` now, on a track of its own. False if it's already
    /// downloading, on its lane or because it was asked for already.
    pub fn request(&self, job: Job) -> Result<bool> {
        if !self.claim(&job)? {
            return Ok(false);
        }
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            match db::open(&shared.db_path) {
                Ok(mut conn) => {
                    download_job(&shared, &mut conn, &job).await;
                }
                Err(e) => {
                    let error = format!("can't open the database: {e:#}");
                    shared
                        .board
                        .send_modify(|board| board.finish(&job, Outcome::Failed(error)));
                }
            }
        });
        Ok(true)
    }

    /// Put `job` on the board as a requested download, unless it's there
    /// already. True if it was added.
    fn claim(&self, job: &Job) -> Result<bool> {
        let platform = &job.item.platform;
        if !self.platforms.contains(&platform.as_str()) {
            anyhow::bail!("can't download from platform {platform}");
        }
        Ok(self.shared.board.send_if_modified(|board| {
            let known = board.find_active(platform, &job.item.video_id).is_some();
            if !known {
                board.start(job, true);
            }
            !known
        }))
    }

    pub fn board(&self) -> Board {
        self.shared.board.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Board> {
        self.shared.board.subscribe()
    }
}

/// A platform's lane: download its backlog forever, one video at a time,
/// pausing between videos. Failures are recorded and logged; nothing stops it.
async fn run_lane(platform: &'static str, shared: Arc<Shared>) {
    // Its own connection, so a lane's writes don't wait on the others' or the
    // syncs' longer than a single write.
    let mut conn = loop {
        match db::open(&shared.db_path) {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("{platform}: can't open the database: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    };
    // On first launch the app is still fetching yt-dlp and ffmpeg. A lane that
    // started without them would fail its first videos and push each one back
    // an hour.
    loop {
        match crate::tools::ready() {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => eprintln!("{platform}: can't check for the download tools: {e:#}"),
        }
        tokio::time::sleep(TOOLS_RETRY).await;
    }
    let mut failures_in_a_row = 0;
    loop {
        let job = match next_job(platform, &shared, &conn) {
            Ok(Some(job)) => job,
            Ok(None) => {
                wait(platform, &shared, Wait::Empty, IDLE).await;
                continue;
            }
            Err(e) => {
                eprintln!("{platform}: can't read the download queue: {e:#}");
                wait(platform, &shared, Wait::Failed, DB_RETRY).await;
                continue;
            }
        };
        if download_job(&shared, &mut conn, &job).await {
            failures_in_a_row = 0;
            let (shortest, longest) = pace(platform);
            wait(platform, &shared, Wait::Pace, between(shortest, longest)).await;
        } else {
            failures_in_a_row += 1;
            if failures_in_a_row >= FAILURES_BEFORE_OUTAGE {
                eprintln!(
                    "{platform}: {failures_in_a_row} downloads failed in a row; \
                     pausing the lane for {} min",
                    OUTAGE_PAUSE.as_secs() / 60
                );
                failures_in_a_row = 0;
                wait(platform, &shared, Wait::Outage, OUTAGE_PAUSE).await;
            } else {
                wait(platform, &shared, Wait::Failed, FAILURE_PAUSE).await;
            }
        }
    }
}

/// The next video in `platform`'s backlog that isn't already downloading on
/// a requested track, put on the board as the lane's. Also updates the
/// lane's count of what's left.
fn next_job(platform: &'static str, shared: &Shared, conn: &Connection) -> Result<Option<Job>> {
    let left = db::pending_count(conn, platform)?;
    let candidates = db::pending_downloads(conn, Some(platform), LOOKAHEAD)?;
    let mut picked = None;
    shared.board.send_modify(|board| {
        if let Some(lane) = board.lane_mut(platform) {
            lane.left = Some(left);
        }
        // Checked and claimed in one go, so a request can't start the same
        // video in between.
        if let Some(item) = candidates
            .into_iter()
            .find(|c| board.find_active(&c.platform, &c.video_id).is_none())
        {
            let job = Job::new(item);
            board.start(&job, false);
            picked = Some(job);
        }
    });
    Ok(picked)
}

/// Sleep for `pause`, with the lane showing why and until when.
async fn wait(platform: &str, shared: &Shared, why: Wait, pause: Duration) {
    let until = db::now() + i64::try_from(pause.as_secs()).unwrap_or(i64::MAX);
    shared.board.send_modify(|board| {
        if let Some(lane) = board.lane_mut(platform) {
            lane.waiting = Some((why, until));
        }
    });
    tokio::time::sleep(pause).await;
    shared.board.send_modify(|board| {
        if let Some(lane) = board.lane_mut(platform) {
            lane.waiting = None;
        }
    });
}

/// A pause of random length from `shortest` to `longest`. The clock's
/// nanoseconds are random enough to vary a pause by a few seconds.
fn between(shortest: Duration, longest: Duration) -> Duration {
    let spread = longest.saturating_sub(shortest).as_millis();
    if spread == 0 {
        return shortest;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let extra = u64::try_from(u128::from(nanos) % (spread + 1)).unwrap_or(0);
    shortest + Duration::from_millis(extra)
}

/// Download a job that's on the board, record how it went, and take it off
/// the board. True if it was downloaded and recorded.
async fn download_job(shared: &Shared, conn: &mut Connection, job: &Job) -> bool {
    let started = Instant::now();
    let on_progress = |progress| {
        shared
            .board
            .send_modify(|board| board.progress(job, progress));
    };
    // Cookies are read for every video: downloads can be minutes or hours
    // apart, and a login can change in between.
    let result = async {
        let credentials =
            Credentials::read(shared.browser, &shared.profile, &job.item.platform).await?;
        download::download(
            &job.item,
            &credentials.login(),
            &shared.media_root,
            &on_progress,
        )
        .await
    }
    .await;
    let mut outcome = match &result {
        Ok(saved) => Outcome::Downloaded {
            bytes: saved.bytes,
            seconds: started.elapsed().as_secs(),
        },
        Err(e) => Outcome::Failed(format!("{e:#}")),
    };
    if let Err(e) = record(conn, &job.item, &result, started.elapsed()) {
        eprintln!(
            "{}: couldn't record the download of {}: {e:#}",
            job.item.platform, job.item.video_id
        );
        outcome = Outcome::Failed(format!("couldn't record it: {e:#}"));
    }
    let downloaded = matches!(outcome, Outcome::Downloaded { .. });
    shared.board.send_modify(|board| board.finish(job, outcome));
    downloaded
}

/// Store how a download went and log it. A failure counts as an attempt, so
/// the video waits out its retry delay before its lane
/// (`db::pending_downloads`) offers it again.
pub fn record(
    conn: &mut Connection,
    item: &PendingDownload,
    result: &Result<Saved>,
    took: Duration,
) -> Result<()> {
    let name = &item.platform;
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
            )
            .context("storing the download")?;
            #[expect(clippy::cast_precision_loss, reason = "a size for a log line")]
            let megabytes = saved.bytes as f64 / 1e6;
            eprintln!(
                "{name}: downloaded {} ({megabytes:.1} MB, format {}) in {}s",
                item.video_id,
                saved.format,
                took.as_secs()
            );
        }
        Err(e) => {
            db::record_download_failure(conn, &item.platform, &item.video_id, &format!("{e:#}"))?;
            eprintln!("{name}: download of {} failed: {e:#}", item.video_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(platform: &str, id: &str) -> PendingDownload {
        PendingDownload {
            platform: platform.into(),
            video_id: id.into(),
            url: "https://example.com".into(),
            kind: "video".into(),
            title: None,
            author_name: None,
            liked_on: "2026-10-12".into(),
        }
    }

    fn job(platform: &str, id: &str) -> Job {
        Job::new(item(platform, id))
    }

    fn pipeline() -> Pipeline {
        Pipeline::new(
            &["youtube", "tiktok"],
            true,
            Shared {
                browser: Browser::Brave,
                profile: "Default".into(),
                db_path: PathBuf::from("/nonexistent/joyscroller.db"),
                media_root: PathBuf::from("/nonexistent/downloads"),
                board: watch::Sender::new(Board::default()),
            },
        )
    }

    #[test]
    fn names_jobs_after_the_first_line_of_the_title() {
        let titled = PendingDownload {
            title: Some("\n  first\nsecond".into()),
            ..item("tiktok", "1")
        };
        assert_eq!(Job::new(titled).title, "first");
        let authored = PendingDownload {
            title: Some(" ".into()),
            author_name: Some("cat".into()),
            ..item("tiktok", "1")
        };
        assert_eq!(Job::new(authored).title, "cat");
        assert_eq!(job("tiktok", "1").title, "1");
    }

    #[test]
    fn board_tracks_lanes_and_requests() {
        let mut board = Board::default();
        board.start(&job("youtube", "a"), false);
        board.start(&job("youtube", "b"), true);
        assert_eq!(
            board.lane_download("youtube").unwrap().job.item.video_id,
            "a"
        );
        assert!(board.lane_download("tiktok").is_none());
        assert!(board.find_active("youtube", "b").unwrap().requested);

        board.progress(
            &job("youtube", "b"),
            Progress {
                downloaded: 1,
                total: None,
                speed: None,
                eta: None,
                format_id: "18".into(),
            },
        );
        assert!(
            board
                .find_active("youtube", "b")
                .unwrap()
                .progress
                .is_some()
        );

        board.finish(&job("youtube", "a"), Outcome::Failed("nope".into()));
        assert!(board.lane_download("youtube").is_none());
        assert_eq!(board.active.len(), 1);
        assert_eq!(board.recent.len(), 1);
    }

    #[test]
    fn board_keeps_only_the_latest_finished() {
        let mut board = Board::default();
        for i in 0..RECENT + 5 {
            board.finish(
                &job("tiktok", &i.to_string()),
                Outcome::Failed(String::new()),
            );
        }
        assert_eq!(board.recent.len(), RECENT);
        assert_eq!(board.recent[0].job.item.video_id, (RECENT + 4).to_string());
    }

    #[test]
    fn requests_start_once_and_alongside_the_lane() {
        let pipeline = pipeline();
        assert_eq!(pipeline.board().lanes.len(), 2);
        assert!(pipeline.claim(&job("youtube", "a")).unwrap());
        assert!(!pipeline.claim(&job("youtube", "a")).unwrap());
        assert!(pipeline.claim(&job("youtube", "b")).unwrap());
        assert_eq!(pipeline.board().active.len(), 2);
        assert!(pipeline.claim(&job("instagram", "c")).is_err());

        // A video its lane is already on isn't started a second time.
        pipeline
            .shared
            .board
            .send_modify(|board| board.start(&job("tiktok", "t"), false));
        assert!(!pipeline.claim(&job("tiktok", "t")).unwrap());
    }

    #[test]
    fn lanes_skip_videos_already_downloading() {
        let pipeline = pipeline();
        let mut conn = Connection::open_in_memory().unwrap();
        db::init(&mut conn).unwrap();
        for id in ["old", "new"] {
            conn.execute(
                "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) \
                 VALUES ('tiktok', ?1, 'https://example.com', '{}', 0)",
                [id],
            )
            .unwrap();
        }
        assert!(pipeline.claim(&job("tiktok", "new")).unwrap());
        let picked = next_job("tiktok", &pipeline.shared, &conn)
            .unwrap()
            .unwrap();
        assert_eq!(picked.item.video_id, "old");
        let board = pipeline.board();
        assert_eq!(
            board.lane_download("tiktok").unwrap().job.item.video_id,
            "old"
        );
        assert_eq!(board.lanes[1].left, Some(2));
        // Both are taken now, so there's nothing for the lane.
        assert!(
            next_job("tiktok", &pipeline.shared, &conn)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pauses_vary_within_their_range() {
        let (shortest, longest) = pace("tiktok");
        for _ in 0..100 {
            let pause = between(shortest, longest);
            assert!(shortest <= pause && pause <= longest);
        }
        let fixed = Duration::from_secs(30);
        assert_eq!(between(fixed, fixed), fixed);
        assert_eq!(pace("youtube"), (fixed, fixed));
    }
}
