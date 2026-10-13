//! The localhost web UI: plain server-rendered HTML. The list page pages
//! through the liked videos, newest first, on all platforms or one; each video
//! has its own page, with a button that has the download pipeline download it
//! right away, and `/downloading` shows what the pipeline is doing. Every open page
//! keeps a Server-Sent Events connection, over which newly stored videos, the
//! per-platform counts, what each sync is doing and the pipeline's progress
//! arrive as soon as they change. The pages are dressed as a Windows XP
//! desktop with a Nickelodeon clock radio in the side panel; the stylesheet,
//! fonts and pictures in `assets/` are built into the executable.

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
        .route("/assets/{name}", get(asset))
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

/// The stylesheet, fonts and pictures, as (name, type, contents). They're
/// built into the executable, so the app stays a single file.
const ASSETS: [(&str, &str, &[u8]); 8] = [
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/style.css"),
    ),
    (
        "desktop.svg",
        "image/svg+xml",
        include_bytes!("../assets/desktop.svg"),
    ),
    (
        "radio.webp",
        "image/webp",
        include_bytes!("../assets/radio.webp"),
    ),
    (
        "splat.svg",
        "image/svg+xml",
        include_bytes!("../assets/splat.svg"),
    ),
    (
        "luckiest-guy.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/luckiest-guy.woff2"),
    ),
    (
        "fredoka.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/fredoka.woff2"),
    ),
    (
        "dseg7-classic-bold.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/dseg7-classic-bold.woff2"),
    ),
    (
        "dseg14-classic-bold.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/dseg14-classic-bold.woff2"),
    ),
];

async fn asset(Path(name): Path<String>) -> Result<Response, Failure> {
    let &(_, content_type, body) = ASSETS
        .iter()
        .find(|(file, ..)| *file == name)
        .ok_or_else(not_found)?;
    // Fonts never change. The rest changes with the app, so the browser
    // checks for a new copy each time.
    let cache = if content_type.starts_with("font/") {
        "max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response())
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
        "<p id=\"new\" class=\"balloon\" hidden><a href=\"{}\"><span id=\"new-count\">0</span> new likes — show them</a></p>\n\
         {pager}",
        list_href(platform, 1)
    );
    main.push_str(
        "<table class=\"list\">\n<thead><tr><th></th><th>Title</th><th class=\"author\">Author</th>\
         <th class=\"num\">Length</th><th class=\"date\">Posted</th><th class=\"date\">Added</th>\
         <th class=\"source\">Source</th></tr></thead>\n<tbody id=\"videos\">\n",
    );
    for video in &videos {
        main.push_str(&video_row(video));
        main.push('\n');
    }
    main.push_str("</tbody>\n</table>\n");
    if videos.is_empty() {
        main.push_str(if page == 1 {
            "<p id=\"empty\" class=\"empty\">No liked videos yet. They show up here as each sync finishes.</p>\n"
        } else {
            "<p id=\"empty\" class=\"empty\">Nothing on this page.</p>\n"
        });
    }
    main.push_str(&pager);

    Ok(Html(layout(
        &Frame {
            title: &heading,
            current: Section::Likes(platform),
            status: &status,
            counts: &counts,
            board: &board,
            footer: format!(
                "<span data-count=\"{}\">{}</span> likes",
                platform.unwrap_or("all"),
                count_of(&counts, platform)
            ),
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

    let thumbnail = thumbnail(&video, "hqdefault", &escape(&video.url));
    // TikTok captions run to several lines; the heading only has the first.
    let caption = match video.title.as_deref() {
        Some(full) if full.trim() != title => {
            format!(
                "<p class=\"caption\">{}</p>\n",
                escape(full).replace('\n', "<br>\n")
            )
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
        ("ID", format!("<code>{}</code>", escape(&video.video_id))),
    ]
    .map(|(name, value)| format!("<dt>{name}</dt><dd>{value}</dd>\n"))
    .concat();
    // Fall back to the stored text if it somehow isn't valid JSON.
    let pretty = serde_json::from_str::<serde_json::Value>(&raw_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or(raw_json);
    let main = format!(
        "<div class=\"video\">\n\
         <div class=\"preview\">\n{thumbnail}\n\
         <a class=\"button button-go\" href=\"{url}\">{icon}Open on {source}</a>\n\
         <a class=\"button\" href=\"{back}\">← {source} likes</a>\n\
         </div>\n\
         <div>\n\
         <h2 class=\"video-title\">{heading}</h2>\n\
         {caption}\
         <fieldset class=\"group\"><legend>Media</legend>{media}</fieldset>\n\
         <fieldset class=\"group\"><legend>Details</legend><dl class=\"facts\">\n{fields}</dl></fieldset>\n\
         <details class=\"raw\"><summary>Raw JSON</summary><pre>{json}</pre></details>\n\
         </div>\n\
         </div>\n",
        icon = icon(Section::Likes(Some(&video.platform))),
        back = list_href(Some(&video.platform), 1),
        heading = escape(&title),
        url = escape(&video.url),
        media = media_html(&video, download.as_ref(), &board),
        json = escape(&pretty),
    );

    Ok(Html(layout(
        &Frame {
            title: &title,
            current: Section::Likes(Some(&video.platform)),
            status: &status,
            counts: &counts,
            board: &board,
            footer: format!("{source} · {}", escape(&author_label(&video))),
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
    let about = if board.lanes.is_empty() {
        "Videos download when you click Download now on their page, right away, \
         several at a time if you like."
    } else {
        "YouTube and TikTok each have a lane that downloads every liked video \
         not downloaded yet, newest first, one at a time: YouTube waits 30 seconds \
         between videos, TikTok 15 to 30. A video's Download now button downloads it \
         right away, alongside the lanes."
    };
    let main = format!(
        "<p class=\"intro\">{about}</p>\n\
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
            footer: format!(
                "<span data-count=\"downloading\">{}</span> downloading",
                busy_count(&board)
            ),
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
    /// The window's status bar, under its contents.
    footer: String,
    /// The newest video's rowid when the page was rendered.
    after: i64,
    /// Whether new videos go straight into the table (the list's first page)
    /// rather than into a "new" notice.
    live: bool,
}

/// One of the places the top bar and side panel link to.
struct NavEntry {
    section: Section<'static>,
    href: String,
    label: &'static str,
    /// What its `data-count` element is called in the live updates.
    count_name: &'static str,
    count: usize,
}

fn nav_entries(frame: &Frame) -> Vec<NavEntry> {
    [(None, "All likes")]
        .into_iter()
        .chain(PLATFORMS.map(|(name, label)| (Some(name), label)))
        .map(|(name, label)| NavEntry {
            section: Section::Likes(name),
            href: list_href(name, 1),
            label,
            count_name: name.unwrap_or("all"),
            count: count_of(frame.counts, name),
        })
        .chain([NavEntry {
            section: Section::Downloading,
            href: "/downloading".into(),
            label: "Downloading",
            count_name: "downloading",
            count: busy_count(frame.board),
        }])
        .collect()
}

/// ` aria-current="page"` on the link to the page we're on.
fn current(frame: &Frame, section: Section) -> &'static str {
    if frame.current == section {
        " aria-current=\"page\""
    } else {
        ""
    }
}

/// The side panel, in the manner of MSN's: a clock radio (a Blender render,
/// see `assets/blender/radio.py`) whose LCD shows the time, the menu, and
/// each platform's sync.
fn panel_html(frame: &Frame, entries: &[NavEntry]) -> String {
    let menu = |entries: &[NavEntry]| {
        entries
            .iter()
            .map(|entry| {
                format!(
                    "<li><a href=\"{}\"{}>{}{}\
                     <span class=\"count\" data-count=\"{}\">{}</span></a></li>\n",
                    entry.href,
                    current(frame, entry.section),
                    icon(entry.section),
                    entry.label,
                    entry.count_name,
                    entry.count
                )
            })
            .collect::<Vec<_>>()
            .concat()
    };
    let (likes, downloads) = entries.split_at(entries.len() - 1);
    format!(
        "<aside class=\"panel\">\n\
         <div class=\"radio\"><img src=\"/assets/radio.webp\" width=\"960\" height=\"691\" alt=\"\">{RADIO_LCD}</div>\n\
         <nav>\n\
         <h2>Likes</h2>\n<ul class=\"menu\">\n{likes}</ul>\n\
         <h2>Downloads</h2>\n<ul class=\"menu\">\n{downloads}</ul>\n\
         </nav>\n\
         <h2>Sync</h2>\n<ul class=\"sync\" id=\"status\">\n{status}</ul>\n\
         </aside>\n",
        likes = menu(likes),
        downloads = menu(downloads),
        status = status_html(frame.status),
    )
}

/// The page shell, dressed as a Windows XP desktop: a top bar with the logo
/// and big icons, the side panel, the page in a window, and a taskbar.
/// Elements with `data-count` hold a count the live updates change.
fn layout(frame: &Frame, main: &str) -> String {
    let entries = nav_entries(frame);
    let quick = entries
        .iter()
        .map(|entry| {
            format!(
                "<a href=\"{}\"{}>{}{}</a>\n",
                entry.href,
                current(frame, entry.section),
                icon(entry.section),
                entry.label
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
         <link rel=\"stylesheet\" href=\"/assets/style.css\">\n\
         <link rel=\"icon\" href=\"/assets/splat.svg\">\n\
         </head>\n\
         <body data-after=\"{after}\" data-platform=\"{platform}\" data-live=\"{live}\" data-size=\"{PAGE_SIZE}\">\n\
         <header class=\"topbar\">\n\
         <a class=\"logo\" href=\"/\"><span class=\"splat\">joyscroller</span>\
         <span class=\"edition\">Archive Edition</span></a>\n\
         <nav class=\"quick\">\n{quick}</nav>\n\
         </header>\n\
         <div class=\"shell\">\n\
         {panel}\
         <main class=\"window\">\n\
         <div class=\"titlebar\">{icon}<h1>{title}</h1>\
         <div class=\"controls\" aria-hidden=\"true\"><span>_</span><span>□</span>\
         <a class=\"close\" href=\"/\" tabindex=\"-1\">✕</a></div></div>\n\
         <div class=\"window-body\">\n{main}</div>\n\
         <div class=\"statusbar\"><span>{footer}</span><span>Local archive</span></div>\n\
         </main>\n\
         </div>\n\
         <footer class=\"taskbar\">\n\
         <a class=\"start\" href=\"/\"><img src=\"/assets/splat.svg\" alt=\"\">start</a>\n\
         <span class=\"task\">{icon}<span>{title}</span></span>\n\
         <span class=\"tray\" id=\"tray-clock\"></span>\n\
         </footer>\n\
         <script>{SCRIPT}</script>\n\
         </body>\n\
         </html>\n",
        title = escape(frame.title),
        after = frame.after,
        platform = match frame.current {
            Section::Likes(platform) => platform.unwrap_or(""),
            Section::Downloading => "",
        },
        live = u8::from(frame.live),
        panel = panel_html(frame, &entries),
        icon = icon(frame.current),
        footer = frame.footer,
    )
}

/// The live clock on the radio's LCD. The render leaves the glass blank;
/// this draws the lit digits in a 100 x 45.5 box and maps it onto the glass,
/// seen at an angle, with the `svg_matrix` that `assets/blender/radio.py`
/// writes to its JSON. The viewBox is the render's size, so it scales with
/// the picture.
const RADIO_LCD: &str = "<svg class=\"radio-lcd\" viewBox=\"0 0 960 691\" aria-hidden=\"true\">\
<g transform=\"matrix(1.98584 -0.19589 0.00196 1.91723 462.30878 326.37076)\">\
<text class=\"lcd-ghost\" x=\"84\" y=\"33\">88:88</text>\
<text class=\"lcd-digits\" id=\"lcd-clock\" x=\"84\" y=\"33\">!!:!!</text>\
<text class=\"lcd-ampm\" id=\"lcd-ampm\" x=\"95\" y=\"15\"></text>\
</g></svg>";

/// A glossy little icon for a section, drawn inline. Flat fills only: an
/// SVG gradient is looked up by id across the page, and breaks when the
/// copy that defines it is hidden.
fn icon(section: Section) -> &'static str {
    match section {
        Section::Likes(Some("youtube")) => {
            r##"<svg class="icon" viewBox="0 0 32 32" aria-hidden="true"><rect x="2" y="6" width="28" height="20" rx="6" fill="#e62117" stroke="#8f0c06" stroke-width="1.5"/><path d="M4 12 Q16 8 28 12 V10 Q28 7.5 25 7.5 H7 Q4 7.5 4 10Z" fill="#fff" opacity=".3"/><path d="M13 11 V21 L22 16Z" fill="#fff"/></svg>"##
        }
        Section::Likes(Some("tiktok")) => {
            r##"<svg class="icon" viewBox="0 0 32 32" aria-hidden="true"><rect x="3" y="3" width="26" height="26" rx="7" fill="#161616" stroke="#000" stroke-width="1.5"/><g fill="none" stroke-width="3" stroke-linecap="round"><path d="M17 8 V20 M17 8 Q19 12 23 12" stroke="#25f4ee" transform="translate(-1 -1)"/><path d="M17 8 V20 M17 8 Q19 12 23 12" stroke="#fe2c55" transform="translate(1 1)"/><path d="M17 8 V20 M17 8 Q19 12 23 12" stroke="#fff"/></g><circle cx="13.5" cy="21" r="3.8" fill="none" stroke="#fff" stroke-width="3"/></svg>"##
        }
        Section::Likes(_) => {
            r##"<svg class="icon" viewBox="0 0 32 32" aria-hidden="true"><path d="M16 28 C6 20 2 14.5 2 10 A6.8 6.8 0 0 1 16 7.5 A6.8 6.8 0 0 1 30 10 C30 14.5 26 20 16 28Z" fill="#ff3d7f" stroke="#a3123f" stroke-width="1.5"/><ellipse cx="9.5" cy="10.5" rx="4" ry="2.4" fill="#fff" opacity=".6" transform="rotate(-30 9.5 10.5)"/></svg>"##
        }
        Section::Downloading => {
            r##"<svg class="icon" viewBox="0 0 32 32" aria-hidden="true"><circle cx="16" cy="16" r="14" fill="#3aa52c" stroke="#1d6a14" stroke-width="1.5"/><ellipse cx="16" cy="9" rx="10" ry="5" fill="#fff" opacity=".3"/><path d="M16 8 V20 M10.5 15 L16 20.5 L21.5 15 M10 24.5 H22" fill="none" stroke="#fff" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"##
        }
    }
}

/// Formats the `<time>` elements in the viewer's own time zone, keeps the
/// clocks going and applies the live updates from `/events`.
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

// The LCD's font draws "!" as a blank digit, so a one-digit hour stays
// right-aligned over the unlit "88:88".
function tick() {
  const now = new Date();
  const hour = now.getHours() % 12 || 12;
  const minutes = String(now.getMinutes()).padStart(2, "0");
  document.getElementById("lcd-clock").textContent = (hour < 10 ? "!" : "") + hour + ":" + minutes;
  document.getElementById("lcd-ampm").textContent = now.getHours() < 12 ? "AM" : "PM";
  document.getElementById("tray-clock").textContent =
    now.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}
tick();
setInterval(tick, 1000);

function setCount(name, count) {
  for (const el of document.querySelectorAll('[data-count="' + name + '"]')) el.textContent = count;
}

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
  for (const [name, count] of Object.entries(JSON.parse(e.data))) setCount(name, count);
});
events.addEventListener("status", (e) => {
  const el = document.getElementById("status");
  el.innerHTML = e.data;
  showTimes(el);
});
events.addEventListener("downloads", (e) => {
  const update = JSON.parse(e.data);
  setCount("downloading", update.count);
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

/// A video's picture, linked to `href`: YouTube's thumbnail at `size`
/// (`mqdefault`, `hqdefault`), or for TikTok, whose picture links expire, a
/// colored tile.
fn thumbnail(video: &db::StoredVideo, size: &str, href: &str) -> String {
    if video.platform == "youtube" {
        format!(
            "<a href=\"{href}\"><img src=\"https://i.ytimg.com/vi/{}/{size}.jpg\" \
             loading=\"lazy\" alt=\"\"></a>",
            escape(&video.video_id)
        )
    } else {
        format!(
            "<a class=\"nothumb\" data-kind=\"{}\" href=\"{href}\" tabindex=\"-1\" aria-hidden=\"true\"></a>",
            escape(&video.kind)
        )
    }
}

/// A video's row in the list table. Also sent as-is in `video` events.
fn video_row(video: &db::StoredVideo) -> String {
    let href = video_href(video);
    let thumbnail = thumbnail(video, "mqdefault", &href);
    let length = if video.kind == "photo" {
        "photo".into()
    } else {
        video.duration_seconds.map(duration).unwrap_or_default()
    };
    format!(
        "<tr data-platform=\"{platform}\"><td class=\"thumb\">{thumbnail}</td>\
         <td class=\"title\"><a href=\"{href}\">{title}</a></td><td class=\"author\">{author}</td>\
         <td class=\"num\">{length}</td><td class=\"date\">{posted}</td><td class=\"date\">{added}</td>\
         <td class=\"source\"><span class=\"badge badge-{platform}\">{source}</span></td></tr>",
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

/// The side panel's list of what each platform's sync is doing, each with
/// a light: green when synced, blinking yellow while syncing, red when the
/// last sync failed.
fn status_html(status: &SyncStatus) -> String {
    PLATFORMS
        .iter()
        .map(|(name, label)| {
            let (light, state) = match status.get(name) {
                None => ("waiting", "waiting to sync".into()),
                Some(SyncState::Running) => ("running", "syncing…".into()),
                Some(SyncState::Done { at, added }) => (
                    "done",
                    format!("synced at {}, {added} new", time(*at, "time")),
                ),
                Some(SyncState::Failed { at, error }) => (
                    "failed",
                    format!(
                        "failed at {}: {}",
                        time(*at, "time"),
                        escape(&error.replace(['\r', '\n'], " "))
                    ),
                ),
            };
            format!(
                "<li class=\"sync-{light}\"><span class=\"led\"></span><b>{label}</b>\
                 <span class=\"what\">{state}</span></li>\n"
            )
        })
        .collect::<Vec<_>>()
        .concat()
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
    let newer = if page > 1 {
        format!(
            "<a class=\"button\" href=\"{}\">← Newer</a>",
            list_href(platform, page - 1)
        )
    } else {
        String::new()
    };
    let older = if page < pages {
        format!(
            "<a class=\"button\" href=\"{}\">Older →</a>",
            list_href(platform, page + 1)
        )
    } else {
        String::new()
    };
    format!(
        "<nav class=\"pager\"><span class=\"where\">Page {} of {}</span>{newer}{older}</nav>\n",
        thousands(page),
        thousands(pages)
    )
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
                "<form method=\"post\" action=\"{}/download\"><button class=\"dome\">Download now</button></form>\n\
                 <span>{}</span>",
                video_href(video),
                download_html(download)
            ),
            "",
        ),
        None => (download_html(download), ""),
    };
    format!(
        "<div class=\"media\" id=\"media\" data-key=\"{}/{}\"{busy}>{inner}</div>",
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
    // A bar that slides back and forth when how far along isn't known.
    let busy = "<span class=\"progress progress-busy\"><span></span></span>";
    let Some(progress) = &active.progress else {
        return format!("{doing}, started {started}{busy}");
    };
    let downloaded = progress.downloaded as f64;
    let (amount, bar) = match progress.total {
        Some(total) if total > 0.0 => {
            let percent = (downloaded / total * 100.0).min(100.0);
            (
                format!("{percent:.0}% of {:.1} MB", total / 1e6),
                format!(
                    "<span class=\"progress\"><span style=\"width: {percent:.1}%\"></span></span>"
                ),
            )
        }
        _ => (format!("{:.1} MB", downloaded / 1e6), busy.into()),
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
        "{doing} {amount}{speed}{eta} (format {}), started {started}{bar}",
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
    let lanes: String = board
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
        .collect();
    let lanes = if lanes.is_empty() {
        "<p>Automatic downloads are off.</p>\n".into()
    } else {
        format!("<ul class=\"jobs\">\n{lanes}</ul>\n")
    };
    let requested: Vec<String> = board
        .active
        .iter()
        .filter(|a| a.requested)
        .map(|a| format!("<li>{} — {}</li>\n", job_link(&a.job), activity_html(a)))
        .collect();
    let requested = if requested.is_empty() {
        "<p>Nothing right now.</p>\n".into()
    } else {
        format!("<ul class=\"jobs\">\n{}</ul>\n", requested.concat())
    };
    let finished = if board.recent.is_empty() {
        "<p>Nothing since the app started.</p>\n".into()
    } else {
        let items = board
            .recent
            .iter()
            .map(|finished| {
                let (class, outcome) = match &finished.outcome {
                    Outcome::Downloaded { bytes, seconds } => {
                        #[expect(clippy::cast_precision_loss, reason = "a size for display")]
                        let megabytes = *bytes as f64 / 1e6;
                        let seconds = i64::try_from(*seconds).unwrap_or(i64::MAX);
                        (
                            "downloaded",
                            format!("downloaded {megabytes:.1} MB in {}", duration(seconds)),
                        )
                    }
                    Outcome::Failed(error) => (
                        "failed",
                        format!("failed: {}", escape(&error.replace(['\r', '\n'], " "))),
                    ),
                };
                format!(
                    "<li><span class=\"when\">{}</span>{} — <span class=\"outcome-{class}\">{outcome}</span></li>\n",
                    time(finished.at, "time"),
                    job_link(&finished.job)
                )
            })
            .collect::<Vec<_>>()
            .concat();
        format!("<ul class=\"jobs\">\n{items}</ul>\n")
    };
    format!(
        "<h2>Lanes</h2>\n{lanes}\
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
        assert!(row.contains("badge-youtube"));
        assert!(row.contains("&lt;b&gt;hi&lt;/b&gt;"));
        assert!(row.contains("i.ytimg.com/vi/abc/"));
        assert!(!row.contains('\n'));
        let tiktok = video_row(&video("tiktok", Some("x")));
        assert!(!tiktok.contains("<img"));
        assert!(tiktok.contains("class=\"nothumb\" data-kind=\"video\""));
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
        assert!(
            html.starts_with("<li class=\"sync-failed\"><span class=\"led\"></span><b>YouTube</b>")
        );
        assert!(html.contains("failed at"));
        assert!(html.contains("logged out"));
        let tiktok = html.find("<li class=\"sync-done\"><span class=\"led\"></span><b>TikTok</b>");
        assert!(tiktok.is_some_and(|at| at > 0));
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
        assert!(html.contains("<button"));

        let saved = db::DownloadState {
            downloaded_at: Some(1),
            dir: Some("youtube/abc".into()),
            bytes: Some(1),
            ..failed
        };
        assert!(!media_html(&fresh, Some(&saved), &board).contains("<button"));
        let gone = db::StoredVideo {
            available: false,
            ..fresh
        };
        assert!(!media_html(&gone, None, &board).contains("<button"));
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
        assert!(html.contains("Download now</button>"));
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
        assert!(!html.contains("<button"));

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
            "<h2>Lanes</h2>\n<ul class=\"jobs\">\n<li>YouTube: <a href=\"/videos/youtube/abc\">&lt;b&gt;</a> — downloading"
        ));
        assert!(page.contains("(1,234 to go)</li>"));
        assert!(page.contains("<li>TikTok: next download at <time"));
        assert!(page.contains(
            "<h2>Requested</h2>\n<ul class=\"jobs\">\n<li>TikTok: <a href=\"/videos/tiktok/abc\">"
        ));
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
        assert!(html.contains("Automatic downloads are off."));
        assert!(html.contains("Nothing right now."));
        let downloaded = html.find("downloaded 12.3 MB in 1:15").unwrap();
        let failed = html.find("failed: bad thing").unwrap();
        assert!(downloaded < failed);
    }

    #[test]
    fn every_linked_asset_is_served() {
        let page = layout(
            &Frame {
                title: "t",
                current: Section::Downloading,
                status: &SyncStatus::default(),
                counts: &[],
                board: &Board::default(),
                footer: String::new(),
                after: 0,
                live: false,
            },
            "",
        );
        let css = std::str::from_utf8(ASSETS[0].2).unwrap();
        let mut linked = 0;
        for text in [page.as_str(), css] {
            for (at, _) in text.match_indices("/assets/") {
                let rest = &text[at + "/assets/".len()..];
                let name = &rest[..rest.find(['"', ')']).unwrap()];
                assert!(
                    ASSETS.iter().any(|(file, ..)| *file == name),
                    "{name} is linked but not served"
                );
                linked += 1;
            }
        }
        assert!(linked >= ASSETS.len());
    }

    #[test]
    fn progress_shows_as_a_bar() {
        let video = video("youtube", None);
        let mut active = Active {
            job: job(&video),
            requested: true,
            stage: crate::pipeline::Stage::Download,
            started_at: 0,
            progress: None,
        };
        assert!(activity_html(&active).contains("progress-busy"));
        active.progress = Some(crate::download::Progress {
            downloaded: 2_500_000,
            total: Some(10_000_000.0),
            speed: None,
            eta: None,
            format_id: "18".into(),
        });
        assert!(activity_html(&active).contains("style=\"width: 25.0%\""));
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
