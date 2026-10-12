//! The localhost web UI: plain server-rendered HTML. The list page pages
//! through the liked videos, newest first, on all platforms or one; each video
//! has its own page. Every open page keeps a Server-Sent Events connection, over
//! which newly stored videos, the per-platform counts and what each sync is
//! doing arrive as soon as they change.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{Stream, StreamExt, stream};
use rusqlite::Connection;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::db;

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
) -> Result<()> {
    let app = App {
        db: Arc::new(Mutex::new(conn)),
        status,
    };
    let router = Router::new()
        .route("/", get(list_page))
        .route("/videos/{platform}/{video_id}", get(video_page))
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
            current: platform,
            status: &status,
            counts: &counts,
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
        ("Media", download_html(download.as_ref())),
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
            current: Some(&video.platform),
            status: &status,
            counts: &counts,
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
/// sync status.
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
    // Send the current state right away, which also delivers anything stored
    // between rendering the page and connecting.
    status.mark_changed();
    let updates = stream::unfold(
        (app, status, after),
        |(app, mut status, after)| async move {
            status.changed().await.ok()?;
            let current = status.borrow_and_update().clone();
            let (events, after) = updates_since(&app, &current, after);
            Some((stream::iter(events), (app, status, after)))
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

/// What every page shares around its `main`.
struct Frame<'a> {
    title: &'a str,
    /// The highlighted sidebar entry; `None` is All.
    current: Option<&'a str>,
    status: &'a SyncStatus,
    counts: &'a [(String, usize)],
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
            let link = if name == frame.current {
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
         <nav>\n<ul>\n{nav}</ul>\n</nav>\n\
         <hr>\n\
         <main>\n{main}</main>\n\
         <script>{SCRIPT}</script>\n\
         </body>\n\
         </html>\n",
        title = escape(frame.title),
        after = frame.after,
        platform = frame.current.unwrap_or(""),
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
}
