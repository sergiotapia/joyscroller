//! The download pipeline. A requested video goes through each `Stage` in
//! turn; for now the only one is the download itself, and later ones (ffmpeg
//! work on the saved files) will follow it. Every platform has its own lane,
//! so YouTube and TikTok work side by side, but within a lane each stage
//! takes one video at a time: a platform never sees parallel downloads from
//! us. A new stage gets its own worker per lane, fed by a channel from the
//! stage before it, so the next download doesn't wait on the previous video's
//! later stages.
//!
//! What the lanes are doing is kept on a `Board`, which the web UI watches.
//! Requests live only in memory: ones still queued when the app stops are
//! dropped, and need asking for again.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::{mpsc, watch};

use crate::browser::Browser;
use crate::db::{self, PendingDownload};
use crate::download::{self, Credentials, Progress, Saved};

/// The pause after each download before the lane's next one. A queue can hold
/// many videos, and YouTube answers bursts of requests with HTTP 429.
const PAUSE: Duration = Duration::from_secs(5);

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

/// A video someone asked for.
#[derive(Debug, Clone)]
pub struct Job {
    pub item: PendingDownload,
    /// How the UI names it.
    pub title: String,
}

impl Job {
    fn is(&self, platform: &str, video_id: &str) -> bool {
        self.item.platform == platform && self.item.video_id == video_id
    }
}

/// A video a lane is working on.
#[derive(Debug, Clone)]
pub struct Active {
    pub job: Job,
    pub stage: Stage,
    pub started_at: i64,
    /// yt-dlp's latest progress report, once there is one.
    pub progress: Option<Progress>,
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
    /// At most one video per lane and stage.
    pub active: Vec<Active>,
    /// Waiting for their lane, oldest request first.
    pub queued: Vec<Job>,
    /// Since the app started, newest first, up to `RECENT`.
    pub recent: VecDeque<Finished>,
}

impl Board {
    pub fn active_on(&self, platform: &str) -> Option<&Active> {
        self.active.iter().find(|a| a.job.item.platform == platform)
    }

    pub fn find_active(&self, platform: &str, video_id: &str) -> Option<&Active> {
        self.active.iter().find(|a| a.job.is(platform, video_id))
    }

    /// Where `video_id` is in the queue, counting from 0.
    pub fn queue_position(&self, platform: &str, video_id: &str) -> Option<usize> {
        self.queued.iter().position(|j| j.is(platform, video_id))
    }

    fn start(&mut self, job: &Job, stage: Stage) {
        self.queued
            .retain(|j| !j.is(&job.item.platform, &job.item.video_id));
        self.active.push(Active {
            job: job.clone(),
            stage,
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

/// The handle the web UI asks for downloads through.
#[derive(Clone)]
pub struct Pipeline {
    lanes: Vec<(&'static str, mpsc::UnboundedSender<Job>)>,
    board: Arc<watch::Sender<Board>>,
}

impl Pipeline {
    /// Start a lane for each of `platforms`. Call it before anything else
    /// downloads: it clears what interrupted downloads left behind.
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
        let board = Arc::new(watch::Sender::new(Board::default()));
        let lanes = platforms
            .iter()
            .map(|&platform| {
                let (jobs, queue) = mpsc::unbounded_channel();
                let lane = Lane {
                    platform,
                    browser,
                    profile: profile.to_owned(),
                    db_path: db_path.to_owned(),
                    media_root: media_root.to_owned(),
                    board: Arc::clone(&board),
                };
                tokio::spawn(lane.download_stage(queue));
                (platform, jobs)
            })
            .collect();
        Pipeline { lanes, board }
    }

    /// Put `job` in its lane's queue. False if it's already queued or in
    /// progress, so a double click doesn't download twice.
    pub fn request(&self, job: Job) -> Result<bool> {
        let (_, lane) = self
            .lanes
            .iter()
            .find(|(platform, _)| *platform == job.item.platform)
            .with_context(|| format!("can't download from platform {}", job.item.platform))?;
        let (platform, video_id) = (job.item.platform.clone(), job.item.video_id.clone());
        let added = self.board.send_if_modified(|board| {
            let known = board.find_active(&platform, &video_id).is_some()
                || board.queue_position(&platform, &video_id).is_some();
            if !known {
                board.queued.push(job.clone());
            }
            !known
        });
        if added && lane.send(job).is_err() {
            self.board.send_modify(|board| {
                board.queued.retain(|j| !j.is(&platform, &video_id));
            });
            anyhow::bail!("the {platform} download lane has stopped");
        }
        Ok(added)
    }

    pub fn board(&self) -> Board {
        self.board.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Board> {
        self.board.subscribe()
    }
}

/// One platform's lane.
struct Lane {
    platform: &'static str,
    browser: Browser,
    profile: String,
    db_path: PathBuf,
    media_root: PathBuf,
    board: Arc<watch::Sender<Board>>,
}

impl Lane {
    /// The download stage's worker: download each queued video in turn and
    /// record how it went. Failures are recorded and logged; nothing stops it.
    async fn download_stage(self, mut queue: mpsc::UnboundedReceiver<Job>) {
        // Its own connection, so a lane's writes don't wait on the others'
        // or the syncs' longer than a single write.
        let mut conn = loop {
            match db::open(&self.db_path) {
                Ok(conn) => break conn,
                Err(e) => {
                    eprintln!("{}: can't open the database: {e:#}", self.platform);
                    tokio::time::sleep(DB_RETRY).await;
                }
            }
        };
        while let Some(job) = queue.recv().await {
            self.board
                .send_modify(|board| board.start(&job, Stage::Download));
            let started = Instant::now();
            let on_progress = |progress| {
                self.board
                    .send_modify(|board| board.progress(&job, progress));
            };
            // Cookies are read for every video: requests come one by one,
            // possibly hours apart, and a login can change in between.
            let result = async {
                let credentials =
                    Credentials::read(self.browser, &self.profile, &job.item.platform).await?;
                download::download(
                    &job.item,
                    &credentials.login(),
                    &self.media_root,
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
            if let Err(e) = record(&mut conn, &job.item, &result, started.elapsed()) {
                eprintln!(
                    "{}: couldn't record the download of {}: {e:#}",
                    self.platform, job.item.video_id
                );
                outcome = Outcome::Failed(format!("couldn't record it: {e:#}"));
            }
            // A later stage would take the job from here, through its channel.
            self.board.send_modify(|board| board.finish(&job, outcome));
            tokio::time::sleep(PAUSE).await;
        }
    }
}

/// Store how a download went and log it. A failure counts as an attempt, so
/// the video waits out its retry delay before the background queue
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
            )?;
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

    fn job(platform: &str, id: &str) -> Job {
        Job {
            item: PendingDownload {
                platform: platform.into(),
                video_id: id.into(),
                url: "https://example.com".into(),
                kind: "video".into(),
            },
            title: id.into(),
        }
    }

    #[test]
    fn board_moves_jobs_from_queued_to_active_to_recent() {
        let mut board = Board::default();
        board.queued.push(job("youtube", "a"));
        board.queued.push(job("youtube", "b"));
        board.start(&job("youtube", "a"), Stage::Download);
        assert_eq!(board.queue_position("youtube", "b"), Some(0));
        assert!(board.find_active("youtube", "a").is_some());
        assert!(board.active_on("tiktok").is_none());

        board.progress(
            &job("youtube", "a"),
            Progress {
                downloaded: 1,
                total: None,
                speed: None,
                eta: None,
                format_id: "18".into(),
            },
        );
        assert!(board.active_on("youtube").unwrap().progress.is_some());

        board.finish(&job("youtube", "a"), Outcome::Failed("nope".into()));
        assert!(board.active.is_empty());
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

    #[tokio::test]
    async fn requests_are_queued_once() {
        // A lane whose receiver is kept but never read, so jobs stay queued.
        let (jobs, _queue) = mpsc::unbounded_channel();
        let pipeline = Pipeline {
            lanes: vec![("youtube", jobs)],
            board: Arc::new(watch::Sender::new(Board::default())),
        };
        assert!(pipeline.request(job("youtube", "a")).unwrap());
        assert!(!pipeline.request(job("youtube", "a")).unwrap());
        assert!(pipeline.request(job("youtube", "b")).unwrap());
        assert_eq!(pipeline.board().queued.len(), 2);
        assert!(pipeline.request(job("instagram", "c")).is_err());
    }

    #[tokio::test]
    async fn a_stopped_lane_leaves_nothing_queued() {
        let (jobs, queue) = mpsc::unbounded_channel();
        drop(queue);
        let pipeline = Pipeline {
            lanes: vec![("youtube", jobs)],
            board: Arc::new(watch::Sender::new(Board::default())),
        };
        assert!(pipeline.request(job("youtube", "a")).is_err());
        assert!(pipeline.board().queued.is_empty());
    }
}
