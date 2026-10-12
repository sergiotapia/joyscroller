//! The localhost web UI: plain server-rendered HTML. The list page pages
//! through the liked videos, newest first, on all platforms or one; each video
//! has its own page, with a button that has the download pipeline download it
//! right away, and `/downloading` shows what the pipeline is doing. Every open page
//! keeps a Server-Sent Events connection, over which newly stored videos, the
//! per-platform counts, what each sync is doing and the pipeline's progress
//! arrive as soon as they change.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{Stream, StreamExt, stream};
use rusqlite::Connection;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::db;
use crate::pipeline::{Active, Board, Job, Lane, Outcome, Pipeline, Wait};

/// Where the UI listens. Localhost only: the archive is the user's own.
pub const ADDR: &str = "127.0.0.1:4774";

const PAGE_SIZE: usize = 50;

/// The sidebar's platforms, as (stored name, display name).
const PLATFORMS: [(&str, &str); 2] = [("youtube", "YouTube"), ("tiktok", "TikTok")];

fn display_name(platform: &str) -> &str {
    PLATFORMS
        .iter()
        .find(|(name, _)| *name == platform)
        .map_or(platform, |(_, display)| display)
}

/// What each platform's sync is doing, shown in the header. The app loop
/// updates it, and every change is pushed to the open pages.
#[derive(Debug, Clone, Default)]
pub struct SyncStatus(Vec<(&'static str, SyncState)>);

#[derive(Debug, Clone)]
pub enum SyncState {
    Running,
    Done { at: i64, added: usize },
    Failed { at: i64, error: String },
}

impl SyncStatus {
    pub fn set(&mut self, platform: &'static str, state: SyncState) {
        match self.0.iter_mut().find(|(name, _)| *name == platform) {
            Some(entry) => entry.1 = state,
            None => self.0.push((platform, state)),
        }
    }

    fn get(&self, platform: &str) -> Option<&SyncState> {
        self.0
            .iter()
            .find(|(name, _)| *name == platform)
            .map(|(_, state)| state)
    }
}

#[derive(Clone)]
struct App {
    db: Arc<Mutex<Connection>>,
    status: watch::Receiver<SyncStatus>,
    pipeline: Pipeline,
}

impl App {
    /// Run queries on the UI's connection. They're short reads, and WAL means
    /// a sync's write doesn't block them, so a plain mutex is enough. A panic
    /// mid-query leaves the connection usable, so a poisoned lock is still taken.
    fn db<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        f(&conn)
    }
}

/// Serve the UI on `listener` until the process exits.
pub async fn serve(
    listener: TcpListener,
    conn: Connection,
    status: watch::Receiver<SyncStatus>,
    pipeline: Pipeline,
) -> Result<()> {
    let app = App {
        db: Arc::new(Mutex::new(conn)),
        status,
        pipeline,
    };
    let router = Router::new()
        .route("/", get(list_page))
        .route("/videos/{platform}/{video_id}", get(video_page))
        .route(
            "/videos/{platform}/{video_id}/download",
            post(request_download),
        )
        .route("/downloading", get(downloading_page))
        .route("/events", get(events))
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

/// A failed request: a status code and a plain-text message.
struct Failure(StatusCode, String);

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        eprintln!("web: {e:#}");
        Failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

fn not_found() -> Failure {
    Failure(StatusCode::NOT_FOUND, "not found".into())
}

#[derive(Deserialize)]
struct ListQuery {
    platform: Option<String>,
    page: Option<usize>,
}

async fn list_page(
    State(app): State<App>,
    Query(query): Query<ListQuery>,
) -> Result<Html<String>, Failure> {
    let platform = match query.platform.as_deref() {
        None | Some("") => None,
        Some(wanted) => Some(
            PLATFORMS
                .iter()
                .find(|(name, _)| *name == wanted)
                .ok_or_else(not_found)?
                .0,
        ),
    };
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1).saturating_mul(PAGE_SIZE);
    let (videos, counts, after) = app.db(|conn| {
        Ok((
            db::videos_page(conn, platform, PAGE_SIZE, offset)?,
            db::video_counts(conn)?,
            db::last_rowid(conn)?,
        ))
    })?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let pages = count_of(&counts, platform).div_ceil(PAGE_SIZE).max(1);

    let heading = match platform {
        Some(name) => format!("{} likes", display_name(name)),
        None => "All likes".into(),
    };
    let pager = pager(platform, page, pages);
    let mut main = format!(
        "<h1>{heading}</h1>\n\
         <p id=\"new\" hidden><a href=\"{}\"><span id=\"new-count\">0</span> new — show them</a></p>\n\
         {pager}\n",
        list_href(platform, 1)
    );
    if videos.is_empty() {
        main.push_str(if page == 1 {
            "<p id=\"empty\">No liked videos yet. They show up here as each sync finishes.</p>\n"
        } else {
            "<p id=\"empty\">Nothing on this page.</p>\n"
        });
    }
    main.push_str(
        "<table>\n<thead><tr><th></th><th>Title</th><th>Author</th><th>Length</th>\
         <th>Posted</th><th>Added</th><th>Source</th></tr></thead>\n<tbody id=\"videos\">\n",
    );
    for video in &videos {
        main.push_str(&video_row(video));
        main.push('\n');
    }
    main.push_str("</tbody>\n</table>\n");
    main.push_str(&pager);

    Ok(Html(layout(
        &Frame {
            title: &heading,
            current: Section::Likes(platform),
            status: &status,
            counts: &counts,
            board: &board,
            after,
            live: page == 1,
        },
        &main,
    )))
}

async fn video_page(
    State(app): State<App>,
    Path((platform, video_id)): Path<(String, String)>,
) -> Result<Html<String>, Failure> {
    let (found, download, counts, after) = app.db(|conn| {
        Ok((
            db::video(conn, &platform, &video_id)?,
            db::download_state(conn, &platform, &video_id)?,
            db::video_counts(conn)?,
            db::last_rowid(conn)?,
        ))
    })?;
    let (video, raw_json) = found.ok_or_else(not_found)?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let source = display_name(&video.platform);
    let title = short_title(&video);

    let thumbnail = if video.platform == "youtube" {
        format!(
            "<p><a href=\"{url}\"><img src=\"https://i.ytimg.com/vi/{id}/hqdefault.jpg\" \
             width=\"480\" height=\"360\" alt=\"\"></a></p>\n",
            url = escape(&video.url),
            id = escape(&video.video_id),
        )
    } else {
        String::new()
    };
    // TikTok captions run to several lines; the heading only has the first.
    let caption = match video.title.as_deref() {
        Some(full) if full.trim() != title => {
            format!("<p>{}</p>\n", escape(full).replace('\n', "<br>\n"))
        }
        _ => String::new(),
    };
    let on_source = format!("On {source}");
    let fields = [
        ("Author", author_html(&video)),
        (
            "Length",
            video.duration_seconds.map(duration).unwrap_or_default(),
        ),
        (
            "Kind",
            if video.kind == "photo" {
                "photo post"
            } else {
                "video"
            }
            .into(),
        ),
        (
            "Posted",
            video
                .posted_at
                .map_or_else(|| "not known yet".into(), |at| time(at, "datetime")),
        ),
        ("Added", time(video.liked_at, "datetime")),
        (
            &on_source,
            if video.available {
                "available"
            } else {
                "private or deleted"
            }
            .into(),
        ),
        ("Media", media_html(&video, download.as_ref(), &board)),
        ("ID", format!("<code>{}</code>", escape(&video.video_id))),
    ]
    .map(|(name, value)| format!("<dt>{name}</dt><dd>{value}</dd>\n"))
    .concat();
    // Fall back to the stored text if it somehow isn't valid JSON.
    let pretty = serde_json::from_str::<serde_json::Value>(&raw_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or(raw_json);
    let main = format!(
        "<p><a href=\"{back}\">← {source} likes</a></p>\n\
         <h1>{heading}</h1>\n\
         {thumbnail}{caption}\
         <p><a href=\"{url}\">Open on {source}</a></p>\n\
         <dl>\n{fields}</dl>\n\
         <details><summary>Raw JSON</summary><pre>{json}</pre></details>\n",
        back = list_href(Some(&video.platform), 1),
        heading = escape(&title),
        url = escape(&video.url),
        json = escape(&pretty),
    );

    Ok(Html(layout(
        &Frame {
            title: &title,
            current: Section::Likes(Some(&video.platform)),
            status: &status,
            counts: &counts,
            board: &board,
            after,
            live: false,
        },
        &main,
    )))
}

/// The video page's download button. Queues the video in the pipeline and
/// sends the browser back to the video's page, where its progress shows.
async fn request_download(
    State(app): State<App>,
    headers: HeaderMap,
    Path((platform, video_id)): Path<(String, String)>,
) -> Result<Response, Failure> {
    if !from_this_ui(&headers) {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "refusing a download request from another site".into(),
        ));
    }
    let (found, download, item) = app.db(|conn| {
        Ok((
            db::video(conn, &platform, &video_id)?,
            db::download_state(conn, &platform, &video_id)?,
            db::download_item(conn, &platform, &video_id)?,
        ))
    })?;
    let ((video, _), Some(item)) = (found.ok_or_else(not_found)?, item) else {
        return Err(not_found());
    };
    // A stale page's button for a video that has since been downloaded does
    // nothing; the page it lands on says so.
    if can_download(&video, download.as_ref()) {
        app.pipeline.request(Job {
            title: short_title(&video),
            item,
        })?;
    }
    let location = HeaderValue::try_from(format!("/videos/{platform}/{video_id}"))
        .map_err(anyhow::Error::from)?;
    Ok((StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response())
}

/// Any page on the web can make the browser POST to localhost. Browsers say
/// which site a POST comes from in `Origin`, so refuse it unless it's this UI.
fn from_this_ui(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Not sent by a browser, or by one too old to say.
        return true;
    };
    let ours = format!("http://{ADDR}");
    let ours_by_name = ours.replace("127.0.0.1", "localhost");
    origin
        .to_str()
        .is_ok_and(|origin| origin == ours || origin == ours_by_name)
}

async fn downloading_page(State(app): State<App>) -> Result<Html<String>, Failure> {
    let (counts, after) = app.db(|conn| Ok((db::video_counts(conn)?, db::last_rowid(conn)?)))?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let main = format!(
        "<h1>Downloading</h1>\n\
         <p>YouTube and TikTok each have a lane that downloads every liked video \
         not downloaded yet, newest first, one at a time: YouTube waits 30 seconds \
         between videos, TikTok 15 to 30. A video's Download now button downloads it \
         right away, alongside the lanes.</p>\n\
         <div id=\"downloads\">\n{}</div>\n",
        board_html(&board)
    );
    Ok(Html(layout(
        &Frame {
            title: "Downloading",
            current: Section::Downloading,
            status: &status,
            counts: &counts,
            board: &board,
            after,
            live: false,
        },
        &main,
    )))
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<i64>,
}

/// Live updates for an open page. Each newly stored video comes as a `video`
/// event holding its table row, with its rowid as the event id; then a
/// `counts` event, and on every change a `status` event with the header's
/// sync status. Whenever the download pipeline changes, a `downloads` event
/// brings what `downloads_json` describes.
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Failure> {
    // A reconnecting browser sends the last event id it got, so videos stored
    // while the server was down aren't missed. On first connect the page says
    // which video was newest when it was rendered.
    let resumed = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let after = match resumed.or(query.after) {
        Some(after) => after,
        None => app.db(db::last_rowid)?,
    };
    let mut status = app.status.clone();
    let mut board = app.pipeline.subscribe();
    // Send the current state right away, which also delivers anything stored
    // between rendering the page and connecting.
    status.mark_changed();
    board.mark_changed();
    let updates = stream::unfold(
        (app, status, board, after),
        |(app, mut status, mut board, after)| async move {
            let (events, after) = tokio::select! {
                changed = status.changed() => {
                    changed.ok()?;
                    let current = status.borrow_and_update().clone();
                    updates_since(&app, &current, after)
                }
                changed = board.changed() => {
                    changed.ok()?;
                    let current = board.borrow_and_update().clone();
                    let event = Event::default()
                        .event("downloads")
                        .data(downloads_json(&current));
                    (vec![event], after)
                }
            };
            Some((stream::iter(events), (app, status, board, after)))
        },
    )
    .flatten()
    .map(Ok);
    Ok(Sse::new(updates).keep_alive(KeepAlive::default()))
}

/// The events for one change: videos stored after `after`, the counts if
/// there were any, and the sync status. Also returns the new `after`.
fn updates_since(app: &App, status: &SyncStatus, after: i64) -> (Vec<Event>, i64) {
    let mut events = Vec::new();
    let mut newest = after;
    match app.db(|conn| Ok((db::videos_after(conn, after)?, db::video_counts(conn)?))) {
        Ok((videos, counts)) if !videos.is_empty() => {
            for video in &videos {
                events.push(
                    Event::default()
                        .event("video")
                        .id(video.rowid.to_string())
                        .data(video_row(video)),
                );
                newest = video.rowid;
            }
            events.push(Event::default().event("counts").data(counts_json(&counts)));
        }
        Ok(_) => {}
        Err(e) => eprintln!("web: live update failed: {e:#}"),
    }
    events.push(Event::default().event("status").data(status_html(status)));
    (events, newest)
}

/// The highlighted sidebar entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section<'a> {
    /// A list of likes: all of them (`None`), or one platform's.
    Likes(Option<&'a str>),
    Downloading,
}

/// What every page shares around its `main`.
struct Frame<'a> {
    title: &'a str,
    current: Section<'a>,
    status: &'a SyncStatus,
    counts: &'a [(String, usize)],
    board: &'a Board,
    /// The newest video's rowid when the page was rendered.
    after: i64,
    /// Whether new videos go straight into the table (the list's first page)
    /// rather than into a "new" notice.
    live: bool,
}

fn layout(frame: &Frame, main: &str) -> String {
    let nav = [(None, "All")]
        .into_iter()
        .chain(PLATFORMS.map(|(name, label)| (Some(name), label)))
        .map(|(name, label)| {
            let link = format!("<a href=\"{}\">{label}</a>", list_href(name, 1));
            let link = if frame.current == Section::Likes(name) {
                format!("<b>{link}</b>")
            } else {
                link
            };
            format!(
                "<li>{link} (<span id=\"count-{}\">{}</span>)</li>\n",
                name.unwrap_or("all"),
                count_of(frame.counts, name)
            )
        })
        .collect::<Vec<_>>()
        .concat();
    let downloading = "<a href=\"/downloading\">Downloading</a>";
    let downloading = if frame.current == Section::Downloading {
        format!("<b>{downloading}</b>")
    } else {
        downloading.into()
    };
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title} – joyscroller</title>\n\
         </head>\n\
         <body data-after=\"{after}\" data-platform=\"{platform}\" data-live=\"{live}\" data-size=\"{PAGE_SIZE}\">\n\
         <header>\n\
         <p><a href=\"/\"><b>joyscroller</b></a></p>\n\
         <p id=\"status\">{status}</p>\n\
         </header>\n\
         <hr>\n\
         <nav>\n<ul>\n{nav}<li>{downloading} (<span id=\"count-downloading\">{busy}</span>)</li>\n</ul>\n</nav>\n\
         <hr>\n\
         <main>\n{main}</main>\n\
         <script>{SCRIPT}</script>\n\
         </body>\n\
         </html>\n",
        title = escape(frame.title),
        after = frame.after,
        platform = match frame.current {
            Section::Likes(platform) => platform.unwrap_or(""),
            Section::Downloading => "",
        },
        busy = busy_count(frame.board),
        live = u8::from(frame.live),
        status = status_html(frame.status),
    )
}

/// Formats the `<time>` elements in the viewer's own time zone and applies
/// the live updates from `/events`.
const SCRIPT: &str = r#"
function showTimes(root) {
  for (const t of root.querySelectorAll("time[data-ts]")) {
    const d = new Date(Number(t.dataset.ts) * 1000);
    t.textContent = t.dataset.format === "date" ? d.toLocaleDateString()
      : t.dataset.format === "time" ? d.toLocaleTimeString()
      : d.toLocaleString();
  }
}
showTimes(document);

const page = document.body.dataset;
const events = new EventSource("/events?after=" + page.after);
let unseen = 0;
events.addEventListener("video", (e) => {
  const rows = document.getElementById("videos");
  const template = document.createElement("template");
  template.innerHTML = e.data;
  const row = template.content.firstElementChild;
  if (!rows || (page.platform && row.dataset.platform !== page.platform)) return;
  if (page.live === "1") {
    showTimes(row);
    rows.prepend(row);
    while (rows.rows.length > Number(page.size)) rows.lastElementChild.remove();
    const empty = document.getElementById("empty");
    if (empty) empty.hidden = true;
  } else {
    unseen += 1;
    document.getElementById("new-count").textContent = unseen;
    document.getElementById("new").hidden = false;
  }
});
events.addEventListener("counts", (e) => {
  for (const [name, count] of Object.entries(JSON.parse(e.data))) {
    const el = document.getElementById("count-" + name);
    if (el) el.textContent = count;
  }
});
events.addEventListener("status", (e) => {
  const el = document.getElementById("status");
  el.innerHTML = e.data;
  showTimes(el);
});
events.addEventListener("downloads", (e) => {
  const update = JSON.parse(e.data);
  document.getElementById("count-downloading").textContent = update.count;
  const board = document.getElementById("downloads");
  if (board) {
    board.innerHTML = update.board;
    showTimes(board);
  }
  // A video page: show its progress, and once it's out of the pipeline,
  // reload for the result.
  const media = document.getElementById("media");
  if (media) {
    const progress = update.videos[media.dataset.key];
    if (progress) {
      media.innerHTML = progress;
      media.dataset.busy = "1";
      showTimes(media);
    } else if (media.dataset.busy) {
      location.reload();
    }
  }
});
"#;

/// A video's row in the list table. Also sent as-is in `video` events.
fn video_row(video: &db::StoredVideo) -> String {
    let href = video_href(video);
    let thumbnail = if video.platform == "youtube" {
        format!(
            "<a href=\"{href}\"><img src=\"https://i.ytimg.com/vi/{}/mqdefault.jpg\" \
             width=\"160\" height=\"90\" loading=\"lazy\" alt=\"\"></a>",
            escape(&video.video_id)
        )
    } else {
        String::new()
    };
    let length = if video.kind == "photo" {
        "photo".into()
    } else {
        video.duration_seconds.map(duration).unwrap_or_default()
    };
    format!(
        "<tr data-platform=\"{platform}\"><td>{thumbnail}</td><td><a href=\"{href}\">{title}</a></td>\
         <td>{author}</td><td>{length}</td><td>{posted}</td><td>{added}</td><td>{source}</td></tr>",
        platform = escape(&video.platform),
        title = escape(&truncate(&short_title(video), 120)),
        author = escape(&author_label(video)),
        posted = video
            .posted_at
            .map(|at| time(at, "date"))
            .unwrap_or_default(),
        added = time(video.liked_at, "date"),
        source = display_name(&video.platform),
    )
}

/// The header's line about each platform's sync.
fn status_html(status: &SyncStatus) -> String {
    PLATFORMS
        .iter()
        .map(|(name, label)| {
            let state = match status.get(name) {
                None => "waiting to sync".into(),
                Some(SyncState::Running) => "syncing…".into(),
                Some(SyncState::Done { at, added }) => {
                    format!("synced at {}, {added} new", time(*at, "time"))
                }
                Some(SyncState::Failed { at, error }) => format!(
                    "sync failed at {}: {}",
                    time(*at, "time"),
                    escape(&error.replace(['\r', '\n'], " "))
                ),
            };
            format!("{label}: {state}")
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `{"all": n, "youtube": n, ...}`, for the sidebar's counts.
fn counts_json(counts: &[(String, usize)]) -> String {
    let mut object = serde_json::Map::new();
    object.insert("all".into(), count_of(counts, None).into());
    for (name, _) in PLATFORMS {
        object.insert(name.into(), count_of(counts, Some(name)).into());
    }
    serde_json::Value::Object(object).to_string()
}

/// How many videos `platform` has, or all platforms together.
fn count_of(counts: &[(String, usize)], platform: Option<&str>) -> usize {
    counts
        .iter()
        .filter(|(name, _)| platform.is_none_or(|wanted| name == wanted))
        .map(|(_, count)| count)
        .sum()
}

/// The pager line: where we are, and links to the neighboring pages.
fn pager(platform: Option<&str>, page: usize, pages: usize) -> String {
    let mut links = vec![format!("Page {page} of {pages}")];
    if page > 1 {
        links.push(format!(
            "<a href=\"{}\">← Newer</a>",
            list_href(platform, page - 1)
        ));
    }
    if page < pages {
        links.push(format!(
            "<a href=\"{}\">Older →</a>",
            list_href(platform, page + 1)
        ));
    }
    format!("<p>{}</p>\n", links.join(" · "))
}

fn list_href(platform: Option<&str>, page: usize) -> String {
    let mut params = Vec::new();
    if let Some(name) = platform {
        params.push(format!("platform={name}"));
    }
    if page > 1 {
        params.push(format!("page={page}"));
    }
    if params.is_empty() {
        "/".into()
    } else {
        format!("/?{}", params.join("&amp;"))
    }
}

fn video_href(video: &db::StoredVideo) -> String {
    format!(
        "/videos/{}/{}",
        escape(&video.platform),
        escape(&video.video_id)
    )
}

/// The first non-blank line of the title or caption, or what's missing.
fn short_title(video: &db::StoredVideo) -> String {
    let first_line = video
        .title
        .as_deref()
        .and_then(|title| title.lines().map(str::trim).find(|line| !line.is_empty()));
    match first_line {
        Some(line) => line.into(),
        None if !video.available => "(unavailable)".into(),
        None if video.platform == "tiktok" => "(no caption)".into(),
        None => "(no title)".into(),
    }
}

/// The author as the platform shows them: a channel name, or a TikTok @handle.
fn author_label(video: &db::StoredVideo) -> String {
    match (video.platform.as_str(), video.author_name.as_deref()) {
        (_, None) => String::new(),
        ("tiktok", Some(handle)) => format!("@{handle}"),
        (_, Some(name)) => name.into(),
    }
}

/// The author, linked to their channel or profile when we know where it is.
fn author_html(video: &db::StoredVideo) -> String {
    let label = escape(&author_label(video));
    let href = match video.platform.as_str() {
        "youtube" => video
            .author_id
            .as_deref()
            .map(|id| format!("https://www.youtube.com/channel/{id}")),
        "tiktok" => video
            .author_name
            .as_deref()
            .map(|handle| format!("https://www.tiktok.com/@{handle}")),
        _ => None,
    };
    match href {
        Some(href) if !label.is_empty() => format!("<a href=\"{}\">{label}</a>", escape(&href)),
        _ => label,
    }
}

/// Whether the download button makes sense: there's nothing saved yet, and
/// the platform still has the video.
fn can_download(video: &db::StoredVideo, download: Option<&db::DownloadState>) -> bool {
    video.available && download.is_none_or(|d| d.downloaded_at.is_none())
}

/// The video page's Media field: where its download stands in the pipeline,
/// or else in the database, with a button if it can be downloaded. `id` and
/// `data-key` are for the live updates.
fn media_html(
    video: &db::StoredVideo,
    download: Option<&db::DownloadState>,
    board: &Board,
) -> String {
    let (inner, busy) = match job_html(board, &video.platform, &video.video_id) {
        Some(html) => (html, " data-busy=\"1\""),
        None if can_download(video, download) => (
            format!(
                "{}\n<form method=\"post\" action=\"{}/download\"><button>Download now</button></form>",
                download_html(download),
                video_href(video)
            ),
            "",
        ),
        None => (download_html(download), ""),
    };
    format!(
        "<div id=\"media\" data-key=\"{}/{}\"{busy}>{inner}</div>",
        escape(&video.platform),
        escape(&video.video_id)
    )
}

/// What the pipeline is doing with a video, if it's on it.
fn job_html(board: &Board, platform: &str, video_id: &str) -> Option<String> {
    board.find_active(platform, video_id).map(activity_html)
}

/// What a lane is doing with a video, e.g. "downloading 45% of 12.3 MB,
/// 2.1 MB/s, 0:05 left (format 399), started 3:04:05 PM".
#[expect(clippy::cast_precision_loss, reason = "sizes for display")]
fn activity_html(active: &Active) -> String {
    let doing = active.stage.doing();
    let started = time(active.started_at, "time");
    let Some(progress) = &active.progress else {
        return format!("{doing}, started {started}");
    };
    let downloaded = progress.downloaded as f64;
    let amount = match progress.total {
        Some(total) if total > 0.0 => format!(
            "{:.0}% of {:.1} MB",
            (downloaded / total * 100.0).min(100.0),
            total / 1e6
        ),
        _ => format!("{:.1} MB", downloaded / 1e6),
    };
    let speed = progress
        .speed
        .map(|speed| format!(", {:.1} MB/s", speed / 1e6))
        .unwrap_or_default();
    let eta = progress
        .eta
        .and_then(|eta| i64::try_from(eta).ok())
        .map(|eta| format!(", {} left", duration(eta)))
        .unwrap_or_default();
    format!(
        "{doing} {amount}{speed}{eta} (format {}), started {started}",
        escape(&progress.format_id)
    )
}

/// How many videos are downloading, for the sidebar.
fn busy_count(board: &Board) -> usize {
    board.active.len()
}

/// The downloads page's live part: what each lane is doing, the requested
/// downloads, and what finished since the app started.
fn board_html(board: &Board) -> String {
    let job_link = |job: &Job| {
        format!(
            "{}: <a href=\"/videos/{}/{}\">{}</a>",
            display_name(&job.item.platform),
            escape(&job.item.platform),
            escape(&job.item.video_id),
            escape(&truncate(&job.title, 120))
        )
    };
    let lanes = board
        .lanes
        .iter()
        .map(|lane| match board.lane_download(lane.platform) {
            Some(a) => format!(
                "<li>{} — {}{}</li>\n",
                job_link(&a.job),
                activity_html(a),
                left_html(lane)
            ),
            None => format!(
                "<li>{}: {}{}</li>\n",
                display_name(lane.platform),
                lane_html(lane),
                left_html(lane)
            ),
        })
        .collect::<Vec<_>>()
        .concat();
    let requested: Vec<String> = board
        .active
        .iter()
        .filter(|a| a.requested)
        .map(|a| format!("<li>{} — {}</li>\n", job_link(&a.job), activity_html(a)))
        .collect();
    let requested = if requested.is_empty() {
        "<p>Nothing right now.</p>\n".into()
    } else {
        format!("<ul>\n{}</ul>\n", requested.concat())
    };
    let finished = if board.recent.is_empty() {
        "<p>Nothing since the app started.</p>\n".into()
    } else {
        let items = board
            .recent
            .iter()
            .map(|finished| {
                let outcome = match &finished.outcome {
                    Outcome::Downloaded { bytes, seconds } => {
                        #[expect(clippy::cast_precision_loss, reason = "a size for display")]
                        let megabytes = *bytes as f64 / 1e6;
                        let seconds = i64::try_from(*seconds).unwrap_or(i64::MAX);
                        format!("downloaded {megabytes:.1} MB in {}", duration(seconds))
                    }
                    Outcome::Failed(error) => {
                        format!("failed: {}", escape(&error.replace(['\r', '\n'], " ")))
                    }
                };
                format!(
                    "<li>{} {} — {outcome}</li>\n",
                    time(finished.at, "time"),
                    job_link(&finished.job)
                )
            })
            .collect::<Vec<_>>()
            .concat();
        format!("<ul>\n{items}</ul>\n")
    };
    format!(
        "<h2>Lanes</h2>\n<ul>\n{lanes}</ul>\n\
         <h2>Requested</h2>\n{requested}\
         <h2>Finished</h2>\n{finished}"
    )
}

/// What a lane that isn't downloading is doing.
fn lane_html(lane: &Lane) -> String {
    match lane.waiting {
        None => "starting".into(),
        Some((Wait::Pace, until)) => format!("next download at {}", time(until, "time")),
        Some((Wait::Failed, until)) => {
            format!("a download failed; trying again at {}", time(until, "time"))
        }
        Some((Wait::Outage, until)) => format!(
            "paused after several failed downloads in a row, until {}",
            time(until, "time")
        ),
        Some((Wait::Empty, _)) => "everything is downloaded".into(),
    }
}

/// How much of a lane's backlog is left, e.g. ", 1,234 to go".
fn left_html(lane: &Lane) -> String {
    match lane.left {
        Some(left) if left > 0 && lane.waiting.is_none_or(|(why, _)| why != Wait::Empty) => {
            format!(" ({} to go)", thousands(left))
        }
        _ => String::new(),
    }
}

/// `n` with thousands separators, e.g. `1,234`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The `downloads` event: `{"count": n, "board": html, "videos": {"platform/id": html}}`,
/// with the sidebar count, the downloads page's `board_html`, and the Media
/// field of each video in the pipeline.
fn downloads_json(board: &Board) -> String {
    let videos: serde_json::Map<String, serde_json::Value> = board
        .active
        .iter()
        .map(|active| &active.job)
        .filter_map(|job| {
            let (platform, id) = (&job.item.platform, &job.item.video_id);
            Some((
                format!("{platform}/{id}"),
                job_html(board, platform, id)?.into(),
            ))
        })
        .collect();
    serde_json::json!({
        "count": busy_count(board),
        "board": board_html(board),
        "videos": videos,
    })
    .to_string()
}

fn download_html(download: Option<&db::DownloadState>) -> String {
    let Some(download) = download else {
        return "not downloaded".into();
    };
    if let Some(at) = download.downloaded_at {
        #[expect(clippy::cast_precision_loss, reason = "a size for display")]
        let megabytes = download.bytes.unwrap_or(0) as f64 / 1e6;
        return format!(
            "downloaded {}, {megabytes:.1} MB, in <code>{}</code>",
            time(at, "datetime"),
            escape(download.dir.as_deref().unwrap_or(""))
        );
    }
    let tries = if download.attempts == 1 {
        "try"
    } else {
        "tries"
    };
    let last_error = download
        .last_error
        .as_deref()
        .map(|error| format!(", last error: {}", escape(error)))
        .unwrap_or_default();
    format!(
        "not downloaded; {} failed {tries}{last_error}",
        download.attempts
    )
}

/// A unix time, which the page's script shows in the viewer's time zone as a
/// `date`, a `time` of day, or a `datetime`.
fn time(ts: i64, format: &str) -> String {
    format!("<time data-ts=\"{ts}\" data-format=\"{format}\">{ts}</time>")
}

fn duration(seconds: i64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut short: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        short.push('…');
    }
    short
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(platform: &str, title: Option<&str>) -> db::StoredVideo {
        db::StoredVideo {
            rowid: 1,
            platform: platform.into(),
            video_id: "abc".into(),
            url: "https://example.com/abc".into(),
            title: title.map(Into::into),
            author_id: Some("UC1".into()),
            author_name: Some("someone".into()),
            duration_seconds: Some(75),
            available: true,
            kind: "video".into(),
            posted_at: None,
            liked_at: 1_700_000_000,
        }
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;"
        );
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration(5), "0:05");
        assert_eq!(duration(75), "1:15");
        assert_eq!(duration(3725), "1:02:05");
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("héllo", 5), "héllo");
        assert_eq!(truncate("héllo world", 5), "héllo…");
    }

    #[test]
    fn titles_use_the_first_nonblank_line() {
        let caption = video("tiktok", Some("\n  first line \nsecond #tag"));
        assert_eq!(short_title(&caption), "first line");
        assert_eq!(short_title(&video("tiktok", None)), "(no caption)");
        assert_eq!(short_title(&video("youtube", None)), "(no title)");
        let gone = db::StoredVideo {
            available: false,
            ..video("youtube", None)
        };
        assert_eq!(short_title(&gone), "(unavailable)");
    }

    #[test]
    fn rows_escape_titles_and_tag_their_platform() {
        let row = video_row(&video("youtube", Some("<b>hi</b>")));
        assert!(row.starts_with("<tr data-platform=\"youtube\">"));
        assert!(row.contains("&lt;b&gt;hi&lt;/b&gt;"));
        assert!(row.contains("i.ytimg.com/vi/abc/"));
        assert!(!row.contains('\n'));
        let tiktok = video_row(&video("tiktok", Some("x")));
        assert!(!tiktok.contains("<img"));
        assert!(tiktok.contains("@someone"));
    }

    #[test]
    fn links_keep_the_filter_across_pages() {
        assert_eq!(list_href(None, 1), "/");
        assert_eq!(list_href(None, 2), "/?page=2");
        assert_eq!(list_href(Some("tiktok"), 1), "/?platform=tiktok");
        assert_eq!(list_href(Some("tiktok"), 3), "/?platform=tiktok&amp;page=3");
        let first = pager(None, 1, 3);
        assert!(!first.contains("Newer") && first.contains("Older"));
        let last = pager(None, 3, 3);
        assert!(last.contains("Newer") && !last.contains("Older"));
    }

    #[test]
    fn counts_cover_every_platform() {
        let counts = [("youtube".to_string(), 3)];
        assert_eq!(count_of(&counts, None), 3);
        assert_eq!(count_of(&counts, Some("tiktok")), 0);
        assert_eq!(counts_json(&counts), r#"{"all":3,"tiktok":0,"youtube":3}"#);
    }

    #[test]
    fn status_shows_each_platform_in_order() {
        let mut status = SyncStatus::default();
        status.set("tiktok", SyncState::Running);
        status.set(
            "youtube",
            SyncState::Failed {
                at: 0,
                error: "logged\nout".into(),
            },
        );
        status.set("tiktok", SyncState::Done { at: 0, added: 2 });
        let html = status_html(&status);
        assert!(html.starts_with("YouTube: sync failed at"));
        assert!(html.contains("logged out"));
        assert!(html.contains("TikTok: synced at"));
        assert!(html.contains("2 new"));
    }

    fn job(video: &db::StoredVideo) -> Job {
        Job {
            item: db::PendingDownload {
                platform: video.platform.clone(),
                video_id: video.video_id.clone(),
                url: video.url.clone(),
                kind: video.kind.clone(),
                title: video.title.clone(),
                author_name: video.author_name.clone(),
                liked_on: "2026-10-12".into(),
            },
            title: "<b>".into(),
        }
    }

    #[test]
    fn media_offers_a_button_only_when_there_is_nothing_saved() {
        let board = Board::default();
        let fresh = video("youtube", None);
        assert!(
            media_html(&fresh, None, &board).contains("action=\"/videos/youtube/abc/download\"")
        );

        let failed = db::DownloadState {
            attempts: 2,
            last_error: Some("429".into()),
            downloaded_at: None,
            dir: None,
            bytes: None,
        };
        let html = media_html(&fresh, Some(&failed), &board);
        assert!(html.contains("2 failed tries, last error: 429"));
        assert!(html.contains("<button>"));

        let saved = db::DownloadState {
            downloaded_at: Some(1),
            dir: Some("youtube/abc".into()),
            bytes: Some(1),
            ..failed
        };
        assert!(!media_html(&fresh, Some(&saved), &board).contains("<button>"));
        let gone = db::StoredVideo {
            available: false,
            ..fresh
        };
        assert!(!media_html(&gone, None, &board).contains("<button>"));
    }

    fn lane(platform: &'static str, left: Option<usize>, waiting: Option<(Wait, i64)>) -> Lane {
        Lane {
            platform,
            left,
            waiting,
        }
    }

    #[test]
    fn media_shows_the_pipeline_instead_of_a_button() {
        let youtube = video("youtube", None);
        let tiktok = video("tiktok", None);
        let mut board = Board {
            lanes: vec![
                lane("youtube", Some(1234), None),
                lane("tiktok", Some(3), Some((Wait::Pace, 0))),
            ],
            ..Board::default()
        };
        let html = media_html(&tiktok, None, &board);
        assert!(html.contains("<button>Download now</button>"));
        assert!(!html.contains("data-busy"));

        let downloading = |job, requested| Active {
            job,
            requested,
            stage: crate::pipeline::Stage::Download,
            started_at: 0,
            progress: Some(crate::download::Progress {
                downloaded: 2_500_000,
                total: Some(10_000_000.0),
                speed: Some(1_000_000.0),
                eta: Some(8),
                format_id: "399".into(),
            }),
        };
        board.active.push(downloading(job(&youtube), false));
        board.active.push(downloading(job(&tiktok), true));
        let html = media_html(&youtube, None, &board);
        assert!(
            html.contains("downloading 25% of 10.0 MB, 1.0 MB/s, 0:08 left (format 399), started")
        );
        assert!(html.contains("data-busy=\"1\""));
        assert!(!html.contains("<button>"));

        let update: serde_json::Value = serde_json::from_str(&downloads_json(&board)).unwrap();
        assert_eq!(update["count"], 2);
        assert!(
            update["videos"]["tiktok/abc"]
                .as_str()
                .unwrap()
                .starts_with("downloading")
        );
        let page = update["board"].as_str().unwrap();
        // Titles are escaped; each lane shows what it's on, and the requested
        // download has its own list.
        assert!(page.contains(
            "<h2>Lanes</h2>\n<ul>\n<li>YouTube: <a href=\"/videos/youtube/abc\">&lt;b&gt;</a> — downloading"
        ));
        assert!(page.contains("(1,234 to go)</li>"));
        assert!(page.contains("<li>TikTok: next download at <time"));
        assert!(
            page.contains("<h2>Requested</h2>\n<ul>\n<li>TikTok: <a href=\"/videos/tiktok/abc\">")
        );
        assert!(page.contains("Nothing since the app started."));
    }

    #[test]
    fn lanes_say_why_they_wait() {
        assert_eq!(lane_html(&lane("youtube", None, None)), "starting");
        assert!(lane_html(&lane("youtube", None, Some((Wait::Outage, 0)))).starts_with("paused"));
        let done = lane("youtube", Some(0), Some((Wait::Empty, 0)));
        assert_eq!(lane_html(&done), "everything is downloaded");
        assert_eq!(left_html(&done), "");
        assert_eq!(left_html(&lane("youtube", Some(12), None)), " (12 to go)");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(999), "999");
    }

    #[test]
    fn board_lists_finished_videos() {
        let mut board = Board::default();
        board.recent.push_front(crate::pipeline::Finished {
            job: job(&video("tiktok", None)),
            at: 0,
            outcome: Outcome::Failed("bad\nthing".into()),
        });
        board.recent.push_front(crate::pipeline::Finished {
            job: job(&video("youtube", None)),
            at: 0,
            outcome: Outcome::Downloaded {
                bytes: 12_300_000,
                seconds: 75,
            },
        });
        let html = board_html(&board);
        assert!(html.contains("Nothing right now."));
        let downloaded = html.find("downloaded 12.3 MB in 1:15").unwrap();
        let failed = html.find("failed: bad thing").unwrap();
        assert!(downloaded < failed);
    }

    #[test]
    fn refuses_download_requests_from_other_sites() {
        let mut headers = HeaderMap::new();
        assert!(from_this_ui(&headers));
        for (origin, ok) in [
            ("http://127.0.0.1:4774", true),
            ("http://localhost:4774", true),
            ("https://evil.example", false),
            ("null", false),
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
            assert_eq!(from_this_ui(&headers), ok, "{origin}");
        }
    }
}
