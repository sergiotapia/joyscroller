//! The download pipeline. Every platform has a lane (unless `AUTOMATIC` is
//! off) that works through its backlog on its own: each liked video that isn't
//! downloaded yet, newest first, one at a time, pausing between videos (`pace`) so a backlog of
//! thousands doesn't look like a bot to the platform. A video someone asks
//! for in the UI doesn't wait for its lane: it downloads right away, on a
//! track of its own, alongside the lanes and any other requested videos.
//!
//! Once downloaded, a video goes to the converter, which makes its playback
//! copy (`transcode.rs`) for browsers that can't play the original. It works
//! on one video at a time at low priority, newest first, so the backlog of
//! copies fills in behind the downloads without making the computer
//! sluggish. A page that can't play a video's original asks for its copy,
//! which puts it at the front.
//!
//! The meter measures each download's loudness (`loudness.rs`) for /tv, one
//! at a time at low priority, newest first. It's quick (only the sound is
//! decoded) and nobody waits on it, so it stays off the board.
//!
//! Once an AT Protocol PDS account is connected in Settings, the uploader
//! copies each download there (`pds.rs`), one post at a time, newest first,
//! pausing a few seconds between posts. It runs on its own, from files
//! already downloaded, so downloads and playback copies never hold it up. A
//! refused login or the PDS saying to slow down stops the whole uploader, not
//! the post, until the user connects again or the PDS's limit resets.
//! Pausing it in Settings, or disconnecting, stops an upload under way.
//!
//! What the lanes, tracks, converter and uploader are doing is kept on a `Board`, which
//! the web UI watches. Nothing about the pipeline is stored: the lanes and
//! the converter pick up from the database when the app starts again, and a
//! requested download that was cut off needs asking for again.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{Notify, watch};

use crate::browser::Browser;
use crate::db::{self, PendingDownload, PendingLoudness, PendingPlayback, PendingUpload};
use crate::download::{self, Credentials, Progress, Saved};
use crate::{loudness, pds, transcode};

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
/// account out for "suspicious activity", so it gets the slowest pace. X's
/// downloads only fetch files from its media servers, not its API, and
/// need no login.
fn pace(platform: &str) -> (Duration, Duration) {
    match platform {
        "tiktok" => (Duration::from_secs(15), Duration::from_secs(30)),
        "instagram" => (Duration::from_secs(30), Duration::from_secs(60)),
        "x" => (Duration::from_secs(10), Duration::from_secs(20)),
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

/// The pause after a playback copy fails, so a broken ffmpeg doesn't spin
/// through the backlog. The video itself waits out its own retry delay.
const CONVERT_FAILURE_PAUSE: Duration = Duration::from_secs(10);

/// The pause between uploads. Bluesky's PDS software, when its operator
/// turns rate limits on, allows 5,000 write points an hour, a record costing
/// 3; a post every 5 s stays well under, and the uploads themselves take
/// longer than that anyway.
const UPLOAD_PACE: Duration = Duration::from_secs(5);

/// How long the uploader sits out after the PDS says to slow down without
/// saying until when: this, then doubling up to a day.
const FIRST_UPLOAD_PAUSE: Duration = Duration::from_mins(15);
const LONGEST_UPLOAD_PAUSE: Duration = Duration::from_hours(24);

/// How long the uploader waits after the PDS refused the login before trying
/// again, unless the account is connected again sooner.
const LOGGED_OUT_PAUSE: Duration = Duration::from_hours(1);

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
    Downloaded {
        bytes: u64,
        seconds: u64,
        /// The saved file to show as its picture, if any.
        picture: Option<String>,
    },
    Failed(String),
    /// The platform says the video is gone (`download::Unavailable`, whose
    /// words these are); it isn't tried again.
    Unavailable(String),
    /// Its playback copy was made.
    Converted {
        bytes: u64,
        seconds: u64,
        /// `transcode::Plan::method`: `remux` or `encode`.
        method: &'static str,
    },
    /// Its playback copy couldn't be made.
    ConvertFailed(String),
    /// It was uploaded to the PDS.
    Uploaded {
        bytes: u64,
        seconds: u64,
    },
    /// Its upload failed.
    UploadFailed(String),
    /// A file is over the PDS's limit, so it isn't uploaded.
    UploadTooBig(String),
}

/// What the converter, which makes playback copies, is doing.
#[derive(Debug, Clone, Default)]
pub struct Converter {
    /// Downloaded videos still waiting for a copy, counting the one it's on,
    /// as of the last time it looked. None until it first looks.
    pub left: Option<usize>,
    pub current: Option<Converting>,
}

/// A video the converter is making a playback copy of.
#[derive(Debug, Clone)]
pub struct Converting {
    pub job: Job,
    pub started_at: i64,
    /// ffmpeg's latest progress report, once there is one.
    pub progress: Option<transcode::Progress>,
}

/// Why the uploader is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadWait {
    /// The pause between posts.
    Pace,
    /// The pause after a failed upload.
    Failed,
    /// The longer pause after `FAILURES_BEFORE_OUTAGE` failures in a row.
    Outage,
    /// Nothing to upload; it looks again then.
    Empty,
    /// The PDS refused the login.
    LoggedOut,
    /// The PDS said to slow down.
    RateLimited,
    /// Paused in Settings.
    Paused,
}

/// What the uploader, which copies downloads to the PDS, is doing.
#[derive(Debug, Clone, Default)]
pub struct Uploader {
    /// The connected account, as "handle on host"; None when there's none.
    pub account: Option<String>,
    /// Posts still to upload, counting the one it's on, as of the last time
    /// it looked. None until it first looks.
    pub left: Option<usize>,
    pub current: Option<Uploading>,
    /// Why it's waiting, and until when.
    pub waiting: Option<(UploadWait, i64)>,
    /// The error that stopped it, for a refused login.
    pub error: Option<String>,
}

/// A post the uploader is uploading.
#[derive(Debug, Clone)]
pub struct Uploading {
    pub job: Job,
    pub started_at: i64,
    /// Bytes sent so far, of the post's files' `total`.
    pub sent: u64,
    pub total: u64,
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
    pub converter: Converter,
    pub uploader: Uploader,
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

    /// The video the converter is on, if it's this one.
    pub fn find_converting(&self, platform: &str, video_id: &str) -> Option<&Converting> {
        self.converter
            .current
            .as_ref()
            .filter(|c| c.job.is(platform, video_id))
    }

    /// The post the uploader is on, if it's this one.
    pub fn find_uploading(&self, platform: &str, video_id: &str) -> Option<&Uploading> {
        self.uploader
            .current
            .as_ref()
            .filter(|u| u.job.is(platform, video_id))
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
        self.remember(job, outcome);
    }

    fn remember(&mut self, job: &Job, outcome: Outcome) {
        self.recent.push_front(Finished {
            job: job.clone(),
            at: db::now(),
            outcome,
        });
        self.recent.truncate(RECENT);
    }

    fn start_converting(&mut self, job: &Job) {
        self.converter.current = Some(Converting {
            job: job.clone(),
            started_at: db::now(),
            progress: None,
        });
    }

    fn converting(&mut self, progress: transcode::Progress) {
        if let Some(current) = &mut self.converter.current {
            current.progress = Some(progress);
        }
    }

    /// Take the converter's video off the board. A video that needed no copy
    /// isn't worth listing among the finished.
    fn finish_converting(&mut self, job: &Job, outcome: Option<Outcome>) {
        self.converter.current = None;
        if let Some(outcome) = outcome {
            self.remember(job, outcome);
        }
    }
}

/// What every lane and track needs to download.
struct Shared {
    browser: Browser,
    profile: String,
    db_path: PathBuf,
    media_root: PathBuf,
    board: watch::Sender<Board>,
    /// Wakes the converter when there's a new download to convert.
    wake: Notify,
    /// Wakes the meter when there's a new download to measure.
    meter_wake: Notify,
    /// Wakes the uploader when there's a new download to upload.
    download_wake: Notify,
    /// Wakes the uploader when the PDS account was connected, disconnected,
    /// paused, resumed or changed in Settings.
    settings_wake: Notify,
    /// Set when uploading is paused or the account disconnected, which stops
    /// an upload under way. Only the uploader clears it, before it reads the
    /// account, so a stop that comes in meanwhile isn't lost.
    upload_stop: AtomicBool,
    /// Set when an account was connected: the uploader logs in afresh, with
    /// the password and server just typed.
    relogin: AtomicBool,
    /// Videos whose copies pages asked for, to convert before the backlog.
    asked: Mutex<VecDeque<(String, String)>>,
}

impl Shared {
    fn new(browser: Browser, profile: &str, db_path: &Path, media_root: &Path) -> Shared {
        Shared {
            browser,
            profile: profile.to_owned(),
            db_path: db_path.to_owned(),
            media_root: media_root.to_owned(),
            board: watch::Sender::new(Board::default()),
            wake: Notify::new(),
            meter_wake: Notify::new(),
            download_wake: Notify::new(),
            settings_wake: Notify::new(),
            upload_stop: AtomicBool::new(false),
            relogin: AtomicBool::new(false),
            asked: Mutex::new(VecDeque::new()),
        }
    }
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
            Shared::new(browser, profile, db_path, media_root),
        );
        if AUTOMATIC {
            for &platform in platforms {
                tokio::spawn(run_lane(platform, Arc::clone(&pipeline.shared)));
            }
        }
        // Copies are made on this computer from files already here, so the
        // converter runs whether or not the lanes do.
        tokio::spawn(run_converter(Arc::clone(&pipeline.shared)));
        tokio::spawn(run_meter(Arc::clone(&pipeline.shared)));
        tokio::spawn(run_uploader(Arc::clone(&pipeline.shared)));
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

    /// Make `platform`/`video_id`'s playback copy next, before the backlog:
    /// a page can't play its original.
    pub fn convert_soon(&self, platform: &str, video_id: &str) {
        let key = (platform.to_owned(), video_id.to_owned());
        {
            let mut asked = self
                .shared
                .asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !asked.contains(&key) {
                asked.push_back(key);
            }
        }
        self.shared.wake.notify_one();
    }

    /// The PDS account was resumed or its platforms changed: the uploader
    /// looks again now.
    pub fn uploads_changed(&self) {
        self.shared.settings_wake.notify_one();
    }

    /// Uploading was paused or the account disconnected: an upload under
    /// way stops, and the uploader looks again now. Call it after the
    /// change is stored.
    pub fn stop_uploads(&self) {
        self.shared.upload_stop.store(true, Ordering::SeqCst);
        self.uploads_changed();
    }

    /// An account was connected: the uploader logs in to it afresh now.
    pub fn account_connected(&self) {
        self.shared.relogin.store(true, Ordering::SeqCst);
        self.uploads_changed();
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
pub fn between(shortest: Duration, longest: Duration) -> Duration {
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
/// the board. True if the platform answered: the video was downloaded and
/// recorded, or the platform said it's gone, which is no sign of trouble.
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
            picture: saved.picture().map(Into::into),
        },
        Err(e) if download::is_unavailable(e) => Outcome::Unavailable(
            e.downcast_ref::<download::Unavailable>()
                .map_or_else(|| format!("{e:#}"), ToString::to_string),
        ),
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
    let answered = downloaded || matches!(outcome, Outcome::Unavailable(_));
    shared.board.send_modify(|board| board.finish(job, outcome));
    if downloaded {
        shared.wake.notify_one();
        shared.meter_wake.notify_one();
        shared.download_wake.notify_one();
    }
    answered
}

/// The converter: make every downloaded video's playback copy, one at a
/// time, forever. Failures are recorded and logged; nothing stops it.
async fn run_converter(shared: Arc<Shared>) {
    // Its own connection, as the lanes have.
    let mut conn = loop {
        match db::open(&shared.db_path) {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("playback: can't open the database: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    };
    // ffmpeg may still be on its way on first launch.
    loop {
        match crate::tools::ffmpeg_ready() {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => eprintln!("playback: can't check for ffmpeg: {e:#}"),
        }
        tokio::time::sleep(TOOLS_RETRY).await;
    }
    loop {
        match next_conversion(&shared, &conn) {
            Ok(Some((job, pending))) => {
                if !convert_job(&shared, &mut conn, &job, &pending).await {
                    tokio::time::sleep(CONVERT_FAILURE_PAUSE).await;
                }
            }
            Ok(None) => {
                // New downloads wake it; the timeout catches retries coming due.
                tokio::select! {
                    () = shared.wake.notified() => {}
                    () = tokio::time::sleep(IDLE) => {}
                }
            }
            Err(e) => {
                eprintln!("playback: can't read the queue: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    }
}

/// The next video to make a playback copy of, put on the board as the
/// converter's: one a page asked for, or else the newest in the backlog.
/// Also updates the converter's count of what's left.
fn next_conversion(shared: &Shared, conn: &Connection) -> Result<Option<(Job, PendingPlayback)>> {
    let mut next = None;
    loop {
        let asked = shared
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some((platform, video_id)) = asked else {
            break;
        };
        // Already made, or not downloaded after all: nothing to do.
        if let Some(pending) = db::playback_item(conn, &platform, &video_id)? {
            next = Some(pending);
            break;
        }
    }
    if next.is_none() {
        next = db::pending_playback(conn, 1)?.into_iter().next();
    }
    let left = db::pending_playback_count(conn)?;
    let picked = next.map(|pending| (Job::new(pending.item.clone()), pending));
    shared.board.send_modify(|board| {
        board.converter.left = Some(left);
        if let Some((job, _)) = &picked {
            board.start_converting(job);
        }
    });
    Ok(picked)
}

/// Make the playback copy of a video that's on the board as the
/// converter's, record how it went, and take it off the board. True unless
/// it failed.
async fn convert_job(
    shared: &Shared,
    conn: &mut Connection,
    job: &Job,
    pending: &PendingPlayback,
) -> bool {
    let started = Instant::now();
    let (platform, video_id) = (&job.item.platform, &job.item.video_id);
    let on_progress = |progress| shared.board.send_modify(|board| board.converting(progress));
    let result = transcode::make(
        &shared.media_root.join(&pending.folder),
        &pending.files,
        &on_progress,
    )
    .await;
    let seconds = started.elapsed().as_secs();
    let recorded = match &result {
        Ok(made) => db::record_playback(
            conn,
            platform,
            video_id,
            &db::MadePlayback {
                download_folder: &pending.folder,
                downloaded_at: pending.downloaded_at,
                file_name: made.file,
                size_bytes: made.bytes,
                method: made.method,
                original_config_json: made.source.as_deref(),
            },
        ),
        Err(e) => db::record_playback_failure(
            conn,
            platform,
            video_id,
            (&pending.folder, pending.downloaded_at),
            &format!("{e:#}"),
        )
        .map(|()| true),
    };
    let mut outcome = match &result {
        // Downloaded again while this copy was made; that download's turn
        // comes next.
        Ok(_) if matches!(recorded, Ok(false)) => {
            eprintln!(
                "playback: {platform} video {video_id} was downloaded again; its copy is made afresh"
            );
            None
        }
        Ok(made) if made.file.is_some() => {
            #[expect(clippy::cast_precision_loss, reason = "a size for a log line")]
            let megabytes = made.bytes as f64 / 1e6;
            eprintln!(
                "playback: made {platform} video {video_id}'s copy ({megabytes:.1} MB, {}) in {seconds}s",
                made.method
            );
            Some(Outcome::Converted {
                bytes: made.bytes,
                seconds,
                method: made.method,
            })
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("playback: {platform} video {video_id}'s copy failed: {e:#}");
            Some(Outcome::ConvertFailed(format!("{e:#}")))
        }
    };
    if let Err(e) = recorded {
        eprintln!("playback: couldn't record {platform} video {video_id}'s copy: {e:#}");
        outcome = Some(Outcome::ConvertFailed(format!("couldn't record it: {e:#}")));
    }
    let ok = !matches!(outcome, Some(Outcome::ConvertFailed(_)));
    shared
        .board
        .send_modify(|board| board.finish_converting(job, outcome));
    ok
}

/// The meter: measure every download's loudness, one at a time, forever.
/// Failures are recorded and logged; nothing stops it.
async fn run_meter(shared: Arc<Shared>) {
    let mut conn = loop {
        match db::open(&shared.db_path) {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("loudness: can't open the database: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    };
    loop {
        match crate::tools::ffmpeg_ready() {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => eprintln!("loudness: can't check for ffmpeg: {e:#}"),
        }
        tokio::time::sleep(TOOLS_RETRY).await;
    }
    loop {
        match db::pending_loudness(&conn, 1).map(|mut next| next.pop()) {
            Ok(Some(pending)) => {
                if !measure_one(&shared, &mut conn, &pending).await {
                    tokio::time::sleep(CONVERT_FAILURE_PAUSE).await;
                }
            }
            Ok(None) => {
                tokio::select! {
                    () = shared.meter_wake.notified() => {}
                    () = tokio::time::sleep(IDLE) => {}
                }
            }
            Err(e) => {
                eprintln!("loudness: can't read the queue: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    }
}

/// Measure one download's loudness and record it. True unless it failed.
async fn measure_one(shared: &Shared, conn: &mut Connection, pending: &PendingLoudness) -> bool {
    let (platform, video_id) = (&pending.platform, &pending.video_id);
    let measured =
        loudness::measure(&shared.media_root.join(&pending.folder), &pending.files).await;
    let recorded = match &measured {
        Ok(found) => db::record_loudness(conn, pending, *found).map(|_| ()),
        Err(e) => db::record_loudness_failure(conn, pending, &format!("{e:#}")),
    };
    if let Err(e) = &measured {
        eprintln!("loudness: measuring {platform} video {video_id} failed: {e:#}");
    }
    if let Err(e) = &recorded {
        eprintln!("loudness: couldn't record {platform} video {video_id}'s loudness: {e:#}");
    }
    measured.is_ok() && recorded.is_ok()
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
                    folder: &saved.dir,
                    files: &saved.files,
                    size_bytes: saved.bytes,
                    formats: Some(&saved.format),
                    metadata_json: saved.info_json.as_deref(),
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
        Err(e) if download::is_unavailable(e) => {
            db::record_download_failure(
                conn,
                &item.platform,
                &item.video_id,
                &format!("{e:#}"),
                true,
            )?;
            eprintln!(
                "{name}: {} is unavailable, not trying it again ({e:#})",
                item.video_id
            );
        }
        Err(e) => {
            db::record_download_failure(
                conn,
                &item.platform,
                &item.video_id,
                &format!("{e:#}"),
                false,
            )?;
            eprintln!("{name}: download of {} failed: {e:#}", item.video_id);
        }
    }
    Ok(())
}

/// The uploader: upload every download to the connected PDS account, one
/// post at a time, forever. With no account it idles until one is connected.
async fn run_uploader(shared: Arc<Shared>) {
    let mut conn = loop {
        match db::open(&shared.db_path) {
            Ok(conn) => break conn,
            Err(e) => {
                eprintln!("pds: can't open the database: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
            }
        }
    };
    let mut session: Option<pds::Session> = None;
    let mut pauses = UploadPauses::default();
    loop {
        // Cleared before the account is read: a pause or disconnect stored
        // after this sets it again, and one stored before shows in the read.
        shared.upload_stop.store(false, Ordering::SeqCst);
        if shared.relogin.swap(false, Ordering::SeqCst) {
            session = None;
        }
        let account = match db::pds_account(&conn) {
            Ok(account) => account,
            Err(e) => {
                eprintln!("pds: can't read the account: {e:#}");
                tokio::time::sleep(DB_RETRY).await;
                continue;
            }
        };
        let Some(account) = account else {
            session = None;
            shared
                .board
                .send_modify(|board| board.uploader = Uploader::default());
            upload_wait(&shared, None, IDLE).await;
            continue;
        };
        let host = account
            .pds_url
            .split("://")
            .nth(1)
            .unwrap_or(&account.pds_url)
            .to_string();
        shared.board.send_modify(|board| {
            board.uploader.account = Some(format!("{} on {host}", account.handle));
        });
        if account.paused {
            upload_wait(&shared, Some(UploadWait::Paused), IDLE).await;
            continue;
        }
        if session.as_ref().is_none_or(|s| s.did != account.did) {
            match log_in(&account).await {
                Ok(new) => session = Some(new),
                Err(e) => {
                    eprintln!("pds: can't log in as {}: {e:#}", account.handle);
                    let (why, pause) = if e.is::<pds::LoggedOut>() {
                        (UploadWait::LoggedOut, LOGGED_OUT_PAUSE)
                    } else {
                        (UploadWait::Failed, FAILURE_PAUSE)
                    };
                    shared
                        .board
                        .send_modify(|board| board.uploader.error = Some(format!("{e:#}")));
                    upload_wait(&shared, Some(why), pause).await;
                    continue;
                }
            }
        }
        let next = db::pending_uploads(&conn, None, 1)
            .and_then(|next| Ok((next, db::pending_upload_count(&conn)?)));
        let (pending, left) = match next {
            Ok((mut next, left)) => (next.pop(), left),
            Err(e) => {
                eprintln!("pds: can't read the queue: {e:#}");
                upload_wait(&shared, Some(UploadWait::Failed), DB_RETRY).await;
                continue;
            }
        };
        shared.board.send_modify(|board| {
            board.uploader.left = Some(left);
            board.uploader.error = None;
        });
        let (Some(pending), Some(current)) = (pending, session.take()) else {
            upload_wait(&shared, Some(UploadWait::Empty), IDLE).await;
            continue;
        };
        let (back, next) = upload_job(&shared, &mut conn, current, &account, &pending).await;
        session = back;
        let (why, pause) = pauses.after(&next);
        upload_wait(&shared, Some(why), pause).await;
    }
}

/// What the uploader's run of failures and slow-downs says to wait.
#[derive(Default)]
struct UploadPauses {
    failures_in_a_row: usize,
    /// The next slow-down's pause when the PDS doesn't say; zero until one.
    slow_down: Duration,
}

impl UploadPauses {
    /// Why and how long to wait after a post that ended in `next`.
    fn after(&mut self, next: &Next) -> (UploadWait, Duration) {
        match next {
            Next::Done => {
                *self = UploadPauses::default();
                (UploadWait::Pace, UPLOAD_PACE)
            }
            Next::Failed => {
                self.failures_in_a_row += 1;
                if self.failures_in_a_row < FAILURES_BEFORE_OUTAGE {
                    return (UploadWait::Failed, FAILURE_PAUSE);
                }
                eprintln!(
                    "pds: {} uploads failed in a row; pausing uploads for {} min",
                    self.failures_in_a_row,
                    OUTAGE_PAUSE.as_secs() / 60
                );
                self.failures_in_a_row = 0;
                (UploadWait::Outage, OUTAGE_PAUSE)
            }
            Next::SlowDown(Some(pause)) => (UploadWait::RateLimited, *pause),
            Next::SlowDown(None) => {
                let pause = self.slow_down.max(FIRST_UPLOAD_PAUSE);
                self.slow_down = (pause * 2).min(LONGEST_UPLOAD_PAUSE);
                (UploadWait::RateLimited, pause)
            }
            Next::LoggedOut => (UploadWait::LoggedOut, LOGGED_OUT_PAUSE),
            // The account says what's next, and it's looked at right away.
            Next::Stopped => (UploadWait::Paused, Duration::ZERO),
        }
    }
}

/// What the uploader does after a post.
enum Next {
    /// Go on to the next post: this one was uploaded, or is too big.
    Done,
    /// The upload failed; the post waits out its retry delay.
    Failed,
    /// The PDS said to slow down, and maybe for how long.
    SlowDown(Option<Duration>),
    /// The PDS refused the login.
    LoggedOut,
    /// Paused or disconnected while it uploaded.
    Stopped,
}

/// Log in to the account's PDS with the app password in the keyring.
async fn log_in(account: &db::PdsAccount) -> Result<pds::Session> {
    let Some(password) = crate::keyring::load(&account.did).await? else {
        return Err(pds::LoggedOut("no app password is kept for this account".into()).into());
    };
    let (service, handle) = (account.service_url.clone(), account.handle.clone());
    tokio::task::spawn_blocking(move || pds::Session::log_in(&service, &handle, &password))
        .await
        .context("the login stopped")?
}

/// Sleep for `pause`, or until something that ends the wait early
/// (`cut_short_by`), with the uploader showing why and until when.
async fn upload_wait(shared: &Shared, why: Option<UploadWait>, pause: Duration) {
    let until = db::now() + i64::try_from(pause.as_secs()).unwrap_or(i64::MAX);
    shared.board.send_modify(|board| {
        board.uploader.waiting = why.map(|why| (why, until));
    });
    let (by_download, by_settings) = cut_short_by(why);
    let download = async {
        if by_download {
            shared.download_wake.notified().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let settings = async {
        if by_settings {
            shared.settings_wake.notified().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = download => {}
        () = settings => {}
        () = tokio::time::sleep(pause) => {}
    }
    shared
        .board
        .send_modify(|board| board.uploader.waiting = None);
}

/// Whether a finished download, and a change in Settings, end a wait for
/// `why` early. Only having nothing to upload waits on downloads. The pace
/// and a rate limit are kept whatever happens. After a refused login or a
/// failure, only the user changing something is worth trying again for:
/// logging in again on every download would hammer the server with a
/// password it refuses, which can get the account locked.
fn cut_short_by(why: Option<UploadWait>) -> (bool, bool) {
    match why {
        Some(UploadWait::Empty) => (true, true),
        Some(UploadWait::Pace | UploadWait::RateLimited) => (false, false),
        Some(
            UploadWait::Failed | UploadWait::Outage | UploadWait::LoggedOut | UploadWait::Paused,
        )
        | None => (false, true),
    }
}

/// How the board shows a post being uploaded: as a job, which only needs
/// its name and where it links.
fn upload_board_job(pending: &PendingUpload) -> Job {
    Job::new(PendingDownload {
        platform: pending.platform.clone(),
        video_id: pending.video_id.clone(),
        url: pending.url.clone(),
        kind: pending.kind.clone(),
        title: pending.title.clone(),
        author_name: pending.author_name.clone(),
        added_on: String::new(),
        listing_json: String::new(),
    })
}

/// Upload a post, record how it went and keep the board up to date. Returns
/// the session (None if it has to log in again) and what to do next.
async fn upload_job(
    shared: &Arc<Shared>,
    conn: &mut Connection,
    session: pds::Session,
    account: &db::PdsAccount,
    pending: &PendingUpload,
) -> (Option<pds::Session>, Next) {
    let started = Instant::now();
    let job = upload_board_job(pending);
    shared.board.send_modify(|board| {
        board.uploader.current = Some(Uploading {
            job: job.clone(),
            started_at: db::now(),
            sent: 0,
            total: pending.size_bytes,
        });
    });
    // A file is read in small pieces; the board needs a few updates a second.
    let reported = Mutex::new(Instant::now());
    let progress_board = Arc::clone(shared);
    let progress = move |sent, total| {
        let go_on = !progress_board.upload_stop.load(Ordering::SeqCst);
        let mut last = reported.lock().unwrap_or_else(PoisonError::into_inner);
        if last.elapsed() < Duration::from_millis(250) && sent < total {
            return go_on;
        }
        *last = Instant::now();
        progress_board.board.send_modify(|board| {
            if let Some(current) = &mut board.uploader.current {
                current.sent = sent;
                current.total = total;
            }
        });
        go_on
    };
    let (session, result) = upload_post(
        session,
        &shared.media_root,
        pending,
        account.blob_limit_bytes,
        progress,
    )
    .await;
    if matches!(&result, Err(e) if e.is::<pds::Stopped>()) {
        eprintln!(
            "pds: stopped uploading {} post {}",
            pending.platform, pending.video_id
        );
        shared
            .board
            .send_modify(|board| board.uploader.current = None);
        return (session, Next::Stopped);
    }
    let recorded = record_upload(conn, &account.did, pending, &result);
    let (mut outcome, mut next) = match &result {
        Ok(uploaded) => (
            Outcome::Uploaded {
                bytes: uploaded.blobs.iter().map(|b| b.size_bytes).sum(),
                seconds: started.elapsed().as_secs(),
            },
            Next::Done,
        ),
        Err(e) if e.is::<pds::TooBig>() => (Outcome::UploadTooBig(format!("{e:#}")), Next::Done),
        Err(e) if e.is::<pds::LoggedOut>() => {
            (Outcome::UploadFailed(format!("{e:#}")), Next::LoggedOut)
        }
        Err(e) => match e.downcast_ref::<pds::RateLimited>() {
            // The PDS said when its limit resets: wait that long.
            Some(limited) => (
                Outcome::UploadFailed(format!("{e:#}")),
                Next::SlowDown(limited.until.map(|until| {
                    Duration::from_secs(u64::try_from(until - db::now()).unwrap_or(0).max(60))
                })),
            ),
            None => (Outcome::UploadFailed(format!("{e:#}")), Next::Failed),
        },
    };
    if let Err(e) = recorded {
        eprintln!(
            "pds: couldn't record the upload of {} post {}: {e:#}",
            pending.platform, pending.video_id
        );
        outcome = Outcome::UploadFailed(format!("couldn't record it: {e:#}"));
        if matches!(next, Next::Done) {
            next = Next::Failed;
        }
    }
    let error = match (&next, &result) {
        (Next::LoggedOut, Err(e)) => Some(format!("{e:#}")),
        _ => None,
    };
    shared.board.send_modify(|board| {
        board.uploader.current = None;
        board.uploader.error = error;
        board.remember(&job, outcome);
    });
    // A refused login means logging in again, once the user has fixed it.
    let session = if matches!(next, Next::LoggedOut) {
        None
    } else {
        session
    };
    (session, next)
}

/// Upload `pending`'s files and record to the PDS in a blocking thread,
/// which `session` goes into and comes back out of. None for the session if
/// the thread was lost, which means logging in again.
pub async fn upload_post(
    session: pds::Session,
    media_root: &Path,
    pending: &PendingUpload,
    limit_bytes: Option<u64>,
    progress: impl Fn(u64, u64) -> bool + Send + 'static,
) -> (Option<pds::Session>, Result<pds::Uploaded>) {
    let media_root = media_root.to_owned();
    let post = pending.clone();
    let mut session = session;
    let uploaded = tokio::task::spawn_blocking(move || {
        let result = session.upload_post(&media_root, &post, limit_bytes, &progress);
        (session, result)
    })
    .await;
    match uploaded {
        Ok((session, result)) => (Some(session), result),
        Err(e) => (
            None,
            Err(anyhow::Error::new(e).context("the upload stopped")),
        ),
    }
}

/// Store how an upload to `did` went and log it. A failure counts against
/// the post unless it was about the account or the PDS's pace (logged out,
/// rate limited), which aren't the post's doing.
pub fn record_upload(
    conn: &Connection,
    did: &str,
    pending: &PendingUpload,
    result: &Result<pds::Uploaded>,
) -> Result<()> {
    let (platform, video_id) = (&pending.platform, &pending.video_id);
    match result {
        Ok(uploaded) => {
            let blobs: Vec<serde_json::Value> = uploaded
                .blobs
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "file": b.file,
                        "cid": b.cid,
                        "mime_type": b.mime_type,
                        "size_bytes": b.size_bytes,
                    })
                })
                .collect();
            let size_bytes = uploaded.blobs.iter().map(|b| b.size_bytes).sum();
            let stored = db::record_upload(
                conn,
                did,
                pending,
                &db::UploadedPost {
                    record_uri: &uploaded.record_uri,
                    record_cid: &uploaded.record_cid,
                    blobs_json: &serde_json::to_string(&blobs)?,
                    size_bytes,
                },
            )
            .context("storing the upload")?;
            #[expect(clippy::cast_precision_loss, reason = "a size for a log line")]
            let megabytes = size_bytes as f64 / 1e6;
            if stored {
                eprintln!(
                    "pds: uploaded {platform} post {video_id} ({megabytes:.1} MB) as {}",
                    uploaded.record_uri
                );
            } else {
                eprintln!(
                    "pds: {platform} post {video_id} was downloaded again; it's uploaded afresh"
                );
            }
        }
        Err(e) if e.is::<pds::TooBig>() => {
            db::record_too_big(conn, did, pending, &format!("{e:#}"))?;
            eprintln!("pds: not uploading {platform} post {video_id}: {e:#}");
        }
        Err(e) if e.is::<pds::Stopped>() => {}
        Err(e) if e.is::<pds::LoggedOut>() || e.is::<pds::RateLimited>() => {
            eprintln!("pds: {platform} post {video_id} waits: {e:#}");
        }
        Err(e) => {
            db::record_upload_failure(conn, did, pending, &format!("{e:#}"))?;
            eprintln!("pds: upload of {platform} post {video_id} failed: {e:#}");
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
            added_on: "2026-10-12".into(),
            listing_json: "{}".into(),
        }
    }

    fn job(platform: &str, id: &str) -> Job {
        Job::new(item(platform, id))
    }

    fn pipeline() -> Pipeline {
        Pipeline::new(
            &["youtube", "tiktok"],
            true,
            Shared::new(
                Browser::Brave,
                "Default",
                Path::new("/nonexistent/joyscroller.db"),
                Path::new("/nonexistent/downloads"),
            ),
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
                "INSERT INTO videos (platform, video_id, url, listing_json, added_at) \
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
    fn the_converter_takes_asked_for_videos_first() {
        let pipeline = pipeline();
        let mut conn = Connection::open_in_memory().unwrap();
        db::init(&mut conn).unwrap();
        for id in ["old", "new"] {
            conn.execute(
                "INSERT INTO videos (platform, video_id, url, listing_json, added_at) \
                 VALUES ('tiktok', ?1, 'https://example.com', '{}', 0)",
                [id],
            )
            .unwrap();
            let files = ["video.mp4".to_string()];
            let done = db::Downloaded {
                folder: id,
                files: &files,
                size_bytes: 1,
                formats: None,
                metadata_json: None,
                posted_at: None,
            };
            db::record_download(&mut conn, "tiktok", id, &done).unwrap();
        }
        // Asking twice queues it once; asking for a video that isn't
        // downloaded does nothing.
        pipeline.convert_soon("tiktok", "nope");
        pipeline.convert_soon("tiktok", "old");
        pipeline.convert_soon("tiktok", "old");
        let (job, pending) = next_conversion(&pipeline.shared, &conn).unwrap().unwrap();
        assert_eq!(job.item.video_id, "old");
        assert_eq!(pending.files, ["video.mp4"]);
        let board = pipeline.board();
        assert_eq!(board.converter.left, Some(2));
        assert!(board.find_converting("tiktok", "old").is_some());
        assert!(board.find_converting("tiktok", "new").is_none());

        pipeline.shared.board.send_modify(|board| {
            board.converting(transcode::Progress {
                done: 1.0,
                total: Some(4.0),
                speed: None,
            });
        });
        assert!(
            pipeline
                .board()
                .converter
                .current
                .unwrap()
                .progress
                .is_some()
        );
        pipeline
            .shared
            .board
            .send_modify(|board| board.finish_converting(&job, None));
        let board = pipeline.board();
        assert!(board.converter.current.is_none());
        // A video that needed no copy isn't listed as finished.
        assert!(board.recent.is_empty());

        // Then the backlog, newest first.
        let (job, _) = next_conversion(&pipeline.shared, &conn).unwrap().unwrap();
        assert_eq!(job.item.video_id, "new");
        pipeline.shared.board.send_modify(|board| {
            board.finish_converting(&job, Some(Outcome::ConvertFailed("no".into())));
        });
        assert_eq!(pipeline.board().recent.len(), 1);
    }

    #[test]
    fn uploads_pace_back_off_and_stop() {
        let mut pauses = UploadPauses::default();
        assert_eq!(pauses.after(&Next::Done), (UploadWait::Pace, UPLOAD_PACE));
        // A run of failures ends in the longer pause, and starts over.
        assert_eq!(pauses.after(&Next::Failed).0, UploadWait::Failed);
        assert_eq!(pauses.after(&Next::Failed).0, UploadWait::Failed);
        assert_eq!(
            pauses.after(&Next::Failed),
            (UploadWait::Outage, OUTAGE_PAUSE)
        );
        assert_eq!(pauses.after(&Next::Failed).0, UploadWait::Failed);
        // Slowing down doubles when the server doesn't say how long, up to a
        // day, and takes the server's word when it does.
        let slow: Vec<Duration> = (0..9)
            .map(|_| pauses.after(&Next::SlowDown(None)).1)
            .collect();
        assert_eq!(
            slow[..3],
            [
                FIRST_UPLOAD_PAUSE,
                FIRST_UPLOAD_PAUSE * 2,
                FIRST_UPLOAD_PAUSE * 4
            ]
        );
        assert_eq!(slow[8], LONGEST_UPLOAD_PAUSE);
        let said = Duration::from_secs(90);
        assert_eq!(pauses.after(&Next::SlowDown(Some(said))).1, said);
        // An upload that goes through starts the slow-downs over.
        pauses.after(&Next::Done);
        assert_eq!(pauses.after(&Next::SlowDown(None)).1, FIRST_UPLOAD_PAUSE);
        assert_eq!(
            pauses.after(&Next::LoggedOut),
            (UploadWait::LoggedOut, LOGGED_OUT_PAUSE)
        );
        assert_eq!(
            pauses.after(&Next::Stopped),
            (UploadWait::Paused, Duration::ZERO)
        );
    }

    #[test]
    fn only_settings_wake_an_uploader_that_was_refused() {
        // A download finishing mustn't send the server a refused password
        // again, nor cut a pace or a rate limit short.
        for why in [
            UploadWait::LoggedOut,
            UploadWait::Failed,
            UploadWait::Outage,
            UploadWait::Paused,
        ] {
            assert_eq!(cut_short_by(Some(why)), (false, true), "{why:?}");
        }
        for why in [UploadWait::Pace, UploadWait::RateLimited] {
            assert_eq!(cut_short_by(Some(why)), (false, false), "{why:?}");
        }
        assert_eq!(cut_short_by(Some(UploadWait::Empty)), (true, true));
        assert_eq!(cut_short_by(None), (false, true));
    }

    #[test]
    fn a_stop_is_only_cleared_by_the_uploader() {
        let pipeline = pipeline();
        let stopped = || pipeline.shared.upload_stop.load(Ordering::SeqCst);
        pipeline.stop_uploads();
        assert!(stopped());
        // Saving the platforms, or resuming, right after a pause doesn't let
        // the upload under way go on.
        pipeline.uploads_changed();
        pipeline.account_connected();
        assert!(stopped());
        assert!(pipeline.shared.relogin.load(Ordering::SeqCst));
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
