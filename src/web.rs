//! The localhost web UI: plain server-rendered HTML. The list page pages
//! through the liked videos, newest first, on all platforms or one, sorted by
//! any column or narrowed by a search, all kept in the URL; each video
//! has its own page, with a button that has the download pipeline download it
//! right away, `/downloading` shows what the pipeline is doing, `/stats`
//! how close each platform's downloads are to caught up, and `/tv`
//! plays what's downloaded as television (`web/tv.rs`). Every open page
//! keeps a Server-Sent Events connection, over which newly stored videos, the
//! per-platform counts, what each sync is doing and the pipeline's progress
//! arrive as soon as they change. Once a video is downloaded, its page plays
//! it (see `player.rs`), from files served under the video's own path. The
//! pages are dressed as a Windows XP desktop with a Nickelodeon clock radio
//! in the side panel; the stylesheet, scripts, fonts and pictures in
//! `assets/` are built into the executable.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{Stream, StreamExt, stream};
use rusqlite::Connection;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::services::ServeFile;

use crate::db;
use crate::pipeline::{Active, Board, Converter, Converting, Job, Lane, Outcome, Pipeline, Wait};
use crate::player;
use crate::tv::Tv;

mod tv;

/// Where the UI listens. Localhost only: the archive is the user's own.
pub const ADDR: &str = "127.0.0.1:4774";

const PAGE_SIZE: usize = 50;

/// The running version, in every page's status bar, linking to its release
/// notes so a user can tell whether they already have a fix.
const VERSION: &str = concat!(
    "<a href=\"",
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/tag/v",
    env!("CARGO_PKG_VERSION"),
    "\">v",
    env!("CARGO_PKG_VERSION"),
    "</a>"
);

/// The sidebar's platforms, as (stored name, display name).
const PLATFORMS: [(&str, &str); 3] = [
    ("youtube", "YouTube"),
    ("tiktok", "TikTok"),
    ("instagram", "Instagram"),
];

/// The kinds of post the side panel lists under each platform, as
/// (platform, post type as `db::Filter` names it, label). YouTube's are all
/// videos, so it has none.
const POST_TYPES: [(&str, &str, &str); 5] = [
    ("tiktok", "video", "Videos"),
    ("tiktok", "slideshow", "Slideshows"),
    ("instagram", "video", "Videos"),
    ("instagram", "photo", "Photos"),
    ("instagram", "carousel", "Carousels"),
];

/// The label of `platform`'s `post_type`, e.g. "Slideshows".
fn post_type_label(platform: &str, post_type: &str) -> &'static str {
    POST_TYPES
        .iter()
        .find(|(p, t, _)| *p == platform && *t == post_type)
        .map_or("Posts", |(_, _, label)| label)
}

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
    Done {
        at: i64,
        added: usize,
        paused: Vec<Paused>,
    },
    Failed {
        at: i64,
        /// What happened and what to do, for the user.
        advice: String,
        /// The error itself, shown on hover.
        error: String,
        paused: Vec<Paused>,
    },
}

/// One of a platform's lists sitting out a while, after the platform told
/// us to slow down.
#[derive(Clone, Debug)]
pub struct Paused {
    /// Which list: "liked", "saved".
    pub list: &'static str,
    /// When it's read again (unix seconds).
    pub until: i64,
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
    /// Where downloads are saved; each one's folder is relative to it.
    media_root: PathBuf,
    /// Set to true by the Turn off button; the app loop and the server stop.
    quit: Arc<watch::Sender<bool>>,
    /// The TV channels' schedules.
    tv: Arc<Tv>,
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
    media_root: PathBuf,
    quit: watch::Sender<bool>,
) -> Result<()> {
    let mut quitting = quit.subscribe();
    let app = App {
        db: Arc::new(Mutex::new(conn)),
        status,
        pipeline,
        media_root,
        quit: Arc::new(quit),
        tv: Arc::new(Tv::new(crate::tv::seed(), tv::bumpers())),
    };
    let router = Router::new()
        .route("/", get(list_page))
        .route("/videos/{platform}/{video_id}", get(video_page))
        .route(
            "/videos/{platform}/{video_id}/download",
            post(request_download),
        )
        .route(
            "/videos/{platform}/{video_id}/playback",
            post(request_playback),
        )
        .route(
            "/videos/{platform}/{video_id}/media/{file}",
            get(media_file),
        )
        .route("/downloading", get(downloading_page))
        .route("/stats", get(stats_page))
        .route("/tv", get(tv::all_page))
        .route("/tv/{channel}", get(tv::channel_page))
        .route("/tv/{channel}/lineup", get(tv::lineup))
        .route("/events", get(events))
        .route("/assets/{name}", get(asset))
        .route("/quit", post(turn_off))
        .with_state(app);
    // Finish the "turned off" page before the listener closes.
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            // A closed channel means the app is exiting anyway.
            let _ = quitting.wait_for(|quit| *quit).await;
        })
        .await?;
    Ok(())
}

/// The taskbar's Turn off button: stops the app, including any download or
/// sync in progress, and leaves the browser a page saying so (the stylesheet
/// may already be gone, so it's styled inline).
async fn turn_off(State(app): State<App>, headers: HeaderMap) -> Result<Response, Failure> {
    if !from_this_ui(&headers) {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "refusing to turn off for another site".into(),
        ));
    }
    app.quit.send_replace(true);
    Ok(Html(TURNED_OFF_PAGE).into_response())
}

const TURNED_OFF_PAGE: &str = "<!doctype html>\n\
<html lang=\"en\">\n\
<head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Turned off – joyscroller</title></head>\n\
<body style=\"margin:0;min-height:100vh;display:grid;place-items:center;\
background:#004e98;color:#fff;font:18px Tahoma,Verdana,sans-serif;text-align:center\">\n\
<p>Joyscroller is turned off.<br>Open it again to keep archiving your likes.<br>\
You can close this tab.</p>\n\
</body>\n</html>\n";

/// The stylesheets, scripts, fonts and pictures, as (name, type, contents).
/// They're built into the executable, so the app stays a single file.
/// `videojs.js` is Video.js 10, bundled by `just videojs`.
const ASSETS: [(&str, &str, &[u8]); 46] = [
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/style.css"),
    ),
    (
        "tv.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/tv.css"),
    ),
    (
        "tv.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/tv.js"),
    ),
    (
        "crt.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/crt.js"),
    ),
    (
        "tvsound.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/tvsound.js"),
    ),
    ("tv.webp", "image/webp", include_bytes!("../assets/tv.webp")),
    (
        "tv-power.webp",
        "image/webp",
        include_bytes!("../assets/tv-power.webp"),
    ),
    (
        "tv-up.webp",
        "image/webp",
        include_bytes!("../assets/tv-up.webp"),
    ),
    (
        "tv-down.webp",
        "image/webp",
        include_bytes!("../assets/tv-down.webp"),
    ),
    (
        "tv-knob.webp",
        "image/webp",
        include_bytes!("../assets/tv-knob.webp"),
    ),
    (
        "tv-led.webp",
        "image/webp",
        include_bytes!("../assets/tv-led.webp"),
    ),
    // The station's bumpers (assets/blender/bumpers.py, bumpers.json).
    (
        "bumper-splat.mp4",
        "video/mp4",
        include_bytes!("../assets/bumper-splat.mp4"),
    ),
    (
        "bumper-up-next.mp4",
        "video/mp4",
        include_bytes!("../assets/bumper-up-next.mp4"),
    ),
    (
        "bumper-bounce.mp4",
        "video/mp4",
        include_bytes!("../assets/bumper-bounce.mp4"),
    ),
    (
        "player.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/player.css"),
    ),
    (
        "player.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/player.js"),
    ),
    (
        "videojs.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/videojs.js"),
    ),
    (
        "radio.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/radio.js"),
    ),
    (
        "lab.webp",
        "image/webp",
        include_bytes!("../assets/lab.webp"),
    ),
    (
        "radio.webp",
        "image/webp",
        include_bytes!("../assets/radio.webp"),
    ),
    (
        "radio-lit.webp",
        "image/webp",
        include_bytes!("../assets/radio-lit.webp"),
    ),
    (
        "radio-dome.webp",
        "image/webp",
        include_bytes!("../assets/radio-dome.webp"),
    ),
    (
        "radio-dome-lit.webp",
        "image/webp",
        include_bytes!("../assets/radio-dome-lit.webp"),
    ),
    (
        "radio-knob.webp",
        "image/webp",
        include_bytes!("../assets/radio-knob.webp"),
    ),
    (
        "splat.webp",
        "image/webp",
        include_bytes!("../assets/splat.webp"),
    ),
    (
        "icon-likes.webp",
        "image/webp",
        include_bytes!("../assets/icon-likes.webp"),
    ),
    (
        "icon-youtube.webp",
        "image/webp",
        include_bytes!("../assets/icon-youtube.webp"),
    ),
    (
        "icon-tiktok.webp",
        "image/webp",
        include_bytes!("../assets/icon-tiktok.webp"),
    ),
    (
        "icon-instagram.webp",
        "image/webp",
        include_bytes!("../assets/icon-instagram.webp"),
    ),
    (
        "icon-downloading.webp",
        "image/webp",
        include_bytes!("../assets/icon-downloading.webp"),
    ),
    (
        "casing.webp",
        "image/webp",
        include_bytes!("../assets/casing.webp"),
    ),
    (
        "panel.webp",
        "image/webp",
        include_bytes!("../assets/panel.webp"),
    ),
    (
        "dome-download.webp",
        "image/webp",
        include_bytes!("../assets/dome-download.webp"),
    ),
    (
        "dome-play.webp",
        "image/webp",
        include_bytes!("../assets/dome-play.webp"),
    ),
    (
        "button-violet.webp",
        "image/webp",
        include_bytes!("../assets/button-violet.webp"),
    ),
    (
        "button-red.webp",
        "image/webp",
        include_bytes!("../assets/button-red.webp"),
    ),
    (
        "groove.webp",
        "image/webp",
        include_bytes!("../assets/groove.webp"),
    ),
    (
        "fader.webp",
        "image/webp",
        include_bytes!("../assets/fader.webp"),
    ),
    (
        "ball.webp",
        "image/webp",
        include_bytes!("../assets/ball.webp"),
    ),
    (
        "lcd.webp",
        "image/webp",
        include_bytes!("../assets/lcd.webp"),
    ),
    (
        "led.webp",
        "image/webp",
        include_bytes!("../assets/led.webp"),
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

async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Result<Response, Failure> {
    let &(_, content_type, body) = ASSETS
        .iter()
        .find(|(file, ..)| *file == name)
        .ok_or_else(not_found)?;
    // Fonts never change. The rest changes with the app, so the browser
    // checks for a new copy each time, and with the ETag it's told to keep
    // the one it has (the radio's frames alone are over a megabyte).
    let cache = if content_type.starts_with("font/") {
        "max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let etag = etag(body);
    let kept = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|tags| tags.as_bytes() == etag.as_bytes());
    // Several ranges at once, which browsers don't ask media for, get the
    // whole file, as RFC 9110 allows.
    let range = headers
        .get(header::RANGE)
        .filter(|r| !r.as_bytes().contains(&b','))
        .map(|r| byte_range(r, body.len()));
    let fixed = [
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, cache),
        (header::ETAG, etag.as_str()),
        (header::ACCEPT_RANGES, "bytes"),
    ];
    Ok(match range {
        _ if kept => (StatusCode::NOT_MODIFIED, fixed).into_response(),
        None => (fixed, body).into_response(),
        // A clip (the bumpers): Safari plays video only from a server that
        // answers range requests, and the TV seeks into one it joins late.
        Some(Some((start, end))) => (
            StatusCode::PARTIAL_CONTENT,
            fixed,
            [(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", body.len()),
            )],
            &body[start..=end],
        )
            .into_response(),
        Some(None) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{}", body.len()))],
        )
            .into_response(),
    })
}

/// The first and last byte a `Range` header asks for, of `len` bytes: a
/// single range, as browsers ask for media (`bytes=a-b`, `bytes=a-` or the
/// last n, `bytes=-n`). None if it can't be served.
fn byte_range(header: &HeaderValue, len: usize) -> Option<(usize, usize)> {
    let spec = header.to_str().ok()?.strip_prefix("bytes=")?;
    let (from, to) = spec.split_once('-')?;
    let last = len.checked_sub(1)?;
    let (start, end) = match (from.trim(), to.trim()) {
        ("", n) => (len.checked_sub(n.parse::<usize>().ok()?.min(len))?, last),
        (a, "") => (a.parse().ok()?, last),
        (a, b) => (a.parse().ok()?, b.parse::<usize>().ok()?.min(last)),
    };
    (start <= end).then_some((start, end))
}

/// An asset's entity tag: a hash of its contents. `DefaultHasher::new`
/// hashes with fixed keys, so a build gives the same tag every run.
fn etag(body: &[u8]) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(body, &mut hasher);
    format!("\"{:016x}\"", std::hash::Hasher::finish(&hasher))
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
    /// One of the platform's `POST_TYPES`.
    #[serde(rename = "type")]
    post_type: Option<String>,
    page: Option<usize>,
    q: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
}

/// The column sorts, as (sort, its `sort=` key in the URL, the direction a
/// first click picks): text A to Z, numbers and dates biggest and newest
/// first. Anything else in the URL is ignored, never passed on.
const SORTS: [(db::Sort, &str, db::Dir); 6] = [
    (db::Sort::Best, "best", db::Dir::Asc),
    (db::Sort::Title, "title", db::Dir::Asc),
    (db::Sort::Author, "author", db::Dir::Asc),
    (db::Sort::Length, "duration", db::Dir::Desc),
    (db::Sort::Posted, "posted", db::Dir::Desc),
    (db::Sort::Added, "added", db::Dir::Desc),
];

/// What the list page shows: one platform or all, the search, the order and
/// the page. All of it lives in the URL, so paging, the platform links and
/// the back button keep it, without JavaScript.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListView<'a> {
    platform: Option<&'a str>,
    /// One of the platform's `POST_TYPES`, or None for all its posts.
    post_type: Option<&'a str>,
    /// The search as typed, trimmed; empty for none.
    q: String,
    order: db::Order,
    page: usize,
}

impl<'a> ListView<'a> {
    /// The list as the sidebar links to it: everything, newest first.
    fn plain(platform: Option<&'a str>, page: usize) -> Self {
        ListView {
            platform,
            post_type: None,
            q: String::new(),
            order: db::Order::NEWEST,
            page,
        }
    }

    fn from_query(
        platform: Option<&'a str>,
        post_type: Option<&'a str>,
        query: &ListQuery,
    ) -> Self {
        let mut view = ListView::plain(platform, query.page.unwrap_or(1).max(1));
        view.post_type = post_type;
        view.q = query.q.as_deref().unwrap_or_default().trim().into();
        let sort = SORTS
            .iter()
            .find(|(_, key, _)| query.sort.as_deref() == Some(*key));
        view.order = match sort {
            // Best match without a search means nothing; the default stands.
            Some((db::Sort::Best, _, _)) | None => view.default_order(),
            Some(&(sort, _, first)) => db::Order {
                sort,
                dir: match query.dir.as_deref() {
                    Some("asc") => db::Dir::Asc,
                    Some("desc") => db::Dir::Desc,
                    _ => first,
                },
            },
        };
        view
    }

    fn searching(&self) -> bool {
        !self.q.is_empty()
    }

    /// Best match while searching, newest first otherwise.
    fn default_order(&self) -> db::Order {
        if self.searching() {
            db::Order {
                sort: db::Sort::Best,
                dir: db::Dir::Asc,
            }
        } else {
            db::Order::NEWEST
        }
    }

    /// Whether newly synced likes belong at the top of this page: only the
    /// first page of the list in like order, newest first, unsearched.
    fn live(&self) -> bool {
        self.page == 1 && !self.searching() && self.order == db::Order::NEWEST
    }

    /// The same list on another page.
    fn at(&self, page: usize) -> Self {
        ListView {
            page,
            ..self.clone()
        }
    }

    /// The same list in another order, from its first page.
    fn ordered(&self, order: db::Order) -> Self {
        ListView {
            order,
            page: 1,
            ..self.clone()
        }
    }

    /// The filter it shows.
    fn filter(&self) -> db::Filter<'a> {
        db::Filter {
            platform: self.platform,
            post_type: self.post_type,
        }
    }

    /// The URL that shows this list, escaped for an attribute.
    fn href(&self) -> String {
        let mut params = Vec::new();
        if let Some(name) = self.platform {
            params.push(format!("platform={}", url_encode(name)));
        }
        if let Some(post_type) = self.post_type {
            params.push(format!("type={}", url_encode(post_type)));
        }
        if self.searching() {
            params.push(format!("q={}", url_encode(&self.q)));
        }
        if self.order != self.default_order() {
            let key = SORTS
                .iter()
                .find(|(sort, _, _)| *sort == self.order.sort)
                .map_or("added", |(_, key, _)| key);
            params.push(format!("sort={key}"));
            if self.order.sort != db::Sort::Best {
                params.push(format!("dir={}", dir_key(self.order.dir)));
            }
        }
        if self.page > 1 {
            params.push(format!("page={}", self.page));
        }
        if params.is_empty() {
            "/".into()
        } else {
            escape(&format!("/?{}", params.join("&")))
        }
    }
}

fn dir_key(dir: db::Dir) -> &'static str {
    match dir {
        db::Dir::Asc => "asc",
        db::Dir::Desc => "desc",
    }
}

/// Percent-encodes everything but the characters a URL never needs encoded.
fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
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
    // A type only goes with the platform it's listed under.
    let post_type = match (platform, query.post_type.as_deref()) {
        (_, None | Some("")) => None,
        (Some(platform), Some(wanted)) => Some(
            POST_TYPES
                .iter()
                .find(|(p, t, _)| *p == platform && *t == wanted)
                .ok_or_else(not_found)?
                .1,
        ),
        (None, Some(_)) => return Err(not_found()),
    };
    let view = ListView::from_query(platform, post_type, &query);
    let search = crate::query::parse(&view.q);
    // A search with nothing searchable in it ("*", "-") finds nothing,
    // rather than everything.
    let nothing = view.searching() && search.is_empty();
    let offset = (view.page - 1).saturating_mul(PAGE_SIZE);
    let (videos, found, counts, after) = app.db(|conn| {
        let counts = db::video_counts(conn)?;
        let (videos, found) = if nothing {
            (Vec::new(), 0)
        } else {
            (
                db::videos_page(conn, view.filter(), &search, view.order, PAGE_SIZE, offset)?,
                if view.searching() {
                    db::videos_count(conn, view.filter(), &search)?
                } else {
                    count_of(&counts, platform, post_type)
                },
            )
        };
        Ok((videos, found, counts, db::last_rowid(conn)?))
    })?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let pages = found.div_ceil(PAGE_SIZE).max(1);

    let heading = match (platform, post_type) {
        (Some(name), Some(post_type)) => format!(
            "{} {}",
            display_name(name),
            post_type_label(name, post_type).to_lowercase()
        ),
        (Some(name), None) => format!("{} likes", display_name(name)),
        (None, _) => "All likes".into(),
    };
    let pager = pager(&view, pages);
    let main = format!(
        "<p id=\"new\" class=\"balloon\" hidden><a href=\"{}\"><span id=\"new-count\">0</span> new likes — show them</a></p>\n\
         {}{pager}{}{pager}",
        ListView {
            post_type,
            ..ListView::plain(platform, 1)
        }
        .href(),
        search_html(&view),
        table_html(&view, &videos),
    );

    // A search's count is of that search, which a new like may not be in, so
    // the live updates leave it alone.
    let footer = if view.searching() {
        format!(
            "{} {} for “{}”",
            thousands(found),
            if found == 1 { "match" } else { "matches" },
            escape(&view.q)
        )
    } else {
        format!(
            "<span data-count=\"{}\">{}</span> likes",
            count_name(platform, post_type),
            found
        )
    };
    let current = match (platform, post_type) {
        (Some(platform), Some(post_type)) => Section::PostType(platform, post_type),
        _ => Section::Likes(platform),
    };
    Ok(Html(layout(
        &Frame {
            title: &heading,
            current,
            list: Some(&view),
            status: &status,
            counts: &counts,
            board: &board,
            footer,
            after,
            live: view.live(),
            player: false,
        },
        &main,
    )))
}

/// The list's table, with a line saying why when it's empty.
fn table_html(view: &ListView, videos: &[db::StoredVideo]) -> String {
    let rows: String = videos.iter().map(|video| video_row(video) + "\n").collect();
    let empty = if !videos.is_empty() {
        String::new()
    } else if view.page > 1 {
        "<p id=\"empty\" class=\"empty\">Nothing on this page.</p>\n".into()
    } else if view.searching() {
        format!(
            "<p id=\"empty\" class=\"empty\">No likes match “{}”.</p>\n",
            escape(&view.q)
        )
    } else {
        "<p id=\"empty\" class=\"empty\">No liked videos yet. \
         They show up here as each sync finishes.</p>\n"
            .into()
    };
    format!(
        "<table class=\"list\">\n<thead><tr><th></th>{}{}{}{}{}\
         <th class=\"source\">Source</th></tr></thead>\n<tbody id=\"videos\">\n{rows}</tbody>\n</table>\n{empty}",
        sort_header(view, db::Sort::Title, "Title", ""),
        sort_header(view, db::Sort::Author, "Author", "author"),
        sort_header(view, db::Sort::Length, "Length", "num"),
        sort_header(view, db::Sort::Posted, "Posted", "date"),
        sort_header(view, db::Sort::Added, "Added", "date"),
    )
}

/// The search box: a plain GET form that keeps the platform. A new search
/// starts at its best matches. While searching, it offers to go back to best
/// match after a column sort, and to clear the search.
fn search_html(view: &ListView) -> String {
    let platform = [("platform", view.platform), ("type", view.post_type)]
        .into_iter()
        .filter_map(|(name, value)| {
            value.map(|value| {
                format!(
                    "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                    escape(value)
                )
            })
        })
        .collect::<String>();
    let mut extra = Vec::new();
    if view.searching() {
        if view.order.sort != db::Sort::Best {
            extra.push(format!(
                "<a class=\"button\" href=\"{}\">Best match</a>",
                view.ordered(view.default_order()).href()
            ));
        }
        let cleared = ListView {
            q: String::new(),
            ..view.at(1)
        };
        let cleared = if view.order.sort == db::Sort::Best {
            cleared.ordered(db::Order::NEWEST)
        } else {
            cleared
        };
        extra.push(format!(
            "<a class=\"button\" href=\"{}\">Clear</a>",
            cleared.href()
        ));
    }
    let extra = extra.concat();
    format!(
        "<form class=\"search\" method=\"get\" action=\"/\" role=\"search\">\
         <input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"Search titles, descriptions, authors\" \
         aria-label=\"Search likes\" autocomplete=\"off\">{platform}\
         <button class=\"button\" type=\"submit\">Search</button>{extra}</form>\n",
        escape(&view.q),
    )
}

/// A column header that sorts the list by `sort`: by it, or the other way
/// if it already does, with an arrow on the current one.
fn sort_header(view: &ListView, sort: db::Sort, label: &str, class: &str) -> String {
    let first = SORTS
        .iter()
        .find(|(s, _, _)| *s == sort)
        .map_or(db::Dir::Desc, |(_, _, first)| *first);
    let current = view.order.sort == sort;
    let dir = match (current, view.order.dir) {
        (true, db::Dir::Asc) => db::Dir::Desc,
        (true, db::Dir::Desc) => db::Dir::Asc,
        (false, _) => first,
    };
    let (aria, arrow) = match (current, view.order.dir) {
        (false, _) => (String::new(), ""),
        (true, db::Dir::Asc) => (" aria-sort=\"ascending\"".into(), " ▲"),
        (true, db::Dir::Desc) => (" aria-sort=\"descending\"".into(), " ▼"),
    };
    let class = if class.is_empty() {
        String::new()
    } else {
        format!(" class=\"{class}\"")
    };
    format!(
        "<th{class}{aria}><a href=\"{}\">{label}{arrow}</a></th>",
        view.ordered(db::Order { sort, dir }).href()
    )
}

async fn video_page(
    State(app): State<App>,
    Path((platform, video_id)): Path<(String, String)>,
) -> Result<Html<String>, Failure> {
    let (found, download, playback, counts, after) = app.db(|conn| {
        Ok((
            db::video(conn, &platform, &video_id)?,
            db::download_state(conn, &platform, &video_id)?,
            db::playback_state(conn, &platform, &video_id)?,
            db::video_counts(conn)?,
            db::last_rowid(conn)?,
        ))
    })?;
    let (video, raw_json) = found.ok_or_else(not_found)?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let source = display_name(&video.platform);
    let title = short_title(&video);

    let (screen, playing) = screen_html(&video, download.as_ref(), playback.as_ref());
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
    let fields = facts_html(&video, download.as_ref());
    // Fall back to the stored text if it somehow isn't valid JSON.
    let pretty = serde_json::from_str::<serde_json::Value>(&raw_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or(raw_json);
    let main = format!(
        "<div class=\"video{playing}\">\n\
         <div class=\"preview\">\n{screen}\n\
         <div class=\"links\">\n\
         <a class=\"button button-go\" href=\"{url}\">{icon}Open on {source}</a>\n\
         <a class=\"button\" href=\"{back}\">← {source} likes</a>\n\
         {pick}\
         </div>\n\
         </div>\n\
         <div>\n\
         <h2 class=\"video-title\">{heading}</h2>\n\
         {caption}\
         <fieldset class=\"group\"><legend>Media</legend>{media}</fieldset>\n\
         <fieldset class=\"group\"><legend>Details</legend><dl class=\"facts\">\n{fields}</dl></fieldset>\n\
         <details class=\"raw\"><summary>Raw JSON</summary><pre>{json}</pre></details>\n\
         </div>\n\
         </div>\n",
        playing = if playing { " has-player" } else { "" },
        icon = icon(Section::Likes(Some(&video.platform))),
        back = list_href(Some(&video.platform), 1),
        heading = escape(&title),
        url = escape(&video.url),
        // `player.js` shows it and names the file it would switch to.
        pick = if screen.contains("data-copy=") {
            "<button class=\"button\" id=\"player-pick\" type=\"button\" hidden></button>\n"
        } else {
            ""
        },
        media = media_html(&video, download.as_ref(), playback.as_ref(), &board),
        json = escape(&pretty),
    );

    Ok(Html(layout(
        &Frame {
            title: &title,
            current: Section::Likes(Some(&video.platform)),
            list: None,
            status: &status,
            counts: &counts,
            board: &board,
            footer: format!("{source} · {}", escape(&author_label(&video))),
            after,
            live: false,
            player: playing,
        },
        &main,
    )))
}

/// The video page's Details: author, length, dates, whether it's still up
/// (by the list, or by what the platform said when it was downloaded).
fn facts_html(video: &db::StoredVideo, download: Option<&db::DownloadState>) -> String {
    let on_source = format!("On {}", display_name(&video.platform));
    [
        ("Author", author_html(video)),
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
        ("Added", time(video.added_at, "datetime")),
        (
            &on_source,
            if !video.available {
                "private or deleted"
            } else if download.is_some_and(|d| d.unavailable_at.is_some()) {
                "unavailable"
            } else {
                "available"
            }
            .into(),
        ),
        ("ID", format!("<code>{}</code>", escape(&video.video_id))),
    ]
    .map(|(name, value)| format!("<dt>{name}</dt><dd>{value}</dd>\n"))
    .concat()
}

/// The top of a video's page: its player once the media is saved, and
/// whether that's what it is; until then, the platform's picture.
fn screen_html(
    video: &db::StoredVideo,
    download: Option<&db::DownloadState>,
    playback: Option<&db::PlaybackState>,
) -> (String, bool) {
    let Some(download) = download.filter(|d| d.downloaded_at.is_some()) else {
        return (thumbnail(video, "hqdefault", &escape(&video.url)), false);
    };
    let Some(mut playable) = player::playable(&download.files) else {
        return (thumbnail(video, "hqdefault", &escape(&video.url)), false);
    };
    if let player::Playable::Video { copy, .. } = &mut playable {
        *copy = playback_copy(download, playback);
    }
    (player::player_html(&video_href(video), &playable), true)
}

/// The video's playback copy, if one was made from the files it has now.
fn playback_copy<'a>(
    download: &db::DownloadState,
    playback: Option<&'a db::PlaybackState>,
) -> Option<player::PlaybackCopy<'a>> {
    let playback = playback?;
    if playback.finished_at.is_none()
        || download.folder.as_deref() != Some(playback.download_folder.as_str())
    {
        return None;
    }
    Some(player::PlaybackCopy {
        file: playback.file_name.as_deref()?,
        source: playback.original_config_json.as_deref(),
    })
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

/// Asked by the player when this browser can't play a video's original and
/// there's no playback copy yet: make it before the rest of the backlog.
async fn request_playback(
    State(app): State<App>,
    headers: HeaderMap,
    Path((platform, video_id)): Path<(String, String)>,
) -> Result<StatusCode, Failure> {
    if !from_this_ui(&headers) {
        return Err(Failure(
            StatusCode::FORBIDDEN,
            "refusing a playback request from another site".into(),
        ));
    }
    let (download, playback) = app.db(|conn| {
        Ok((
            db::download_state(conn, &platform, &video_id)?,
            db::playback_state(conn, &platform, &video_id)?,
        ))
    })?;
    // A copy on record whose file has gone (tidied away by hand, say) is
    // made again.
    if let Some(download) = &download
        && let Some(copy) = playback_copy(download, playback.as_ref())
        && let Some(dir) = &download.folder
        && !app.media_root.join(dir).join(copy.file).is_file()
    {
        app.db(|conn| db::forget_playback(conn, &platform, &video_id))?;
    }
    // Nothing to make now: already made, or waiting out a failed try.
    if app
        .db(|conn| db::playback_item(conn, &platform, &video_id))?
        .is_none()
    {
        return Ok(StatusCode::CONFLICT);
    }
    app.pipeline.convert_soon(&platform, &video_id);
    Ok(StatusCode::NO_CONTENT)
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

/// A downloaded video's file, for its player. Served with range requests,
/// which the browser needs to seek. Only the files the download recorded and
/// its playback copy are served, so no path can reach anything else on disk.
async fn media_file(
    State(app): State<App>,
    Path((platform, video_id, file)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response, Failure> {
    let (download, playback) = app.db(|conn| {
        Ok((
            db::download_state(conn, &platform, &video_id)?,
            db::playback_state(conn, &platform, &video_id)?,
        ))
    })?;
    let Some(download) = download.filter(|d| d.downloaded_at.is_some()) else {
        return Err(not_found());
    };
    let copy = playback_copy(&download, playback.as_ref()).map(|copy| copy.file);
    let Some(dir) = download
        .folder
        .filter(|_| download.files.contains(&file) || copy == Some(&file))
    else {
        return Err(not_found());
    };
    let path = app.media_root.join(dir).join(file);
    let response = ServeFile::new(&path)
        .try_call(request)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(response.map(Body::new))
}

async fn downloading_page(State(app): State<App>) -> Result<Html<String>, Failure> {
    let (counts, after) = app.db(|conn| Ok((db::video_counts(conn)?, db::last_rowid(conn)?)))?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let about = if board.lanes.is_empty() {
        "Videos download when you click Download now on their page, right away, \
         several at a time if you like."
    } else {
        "YouTube, TikTok and Instagram each have a lane that downloads every liked \
         video not downloaded yet, newest first, one at a time: YouTube waits 30 seconds \
         between videos, TikTok 15 to 30, Instagram 30 to 60. A video's Download now \
         button downloads it right away, alongside the lanes."
    };
    let about = format!(
        "{about} Videos that won't play in every browser (most of them: YouTube's AV1, \
         TikTok's HEVC, Instagram's VP9) then get a playback copy in H.264 next to the \
         original, made one at a time in the background."
    );
    let main = format!(
        "<p class=\"intro\">{about}</p>\n\
         <div id=\"downloads\">\n{}</div>\n",
        board_html(&board)
    );
    Ok(Html(layout(
        &Frame {
            title: "Downloading",
            current: Section::Downloading,
            list: None,
            status: &status,
            counts: &counts,
            board: &board,
            footer: format!(
                "<span data-count=\"downloading\">{}</span> downloading",
                busy_count(&board)
            ),
            after,
            live: false,
            player: false,
        },
        &main,
    )))
}

/// How close each platform is to caught up: likes found, downloaded, and
/// given a playback copy where needed. The page's script fetches it again
/// as downloads finish.
async fn stats_page(State(app): State<App>) -> Result<Html<String>, Failure> {
    let (counts, after, catch_up) = app.db(|conn| {
        Ok((
            db::video_counts(conn)?,
            db::last_rowid(conn)?,
            db::catch_up(conn)?,
        ))
    })?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let main = format!(
        "<p class=\"intro\">How close the archive is to caught up: of the likes found on \
         each platform, how many are downloaded, and of the downloaded videos, how many \
         have been checked for a playback copy and given one where needed.</p>\n\
         <div id=\"stats\" class=\"stats\">\n{}</div>\n",
        stats_html(&catch_up)
    );
    Ok(Html(layout(
        &Frame {
            title: "Stats",
            current: Section::Stats,
            list: None,
            status: &status,
            counts: &counts,
            board: &board,
            footer: "Updates as downloads finish".into(),
            after,
            live: false,
            player: false,
        },
        &main,
    )))
}

/// The stats page's cards: all platforms together, when there's more than
/// one, then each platform's.
fn stats_html(catch_up: &[db::CatchUp]) -> String {
    if catch_up.is_empty() {
        return "<p class=\"empty\">No likes found yet. They show up here after the first sync.</p>\n"
            .into();
    }
    let mut html = String::new();
    if catch_up.len() > 1 {
        let all = catch_up
            .iter()
            .fold(db::CatchUp::default(), |all, c| db::CatchUp {
                liked: all.liked + c.liked,
                gone: all.gone + c.gone,
                downloaded: all.downloaded + c.downloaded,
                failing: all.failing + c.failing,
                videos: all.videos + c.videos,
                checked: all.checked + c.checked,
                copies: all.copies + c.copies,
                copy_failing: all.copy_failing + c.copy_failing,
                ..all
            });
        html.push_str(&catch_up_html(
            "stat stat-all",
            icon(Section::Likes(None)),
            "All platforms",
            "their platform",
            &all,
        ));
    }
    for (name, label) in PLATFORMS {
        if let Some(c) = catch_up.iter().find(|c| c.platform == name) {
            html.push_str(&catch_up_html(
                "stat",
                icon(Section::Likes(Some(name))),
                label,
                label,
                c,
            ));
        }
    }
    html
}

/// One card of the stats page: how many likes were found, then a meter for
/// the downloads and one for the playback copies.
fn catch_up_html(class: &str, icon: &str, label: &str, gone_from: &str, c: &db::CatchUp) -> String {
    let gone = if c.gone > 0 {
        format!(
            " <span class=\"stat-gone\">· {} gone from {gone_from}, so they can't be downloaded</span>",
            thousands(c.gone)
        )
    } else {
        String::new()
    };
    let downloadable = c.downloadable();
    let mut downloads = vec![if c.downloaded >= downloadable {
        "All downloaded".to_string()
    } else {
        format!(
            "{} of {} · {} to go",
            thousands(c.downloaded),
            thousands(downloadable),
            thousands(downloadable - c.downloaded)
        )
    }];
    if c.failing > 0 {
        downloads.push(format!(
            "{} failed, trying again later",
            thousands(c.failing)
        ));
    }
    let mut copies = vec![if c.videos == 0 {
        "Nothing to check until videos are downloaded".to_string()
    } else {
        format!(
            "{} of {} downloaded videos checked · {} needed a copy",
            thousands(c.checked),
            thousands(c.videos),
            thousands(c.copies)
        )
    }];
    if c.checked < c.videos {
        copies.push(format!("{} waiting", thousands(c.videos - c.checked)));
    }
    if c.copy_failing > 0 {
        copies.push(format!(
            "{} failed, trying again later",
            thousands(c.copy_failing)
        ));
    }
    format!(
        "<section class=\"{class}\">\n\
         <h2>{icon}{label}</h2>\n\
         <p class=\"stat-found\"><strong>{liked}</strong> likes found{gone}</p>\n\
         {downloaded}\
         {checked}\
         </section>\n",
        label = escape(label),
        liked = thousands(c.liked),
        downloaded = meter_html(
            "Downloaded",
            c.downloaded,
            downloadable,
            &downloads.join(" · ")
        ),
        checked = meter_html("Playback copies", c.checked, c.videos, &copies.join(" · ")),
    )
}

/// A labeled progress bar of `done` out of `of`, with its percentage and a
/// line under it.
fn meter_html(label: &str, done: usize, of: usize, note: &str) -> String {
    let (percent, width) = percent(done, of);
    format!(
        "<div class=\"stat-meter{done_class}\">\
         <div class=\"stat-head\"><span>{label}</span><strong class=\"stat-pct\">{percent}</strong></div>\
         <span class=\"progress\" role=\"progressbar\" aria-label=\"{label}\" \
         aria-valuemin=\"0\" aria-valuemax=\"{of}\" aria-valuenow=\"{done}\">\
         <span style=\"width:{width}\"></span></span>\
         <p class=\"stat-note\">{note}</p></div>\n",
        done_class = if of > 0 && done >= of { " done" } else { "" },
    )
}

/// `done` out of `of` as a whole percentage, rounded down so that it only
/// reads 100% when nothing is left, and "<1%" rather than 0% once anything
/// is done; and a bar's width, to a tenth of a percent. With nothing to do
/// there's no percentage.
fn percent(done: usize, of: usize) -> (String, String) {
    if of == 0 {
        return ("–".into(), "0%".into());
    }
    let tenths = done.min(of) * 1000 / of;
    let percent = if done > 0 && tenths < 10 {
        "&lt;1%".to_string()
    } else {
        format!("{}%", tenths / 10)
    };
    (percent, format!("{}.{}%", tenths / 10, tenths % 10))
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
    /// One platform's posts of one type, e.g. TikTok's slideshows.
    PostType(&'a str, &'a str),
    Tv,
    Downloading,
    Stats,
}

/// What every page shares around its `main`.
struct Frame<'a> {
    title: &'a str,
    current: Section<'a>,
    /// On the list page, what it shows, which the platform links keep.
    list: Option<&'a ListView<'a>>,
    status: &'a SyncStatus,
    counts: &'a [db::VideoCount],
    board: &'a Board,
    /// The window's status bar, under its contents.
    footer: String,
    /// The newest video's rowid when the page was rendered.
    after: i64,
    /// Whether new videos go straight into the table (the list's first page)
    /// rather than into a "new" notice.
    live: bool,
    /// Whether the page has a player, which needs Video.js and its skin.
    player: bool,
}

/// What a page with a player loads besides the usual. Module scripts run
/// after the page is parsed, so the player's elements are all there.
const PLAYER_HEAD: &str = "<link rel=\"stylesheet\" href=\"/assets/player.css\">\n\
<script type=\"module\" src=\"/assets/videojs.js\"></script>\n\
<script type=\"module\" src=\"/assets/player.js\"></script>\n";

/// One of the places the top bar and side panel link to.
struct NavEntry {
    section: Section<'static>,
    href: String,
    label: &'static str,
    /// A post type under its platform: only in the side panel, indented.
    sub: bool,
    /// What its `data-count` element is called in the live updates, and
    /// the count, if it has one.
    count_name: String,
    count: Option<usize>,
}

/// The lists of likes, each platform followed by its post types, then TV
/// and the downloads. From a list, the links keep its search and order.
fn nav_entries(frame: &Frame) -> Vec<NavEntry> {
    let entry = |name: Option<&'static str>, post_type: Option<&'static str>, label| {
        let href = frame.list.map_or_else(
            || ListView::plain(name, 1),
            |view| ListView {
                platform: name,
                post_type,
                ..view.at(1)
            },
        );
        let href = ListView { post_type, ..href }.href();
        NavEntry {
            section: match (name, post_type) {
                (Some(name), Some(post_type)) => Section::PostType(name, post_type),
                _ => Section::Likes(name),
            },
            href,
            label,
            sub: post_type.is_some(),
            count_name: count_name(name, post_type),
            count: Some(count_of(frame.counts, name, post_type)),
        }
    };
    let mut entries = vec![entry(None, None, "All likes")];
    for (name, label) in PLATFORMS {
        entries.push(entry(Some(name), None, label));
        for (_, post_type, label) in POST_TYPES.iter().filter(|(p, _, _)| *p == name) {
            entries.push(entry(Some(name), Some(post_type), label));
        }
    }
    entries.push(NavEntry {
        section: Section::Tv,
        href: "/tv".into(),
        label: "TV",
        sub: false,
        count_name: String::new(),
        count: None,
    });
    entries.push(NavEntry {
        section: Section::Downloading,
        href: "/downloading".into(),
        label: "Downloading",
        sub: false,
        count_name: "downloading".into(),
        count: Some(busy_count(frame.board)),
    });
    entries.push(NavEntry {
        section: Section::Stats,
        href: "/stats".into(),
        label: "Stats",
        sub: true,
        count_name: String::new(),
        count: None,
    });
    entries
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
/// see `assets/blender/radio.py`) whose LCD shows the time and which comes
/// alive on hover, the menu, and each platform's sync.
fn panel_html(frame: &Frame, entries: &[NavEntry]) -> String {
    let menu = |entries: &[NavEntry]| {
        entries
            .iter()
            .map(|entry| {
                let count = entry
                    .count
                    .map(|count| {
                        format!(
                            "<span class=\"count\" data-count=\"{}\">{count}</span>",
                            entry.count_name
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "<li{}><a href=\"{}\"{}>{}{}{count}</a></li>\n",
                    if entry.sub { " class=\"sub\"" } else { "" },
                    entry.href,
                    current(frame, entry.section),
                    if entry.sub { "" } else { icon(entry.section) },
                    entry.label,
                )
            })
            .collect::<Vec<_>>()
            .concat()
    };
    let tv = entries
        .iter()
        .position(|entry| entry.section == Section::Tv)
        .unwrap_or(entries.len());
    let (likes, rest) = entries.split_at(tv);
    let (watch, downloads) = rest.split_at(rest.len().min(1));
    format!(
        "<aside class=\"panel\">\n\
         {radio}\
         <nav>\n\
         <h2>Likes</h2>\n<ul class=\"menu\">\n{likes}</ul>\n\
         <h2>Watch</h2>\n<ul class=\"menu\">\n{watch}</ul>\n\
         <h2>Downloads</h2>\n<ul class=\"menu\">\n{downloads}</ul>\n\
         </nav>\n\
         <h2>Sync</h2>\n<ul class=\"sync\" id=\"status\">\n{status}</ul>\n\
         </aside>\n",
        radio = radio_html(),
        likes = menu(likes),
        watch = menu(watch),
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
        .filter(|entry| !entry.sub)
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
         <script type=\"module\" src=\"/assets/radio.js\"></script>\n\
         {player}\
         </head>\n\
         <body data-after=\"{after}\" data-platform=\"{platform}\" data-type=\"{post_type}\" data-live=\"{live}\" data-size=\"{PAGE_SIZE}\">\n\
         <header class=\"topbar\">\n\
         <a class=\"logo\" href=\"/\"><span class=\"splat\">joyscroller</span>\
         <span class=\"edition\">Archive Edition</span></a>\n\
         <nav class=\"quick\">\n{quick}</nav>\n\
         </header>\n\
         <div class=\"shell{bare}\">\n\
         {panel}\
         <main class=\"window\">\n\
         <div class=\"titlebar\">{icon}<h1>{title}</h1>\
         <div class=\"controls\" aria-hidden=\"true\"><span>_</span><span>□</span>\
         <a class=\"close\" href=\"/\" tabindex=\"-1\">✕</a></div></div>\n\
         <div class=\"window-body\">\n{main}</div>\n\
         <div class=\"statusbar\"><span>{footer}</span><span>Local archive</span><span>{VERSION}</span></div>\n\
         </main>\n\
         </div>\n\
         <footer class=\"taskbar\">\n\
         <a class=\"start\" href=\"/\"><img src=\"/assets/icon-likes.webp\" alt=\"\">start</a>\n\
         <span class=\"task\" title=\"{title}\">{icon}<span>{title}</span></span>\n\
         <span class=\"tray\">\
         <span class=\"tray-icon\" id=\"tray-sync\" role=\"img\"><span class=\"led\"></span></span>\
         <span class=\"tray-icon{downloads_idle}\" id=\"tray-downloads\" role=\"img\" \
         data-tip=\"{downloads_tip}\" aria-label=\"{downloads_tip}\">\
         <img src=\"/assets/icon-downloading.webp\" alt=\"\"></span>\
         <span id=\"tray-clock\"></span></span>\n\
         <form class=\"turn-off\" method=\"post\" action=\"/quit\">\
         <button type=\"submit\" title=\"Turn off Joyscroller\">{POWER_ICON}Turn off</button></form>\n\
         </footer>\n\
         {turn_off_dialog}\
         <script>{SCRIPT}</script>\n\
         </body>\n\
         </html>\n",
        title = escape(frame.title),
        after = frame.after,
        platform = match frame.current {
            Section::Likes(platform) => platform.unwrap_or(""),
            Section::PostType(platform, _) => platform,
            Section::Tv | Section::Downloading | Section::Stats => "",
        },
        post_type = match frame.current {
            Section::PostType(_, post_type) => post_type,
            _ => "",
        },
        live = u8::from(frame.live),
        player = if frame.player { PLAYER_HEAD } else { "" },
        panel = panel_html(frame, &entries),
        // TV is just the set on the desktop: no side panel, no window
        // around it. They stay in the page, hidden: the page script keeps
        // the panel's clock and sync list up to date.
        bare = if frame.current == Section::Tv {
            " bare"
        } else {
            ""
        },
        icon = icon(frame.current),
        footer = frame.footer,
        turn_off_dialog = turn_off_dialog(),
        downloads_idle = if busy_count(frame.board) == 0 {
            " idle"
        } else {
            ""
        },
        downloads_tip = downloads_tip(busy_count(frame.board)),
    )
}

/// Where the radio's moving layers go and how their sprite sheets are laid
/// out, as `assets/blender/radio_motion.py` rendered them (`just radio`).
/// `radio.js` reads the rest: the tube's path, the dome's poses.
const RADIO_JSON: &str = include_str!("../assets/radio.json");

#[derive(Deserialize)]
struct RadioMotion {
    width: u32,
    height: u32,
    dome: RadioSheet,
    knob: RadioSheet,
}

/// A sprite sheet of one part's frames: `cols` x `rows` tiles of the size
/// of `box` (x, y, width, height in the still), each `gutter` pixels apart
/// from the next.
#[derive(Deserialize)]
struct RadioSheet {
    r#box: [u32; 4],
    cols: u32,
    rows: u32,
    gutter: u32,
}

/// `RADIO_JSON` read, or `None` (said once) if it couldn't be, which leaves
/// the radio still. A test checks it reads.
static RADIO_MOTION: std::sync::LazyLock<Option<RadioMotion>> = std::sync::LazyLock::new(|| {
    serde_json::from_str(RADIO_JSON)
        .inspect_err(|err| eprintln!("warning: assets/radio.json: {err}"))
        .ok()
});

/// The clock radio: the still with its moving parts on top, at their first
/// frame, then the same lit up, faded in by `radio.js` on hover; the arc it
/// draws in the tube; and the LCD's clock. It links to all likes.
///
/// The lit radio is added to the unlit one (plus-lighter) inside an
/// isolated group, with opacities summing to one: an exact cross-fade, even
/// where they're see-through. Laid over it the usual way, the shadow and
/// edges would count twice.
fn radio_html() -> String {
    let Some(motion) = &*RADIO_MOTION else {
        return format!(
            "<a class=\"radio\" href=\"/\" aria-label=\"All likes\">\
             <img class=\"radio-still\" src=\"/assets/radio.webp\" alt=\"\">{RADIO_LCD}</a>\n"
        );
    };
    let (width, height) = (f64::from(motion.width), f64::from(motion.height));
    let part = |class: &str, file: &str, sheet: &RadioSheet| {
        let [x, y, w, h] = sheet.r#box.map(f64::from);
        let gutter = f64::from(sheet.gutter);
        let (cols, rows) = (f64::from(sheet.cols), f64::from(sheet.rows));
        format!(
            "<span class=\"radio-part {class}\" style=\"left:{:.4}%;top:{:.4}%;width:{:.4}%;height:{:.4}%\">\
             <img src=\"/assets/{file}\" alt=\"\" style=\"width:{:.4}%;transform:translate({:.4}%, {:.4}%)\"></span>",
            x / width * 100.0,
            y / height * 100.0,
            w / width * 100.0,
            h / height * 100.0,
            cols * (w + 2.0 * gutter) / w * 100.0,
            -gutter / (cols * (w + 2.0 * gutter)) * 100.0,
            -gutter / (rows * (h + 2.0 * gutter)) * 100.0,
        )
    };
    format!(
        "<a class=\"radio\" href=\"/\" aria-label=\"All likes\" data-motion=\"{json}\">\
         <span class=\"radio-stack\">\
         <span class=\"radio-off\"><img class=\"radio-still\" src=\"/assets/radio.webp\" \
         width=\"{w}\" height=\"{h}\" alt=\"\">{knob}{dome}</span>\
         <span class=\"radio-on\"><img class=\"radio-still\" src=\"/assets/radio-lit.webp\" \
         width=\"{w}\" height=\"{h}\" alt=\"\">{knob}{dome_lit}</span></span>\
         <svg class=\"radio-arc\" viewBox=\"0 0 {w} {h}\" aria-hidden=\"true\">\
         <filter id=\"arc-blur\" x=\"-10%\" y=\"-10%\" width=\"120%\" height=\"120%\">\
         <feGaussianBlur stdDeviation=\"5\"/></filter>\
         <path class=\"arc-glow\"/><path class=\"arc-core\"/><path class=\"arc-hot\"/>\
         <path class=\"arc-surge\" pathLength=\"1\"/></svg>\
         {RADIO_LCD}</a>\n",
        json = escape(RADIO_JSON.trim()),
        w = motion.width,
        h = motion.height,
        knob = part("radio-knob", "radio-knob.webp", &motion.knob),
        dome = part("radio-dome", "radio-dome.webp", &motion.dome),
        dome_lit = part("radio-dome", "radio-dome-lit.webp", &motion.dome),
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

/// The tray's download arrow's tooltip. The page's script words it the same.
fn downloads_tip(count: usize) -> String {
    if count == 0 {
        "Nothing downloading".into()
    } else {
        format!("{count} downloading")
    }
}

/// The Turn off button's power symbol. Drawn rather than typed: fonts that
/// have ⏻ are few, and the ones that do don't agree on its width.
const POWER_ICON: &str = "<svg viewBox=\"0 0 16 16\" aria-hidden=\"true\">\
<path d=\"M8 1.5v6M4.8 3.6a5.5 5.5 0 1 0 6.4 0\"/></svg>";

/// What the taskbar's Turn off button asks first, in the manner of XP's Turn
/// off computer box: its big button, the screen behind gone grey, and Cancel
/// (or Esc, or a click outside). Without the script, the taskbar's button
/// turns off straight away.
fn turn_off_dialog() -> String {
    format!(
        "<dialog class=\"shutdown\" id=\"turn-off\" aria-labelledby=\"turn-off-title\">\n\
         <div class=\"shutdown-head\"><h2 id=\"turn-off-title\">Turn off Joyscroller</h2>\
         <img src=\"/assets/icon-likes.webp\" alt=\"\"></div>\n\
         <form class=\"shutdown-body\" method=\"post\" action=\"/quit\">\
         <button type=\"submit\" autofocus><span class=\"knob\">{POWER_ICON}</span>Turn off</button>\
         <p>Syncing and downloads stop until you open Joyscroller again.</p></form>\n\
         <form class=\"shutdown-foot\" method=\"dialog\"><button class=\"button\">Cancel</button></form>\n\
         </dialog>\n"
    )
}

/// A section's icon: a molded plastic piece rendered in Blender
/// (`assets/blender/badges.py`), like the radio.
fn icon(section: Section) -> &'static str {
    match section {
        Section::PostType(platform, _) => icon(Section::Likes(Some(platform))),
        Section::Likes(Some("youtube")) => {
            "<img class=\"icon\" src=\"/assets/icon-youtube.webp\" alt=\"\">"
        }
        Section::Likes(Some("tiktok")) => {
            "<img class=\"icon\" src=\"/assets/icon-tiktok.webp\" alt=\"\">"
        }
        Section::Likes(Some("instagram")) => {
            "<img class=\"icon\" src=\"/assets/icon-instagram.webp\" alt=\"\">"
        }
        Section::Likes(_) => "<img class=\"icon\" src=\"/assets/icon-likes.webp\" alt=\"\">",
        Section::Downloading | Section::Stats => {
            "<img class=\"icon\" src=\"/assets/icon-downloading.webp\" alt=\"\">"
        }
        // The player's red play dome, the top frame of its sprite.
        Section::Tv => "<span class=\"icon icon-tv\" aria-hidden=\"true\"></span>",
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
  const tray = document.getElementById("tray-clock");
  tray.textContent = now.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  // XP's tray clock shows the date on hover. Set only when it changes, so
  // an open tooltip doesn't flicker.
  const date = now.toLocaleDateString([], { weekday: "long", year: "numeric", month: "long", day: "numeric" });
  if (tray.dataset.tip !== date) tip(tray, date);
}
tick();
setInterval(tick, 1000);

// The tray's tooltips are drawn by the stylesheet from data-tip, in XP's
// yellow, rather than the browser's own from title. An icon's also names it.
function tip(el, text) {
  el.dataset.tip = text;
  if (el.getAttribute("role") === "img") el.setAttribute("aria-label", text);
}

// The tray's light sums up the panel's: red if a platform's sync failed,
// blinking while one runs, grey until each has synced, green once all
// have. Hovering shows each platform's line.
function traySync() {
  const lines = [...document.getElementById("status").children];
  const light = ["failed", "running", "waiting"].find((state) =>
    lines.some((li) => li.classList.contains("sync-" + state))) || "done";
  const tray = document.getElementById("tray-sync");
  tray.className = "tray-icon sync-" + light;
  tip(tray, lines.map((li) =>
    li.querySelector("b").textContent + ": " + li.querySelector(".what").textContent).join("\n"));
}
traySync();

const turnOff = document.getElementById("turn-off");
document.querySelector("form.turn-off").addEventListener("submit", (e) => {
  e.preventDefault();
  turnOff.showModal();
});
// A click outside the box lands on the dialog itself, which the box fills.
turnOff.addEventListener("click", (e) => {
  if (e.target === turnOff) turnOff.close();
});

// Whether the video on the page has been started and plays: reloading
// would lose the viewer's place.
function watching() {
  const video = document.querySelector(".player video");
  const failed = document.querySelector(".player-error:not([hidden])");
  return Boolean(video && video.currentTime > 0 && !video.ended && !failed);
}

function setCount(name, count) {
  for (const el of document.querySelectorAll('[data-count="' + name + '"]')) el.textContent = count;
}

const page = document.body.dataset;
// Over plain HTTP, browsers allow six connections per site, and each page's
// stream holds one for as long as it's open. Seven tabs, or a few pages
// kept in the back/forward cache, would hold them all and hang the next
// page. So only a page on screen has a stream: it closes when the tab goes
// to the background or the page is left, and reopens when it's shown again,
// from the last video it got. The server sends the current state on
// connecting, so nothing is missed meanwhile.
let after = page.after;
let events = null;
let unseen = 0;
let statsDue = false;
addEventListener("pagehide", disconnect);
addEventListener("pageshow", (e) => { if (e.persisted) connect(); });
document.addEventListener("visibilitychange", () => {
  if (document.hidden) disconnect();
  else connect();
});
// A tab opened in the background waits until it's looked at.
connect();

function disconnect() {
  if (events) events.close();
  events = null;
}

function connect() {
  if (events || document.hidden) return;
  events = new EventSource("/events?after=" + after);
  events.addEventListener("video", (e) => {
    after = e.lastEventId;
    const rows = document.getElementById("videos");
    const template = document.createElement("template");
    template.innerHTML = e.data;
    const row = template.content.firstElementChild;
    if (!rows || (page.platform && row.dataset.platform !== page.platform)) return;
    if (page.type && row.dataset.type !== page.type) return;
    if (page.live === "1") {
      // In like order, newest first. A sync stores a listing page by page,
      // newest first, so a row can belong below others that came before it.
      const order = Number(row.dataset.order);
      const below = [...rows.rows].find((r) => Number(r.dataset.order) < order);
      if (below) rows.insertBefore(row, below);
      else if (rows.rows.length < Number(page.size)) rows.append(row);
      else return;
      showTimes(row);
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
    traySync();
  });
  events.addEventListener("downloads", (e) => {
    const update = JSON.parse(e.data);
    setCount("downloading", update.count);
    const tray = document.getElementById("tray-downloads");
    tray.classList.toggle("idle", update.count === 0);
    tip(tray, update.count === 0 ? "Nothing downloading" : update.count + " downloading");
    const board = document.getElementById("downloads");
    if (board) {
      board.innerHTML = update.board;
      showTimes(board);
    }
    // The stats page: fetch its numbers again, a few seconds after the
    // first change since the last fetch, as changes come in bursts.
    const stats = document.getElementById("stats");
    if (stats && !statsDue) {
      statsDue = true;
      setTimeout(async () => {
        try {
          const page = await (await fetch("/stats")).text();
          const fresh = new DOMParser().parseFromString(page, "text/html").getElementById("stats");
          if (fresh) stats.innerHTML = fresh.innerHTML;
        } finally {
          statsDue = false;
        }
      }, 5000);
    }
    // The list: a row whose video has just been downloaded swaps its
    // placeholder for the saved picture, built as `picture_html` builds it.
    for (const [key, src] of Object.entries(update.pictures)) {
      const cell = document.querySelector('#videos tr[data-key="' + CSS.escape(key) + '"] td.thumb');
      if (!cell || cell.querySelector("img")?.getAttribute("src") === src) continue;
      const link = document.createElement("a");
      link.href = cell.parentElement.querySelector("td.title a").getAttribute("href");
      const img = document.createElement("img");
      img.setAttribute("src", src);
      img.loading = "lazy";
      img.alt = "";
      link.append(img);
      cell.replaceChildren(link);
    }
    // A video page: show its progress, and once it's out of the pipeline,
    // reload for the result.
    const media = document.getElementById("media");
    if (media) {
      const progress = update.videos[media.dataset.key];
      if (progress) {
        media.innerHTML = progress;
        media.dataset.busy = update.converting === media.dataset.key ? "convert" : "1";
        showTimes(media);
      } else if (media.dataset.busy === "convert" && watching()) {
        // A playback copy finished while the original plays: the next
        // visit uses it, and this one carries on.
        delete media.dataset.busy;
        media.textContent = "Playback copy made. It's used the next time this page loads.";
      } else if (media.dataset.busy) {
        location.reload();
      }
    }
  });
}
"#;

/// A video's picture, linked to `href`: once its media is downloaded, the
/// saved thumbnail; until then YouTube's at `size` (`mqdefault`,
/// `hqdefault`), or for TikTok and Instagram, whose picture links expire, a
/// colored tile.
fn thumbnail(video: &db::StoredVideo, size: &str, href: &str) -> String {
    if let Some(file) = &video.thumbnail {
        picture_html(
            href,
            &escape(&media_src(&video.platform, &video.video_id, file)),
        )
    } else if video.platform == "youtube" {
        picture_html(
            href,
            &format!(
                "https://i.ytimg.com/vi/{}/{size}.jpg",
                escape(&video.video_id)
            ),
        )
    } else {
        format!(
            "<a class=\"nothumb\" data-kind=\"{}\" href=\"{href}\" tabindex=\"-1\" aria-hidden=\"true\"></a>",
            escape(&video.kind)
        )
    }
}

/// The picture at `src` (already escaped), linked to `href`. The page's
/// script builds the same markup when a row's video finishes downloading.
fn picture_html(href: &str, src: &str) -> String {
    format!("<a href=\"{href}\"><img src=\"{src}\" loading=\"lazy\" alt=\"\"></a>")
}

/// Where a downloaded video's `file` is served.
fn media_src(platform: &str, video_id: &str, file: &str) -> String {
    format!("/videos/{platform}/{video_id}/media/{file}")
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
        "<tr data-platform=\"{platform}\" data-type=\"{post_type}\" data-key=\"{platform}/{id}\" data-order=\"{order}\"><td class=\"thumb\">{thumbnail}</td>\
         <td class=\"title\"><a href=\"{href}\">{title}</a></td><td class=\"author\">{author}</td>\
         <td class=\"num\">{length}</td><td class=\"date\">{posted}</td><td class=\"date\">{added}</td>\
         <td class=\"source\"><span class=\"badge badge-{platform}\">{source}</span></td></tr>",
        platform = escape(&video.platform),
        post_type = escape(&video.post_type),
        id = escape(&video.video_id),
        order = video.like_order,
        title = escape(&truncate(&short_title(video), 120)),
        author = escape(&author_label(video)),
        posted = video
            .posted_at
            .map(|at| time(at, "date"))
            .unwrap_or_default(),
        added = time(video.added_at, "date"),
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
            let (light, state, details) = match status.get(name) {
                None => ("waiting", "waiting to sync".into(), String::new()),
                Some(SyncState::Running) => ("running", "syncing…".into(), String::new()),
                Some(SyncState::Done { at, added, paused }) => (
                    "done",
                    format!(
                        "synced at {}, {added} new{}",
                        time(*at, "time"),
                        paused_html(label, paused)
                    ),
                    String::new(),
                ),
                Some(SyncState::Failed {
                    at,
                    advice,
                    error,
                    paused,
                }) => (
                    "failed",
                    format!(
                        "didn't sync at {}: {}{}",
                        time(*at, "time"),
                        escape(advice),
                        paused_html(label, paused)
                    ),
                    format!(" title=\"{}\"", escape(error)),
                ),
            };
            format!(
                "<li class=\"sync-{light}\"><span class=\"led\"></span><b>{label}</b>\
                 <span class=\"what\"{details}>{state}</span></li>\n"
            )
        })
        .collect::<Vec<_>>()
        .concat()
}

/// ". Liked posts wait until 7:30 PM, as Instagram asked us to slow down",
/// for each of `site`'s lists sitting out.
fn paused_html(site: &str, paused: &[Paused]) -> String {
    let mut html = String::new();
    for p in paused {
        let _ = write!(
            html,
            ". {}{} posts wait until {}, as {site} asked us to slow down",
            p.list[..1].to_uppercase(),
            escape(&p.list[1..]),
            time(p.until, "time")
        );
    }
    html
}

/// `{"all": n, "youtube": n, ...}`, for the sidebar's counts.
fn counts_json(counts: &[db::VideoCount]) -> String {
    let mut object = serde_json::Map::new();
    object.insert("all".into(), count_of(counts, None, None).into());
    for (name, _) in PLATFORMS {
        object.insert(name.into(), count_of(counts, Some(name), None).into());
    }
    for (name, post_type, _) in POST_TYPES {
        object.insert(
            count_name(Some(name), Some(post_type)),
            count_of(counts, Some(name), Some(post_type)).into(),
        );
    }
    serde_json::Value::Object(object).to_string()
}

/// How many videos `platform` has (all platforms together for None), of
/// `post_type` or all of them.
fn count_of(counts: &[db::VideoCount], platform: Option<&str>, post_type: Option<&str>) -> usize {
    counts
        .iter()
        .filter(|c| platform.is_none_or(|wanted| c.platform == wanted))
        .filter(|c| post_type.is_none_or(|wanted| c.post_type == wanted))
        .map(|c| c.count)
        .sum()
}

/// What a count's `data-count` elements are called, for the live updates:
/// `all`, `tiktok`, `tiktok/slideshow`.
fn count_name(platform: Option<&str>, post_type: Option<&str>) -> String {
    match (platform, post_type) {
        (Some(platform), Some(post_type)) => format!("{platform}/{post_type}"),
        (Some(platform), None) => platform.into(),
        (None, _) => "all".into(),
    }
}

/// The pager line: where we are, and links to the neighboring pages.
fn pager(view: &ListView, pages: usize) -> String {
    // "Newer" and "Older" only say where a page goes in like order.
    let (back, forward) = if view.order == db::Order::NEWEST {
        ("← Newer", "Older →")
    } else {
        ("← Previous", "Next →")
    };
    let back = if view.page > 1 {
        format!(
            "<a class=\"button\" href=\"{}\">{back}</a>",
            view.at(view.page - 1).href()
        )
    } else {
        String::new()
    };
    let forward = if view.page < pages {
        format!(
            "<a class=\"button\" href=\"{}\">{forward}</a>",
            view.at(view.page + 1).href()
        )
    } else {
        String::new()
    };
    format!(
        "<nav class=\"pager\"><span class=\"where\">Page {} of {}</span>{back}{forward}</nav>\n",
        thousands(view.page),
        thousands(pages)
    )
}

/// The list as the sidebar links to it, on `page`.
fn list_href(platform: Option<&str>, page: usize) -> String {
    ListView::plain(platform, page).href()
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
        None if matches!(video.platform.as_str(), "tiktok" | "instagram") => "(no caption)".into(),
        None => "(no title)".into(),
    }
}

/// The author as the platform shows them: a channel name, or a TikTok or
/// Instagram @handle.
fn author_label(video: &db::StoredVideo) -> String {
    match (video.platform.as_str(), video.author_name.as_deref()) {
        (_, None) => String::new(),
        ("tiktok" | "instagram", Some(handle)) => format!("@{handle}"),
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
        "instagram" => video
            .author_name
            .as_deref()
            .map(|handle| format!("https://www.instagram.com/{handle}/")),
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
    video.available
        && download.is_none_or(|d| d.downloaded_at.is_none() && d.unavailable_at.is_none())
}

/// The video page's Media field: where its download stands in the pipeline,
/// or else in the database, with a button if it can be downloaded. `id` and
/// `data-key` are for the live updates.
fn media_html(
    video: &db::StoredVideo,
    download: Option<&db::DownloadState>,
    playback: Option<&db::PlaybackState>,
    board: &Board,
) -> String {
    let (inner, busy) = match job_html(board, &video.platform, &video.video_id) {
        // A finished copy reloads the page only if it isn't playing.
        Some(html)
            if board
                .find_active(&video.platform, &video.video_id)
                .is_none() =>
        {
            (html, " data-busy=\"convert\"")
        }
        Some(html) => (html, " data-busy=\"1\""),
        None if can_download(video, download) => (
            format!(
                "<form method=\"post\" action=\"{}/download\"><button class=\"dome\">Download now</button></form>\n\
                 <span>{}</span>",
                video_href(video),
                download_html(&video.platform, download)
            ),
            "",
        ),
        None => (
            format!(
                "{}{}",
                download_html(&video.platform, download),
                playback_html(video, download, playback)
            ),
            "",
        ),
    };
    format!(
        "<div class=\"media\" id=\"media\" data-key=\"{}/{}\"{busy}>{inner}</div>",
        escape(&video.platform),
        escape(&video.video_id)
    )
}

/// What the pipeline is doing with a video, if it's on it.
fn job_html(board: &Board, platform: &str, video_id: &str) -> Option<String> {
    board
        .find_active(platform, video_id)
        .map(activity_html)
        .or_else(|| {
            board
                .find_converting(platform, video_id)
                .map(converting_html)
        })
}

/// What the converter is doing with a video, e.g. "making a playback copy:
/// 45% of 3:21, 2.1× real time, 1:02 left, started 3:04:05 PM".
#[expect(
    clippy::cast_possible_truncation,
    reason = "whole seconds of a video's length, for display"
)]
fn converting_html(converting: &Converting) -> String {
    let started = time(converting.started_at, "time");
    let busy = "<span class=\"progress progress-busy\"><span></span></span>";
    let Some(progress) = &converting.progress else {
        return format!("making a playback copy, started {started}{busy}");
    };
    let speed = progress
        .speed
        .map(|speed| format!(", {speed:.1}× real time"))
        .unwrap_or_default();
    let Some(total) = progress.total.filter(|&t| t > 0.0) else {
        return format!(
            "making a playback copy: {} done{speed}, started {started}{busy}",
            duration(progress.done.round() as i64)
        );
    };
    let percent = (progress.done / total * 100.0).min(100.0);
    let eta = progress
        .speed
        .map(|speed| {
            format!(
                ", {} left",
                duration(((total - progress.done).max(0.0) / speed).round() as i64)
            )
        })
        .unwrap_or_default();
    format!(
        "making a playback copy: {percent:.0}% of {}{speed}{eta}, started {started}\
         <span class=\"progress\"><span style=\"width: {percent:.1}%\"></span></span>",
        duration(total.round() as i64)
    )
}

/// The Media field's line about a downloaded video's playback copy.
fn playback_html(
    video: &db::StoredVideo,
    download: Option<&db::DownloadState>,
    playback: Option<&db::PlaybackState>,
) -> String {
    let downloaded = download.is_some_and(|d| d.downloaded_at.is_some());
    if !downloaded || video.kind != "video" {
        return String::new();
    }
    let line = match playback {
        None => "playback copy: waiting its turn".into(),
        Some(db::PlaybackState {
            finished_at: Some(at),
            file_name: Some(_),
            size_bytes,
            method,
            ..
        }) => {
            #[expect(clippy::cast_precision_loss, reason = "a size for display")]
            let megabytes = size_bytes.unwrap_or(0) as f64 / 1e6;
            let how = if method.as_deref() == Some("remux") {
                "the picture as it is, the sound in AAC"
            } else {
                "re-encoded to H.264"
            };
            format!(
                "playback copy made {} ({how}), {megabytes:.1} MB",
                time(*at, "datetime")
            )
        }
        Some(db::PlaybackState {
            finished_at: Some(_),
            ..
        }) => "plays in every browser as downloaded".into(),
        Some(state) => {
            let tries = if state.failures == 1 { "try" } else { "tries" };
            let last_error = state
                .last_error
                .as_deref()
                .map(|error| format!(", last error: {}", escape(error)))
                .unwrap_or_default();
            format!(
                "playback copy not made; {} failed {tries}{last_error}",
                state.failures
            )
        }
    };
    format!("<br>{line}")
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
                let (class, outcome) = outcome_html(&finished.outcome);
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
         <h2>Playback copies</h2>\n{converter}\
         <h2>Finished</h2>\n{finished}",
        converter = converter_html(&board.converter, &job_link)
    )
}

/// How a finished video came out, as its CSS class and description.
#[expect(clippy::cast_precision_loss, reason = "sizes for display")]
fn outcome_html(outcome: &Outcome) -> (&'static str, String) {
    let one_line = |error: &str| escape(&error.replace(['\r', '\n'], " "));
    match outcome {
        Outcome::Downloaded { bytes, seconds, .. } => {
            let megabytes = *bytes as f64 / 1e6;
            let seconds = i64::try_from(*seconds).unwrap_or(i64::MAX);
            (
                "downloaded",
                format!("downloaded {megabytes:.1} MB in {}", duration(seconds)),
            )
        }
        Outcome::Failed(error) => ("failed", format!("failed: {}", one_line(error))),
        Outcome::Unavailable(said) => ("unavailable", one_line(said)),
        Outcome::Converted {
            bytes,
            seconds,
            method,
        } => {
            let megabytes = *bytes as f64 / 1e6;
            let seconds = i64::try_from(*seconds).unwrap_or(i64::MAX);
            let how = if *method == "remux" {
                "sound converted"
            } else {
                "re-encoded"
            };
            (
                "downloaded",
                format!(
                    "playback copy made ({how}), {megabytes:.1} MB in {}",
                    duration(seconds)
                ),
            )
        }
        Outcome::ConvertFailed(error) => (
            "failed",
            format!("playback copy failed: {}", one_line(error)),
        ),
    }
}

/// The downloads page's Playback copies: what the converter is on.
fn converter_html(converter: &Converter, job_link: &dyn Fn(&Job) -> String) -> String {
    let left = match converter.left {
        Some(left) if left > 0 => format!(" ({} to go)", thousands(left)),
        _ => String::new(),
    };
    match (&converter.current, converter.left) {
        (Some(current), _) => format!(
            "<ul class=\"jobs\">\n<li>{} — {}{left}</li>\n</ul>\n",
            job_link(&current.job),
            converting_html(current)
        ),
        (None, None) => "<p>Starting.</p>\n".into(),
        (None, Some(0)) => "<p>Every downloaded video plays in every browser.</p>\n".into(),
        (None, Some(_)) => format!("<p>Waiting{left}.</p>\n"),
    }
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

/// The `downloads` event: `{"count": n, "board": html, "videos": {"platform/id": html},
/// "pictures": {"platform/id": url}}`, with the sidebar count, the downloads
/// page's `board_html`, the Media field of each video in the pipeline, and
/// the saved picture of each video downloaded since the app started.
fn downloads_json(board: &Board) -> String {
    let converting = board.converter.current.as_ref().map(|c| &c.job);
    let videos: serde_json::Map<String, serde_json::Value> = board
        .active
        .iter()
        .map(|active| &active.job)
        .chain(converting)
        .filter_map(|job| {
            let (platform, id) = (&job.item.platform, &job.item.video_id);
            Some((
                format!("{platform}/{id}"),
                job_html(board, platform, id)?.into(),
            ))
        })
        .collect();
    // Downloaded since the app started, so a list showing them can swap its
    // placeholder for the saved picture.
    let pictures: serde_json::Map<String, serde_json::Value> = board
        .recent
        .iter()
        .filter_map(|finished| {
            let Outcome::Downloaded {
                picture: Some(file),
                ..
            } = &finished.outcome
            else {
                return None;
            };
            let (platform, id) = (&finished.job.item.platform, &finished.job.item.video_id);
            Some((
                format!("{platform}/{id}"),
                media_src(platform, id, file).into(),
            ))
        })
        .collect();
    serde_json::json!({
        "count": busy_count(board),
        "board": board_html(board),
        "videos": videos,
        "converting": converting.map(|job| format!("{}/{}", job.item.platform, job.item.video_id)),
        "pictures": pictures,
    })
    .to_string()
}

fn download_html(platform: &str, download: Option<&db::DownloadState>) -> String {
    let Some(download) = download else {
        return "not downloaded".into();
    };
    if let Some(at) = download.downloaded_at {
        #[expect(clippy::cast_precision_loss, reason = "a size for display")]
        let megabytes = download.size_bytes.unwrap_or(0) as f64 / 1e6;
        return format!(
            "downloaded {}, {megabytes:.1} MB, in <code>{}</code>",
            time(at, "datetime"),
            escape(download.folder.as_deref().unwrap_or(""))
        );
    }
    // yt-dlp's words for it are misleading (TikTok's "Your IP address is
    // blocked"), so they're left out.
    if let Some(at) = download.unavailable_at {
        return format!(
            "not downloaded: {} says this video is unavailable (as of {}), so it won't be tried again",
            escape(display_name(platform)),
            time(at, "date")
        );
    }
    let tries = if download.failures == 1 {
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
        download.failures
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

pub fn escape(text: &str) -> String {
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
            post_type: "video".into(),
            posted_at: None,
            added_at: 1_700_000_000,
            thumbnail: None,
            like_order: 1,
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
        assert!(row.starts_with(
            "<tr data-platform=\"youtube\" data-type=\"video\" data-key=\"youtube/abc\" data-order=\"1\">"
        ));
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
    fn downloaded_rows_show_the_saved_picture() {
        for platform in ["youtube", "tiktok", "instagram"] {
            let saved = db::StoredVideo {
                thumbnail: Some("thumbnail.jpg".into()),
                ..video(platform, Some("x"))
            };
            let row = video_row(&saved);
            assert!(row.contains(&format!(
                "data-key=\"{platform}/abc\" data-order=\"1\"><td class=\"thumb\"><a href=\"/videos/{platform}/abc\">\
                 <img src=\"/videos/{platform}/abc/media/thumbnail.jpg\""
            )));
            assert!(!row.contains("nothumb") && !row.contains("i.ytimg.com"));
        }
    }

    #[test]
    fn links_keep_the_filter_across_pages() {
        assert_eq!(list_href(None, 1), "/");
        assert_eq!(list_href(None, 2), "/?page=2");
        assert_eq!(list_href(Some("tiktok"), 1), "/?platform=tiktok");
        assert_eq!(list_href(Some("tiktok"), 3), "/?platform=tiktok&amp;page=3");
        let first = pager(&ListView::plain(None, 1), 3);
        assert!(!first.contains("Newer") && first.contains("Older"));
        let last = pager(&ListView::plain(None, 3), 3);
        assert!(last.contains("Newer") && !last.contains("Older"));
    }

    fn list_query(q: Option<&str>, sort: Option<&str>, dir: Option<&str>) -> ListQuery {
        ListQuery {
            platform: None,
            post_type: None,
            page: Some(2),
            q: q.map(Into::into),
            sort: sort.map(Into::into),
            dir: dir.map(Into::into),
        }
    }

    #[test]
    fn sort_and_search_live_in_the_url() {
        let view = ListView::from_query(
            Some("tiktok"),
            None,
            &list_query(Some(" cat & dog "), Some("duration"), Some("asc")),
        );
        assert_eq!(view.q, "cat & dog");
        assert_eq!(
            view.order,
            db::Order {
                sort: db::Sort::Length,
                dir: db::Dir::Asc
            }
        );
        assert_eq!(
            view.href(),
            "/?platform=tiktok&amp;q=cat%20%26%20dog&amp;sort=duration&amp;dir=asc&amp;page=2"
        );
        assert!(!view.live());
        // Paging keeps everything else.
        let next = pager(&view, 5);
        assert!(next.contains("sort=duration&amp;dir=asc&amp;page=3"));
        assert!(next.contains("Next →") && !next.contains("Older"));
    }

    #[test]
    fn a_search_defaults_to_best_match() {
        let view = ListView::from_query(None, None, &list_query(Some("cat"), None, None));
        assert_eq!(view.order.sort, db::Sort::Best);
        assert_eq!(view.at(1).href(), "/?q=cat");
        // Without a search, best match means nothing, and so does junk.
        for sort in [Some("best"), Some("rowid; DROP TABLE videos"), None] {
            let view = ListView::from_query(None, None, &list_query(None, sort, Some("up")));
            assert_eq!(view.order, db::Order::NEWEST, "for {sort:?}");
            assert_eq!(view.at(1).href(), "/");
        }
        let view = ListView::from_query(None, None, &list_query(None, Some("added"), Some("desc")));
        assert!(view.at(1).live());
    }

    #[test]
    fn headers_sort_and_flip() {
        let view = ListView::plain(None, 4);
        // A first click picks the column's own direction, from page 1.
        assert!(
            sort_header(&view, db::Sort::Title, "Title", "")
                .contains("href=\"/?sort=title&amp;dir=asc\"")
        );
        assert!(
            sort_header(&view, db::Sort::Length, "Length", "num")
                .contains("href=\"/?sort=duration&amp;dir=desc\"")
        );
        // The current one shows its arrow, and flips.
        let added = sort_header(&view, db::Sort::Added, "Added", "date");
        assert!(added.contains("aria-sort=\"descending\"") && added.contains("Added ▼"));
        assert!(added.contains("href=\"/?sort=added&amp;dir=asc\""));
        let title = ListView::from_query(None, None, &list_query(None, Some("title"), None));
        assert!(
            sort_header(&title, db::Sort::Title, "Title", "")
                .contains("href=\"/?sort=title&amp;dir=desc\"")
        );
    }

    #[test]
    fn the_search_form_keeps_the_platform() {
        let mut view = ListView::plain(Some("youtube"), 1);
        view.q = "\"piano\" -cat".into();
        view.order = view.default_order();
        let form = search_html(&view);
        assert!(form.contains("value=\"&quot;piano&quot; -cat\""));
        assert!(form.contains("name=\"platform\" value=\"youtube\""));
        assert!(form.contains("href=\"/?platform=youtube\">Clear"));
        assert!(!form.contains("Best match"));
        let sorted = view.ordered(db::Order::NEWEST);
        assert!(search_html(&sorted).contains(">Best match"));
    }

    #[test]
    fn counts_cover_every_platform_and_post_type() {
        let count = |platform: &str, post_type: &str, count| db::VideoCount {
            platform: platform.into(),
            post_type: post_type.into(),
            count,
        };
        let counts = [
            count("youtube", "video", 3),
            count("tiktok", "video", 2),
            count("tiktok", "slideshow", 1),
        ];
        assert_eq!(count_of(&counts, None, None), 6);
        assert_eq!(count_of(&counts, Some("tiktok"), None), 3);
        assert_eq!(count_of(&counts, Some("tiktok"), Some("slideshow")), 1);
        assert_eq!(count_of(&counts, Some("instagram"), None), 0);
        assert_eq!(
            counts_json(&counts),
            r#"{"all":6,"instagram":0,"instagram/carousel":0,"instagram/photo":0,"instagram/video":0,"tiktok":3,"tiktok/slideshow":1,"tiktok/video":2,"youtube":3}"#
        );
    }

    #[test]
    fn post_types_sit_under_their_platform() {
        let view = ListView {
            post_type: Some("slideshow"),
            q: "cat".into(),
            order: db::Order {
                sort: db::Sort::Best,
                dir: db::Dir::Asc,
            },
            ..ListView::plain(Some("tiktok"), 3)
        };
        assert_eq!(
            view.href(),
            "/?platform=tiktok&amp;type=slideshow&amp;q=cat&amp;page=3"
        );
        // The search box keeps the type.
        assert!(search_html(&view).contains("name=\"type\" value=\"slideshow\""));
        let frame = Frame {
            title: "TikTok slideshows",
            current: Section::PostType("tiktok", "slideshow"),
            list: Some(&view),
            status: &SyncStatus::default(),
            counts: &[],
            board: &Board::default(),
            footer: String::new(),
            after: 0,
            live: false,
            player: false,
        };
        let entries = nav_entries(&frame);
        let labels: Vec<_> = entries.iter().map(|e| (e.label, e.sub)).collect();
        assert_eq!(
            labels,
            [
                ("All likes", false),
                ("YouTube", false),
                ("TikTok", false),
                ("Videos", true),
                ("Slideshows", true),
                ("Instagram", false),
                ("Videos", true),
                ("Photos", true),
                ("Carousels", true),
                ("TV", false),
                ("Downloading", false),
                ("Stats", true),
            ]
        );
        // A platform's link drops the type; a type's keeps the search.
        assert_eq!(entries[2].href, "/?platform=tiktok&amp;q=cat");
        assert_eq!(
            entries[7].href,
            "/?platform=instagram&amp;type=photo&amp;q=cat"
        );
        assert_eq!(entries[4].count_name, "tiktok/slideshow");
        // Only the type's own entry is current, and the top bar has no types.
        let html = layout(&frame, "");
        assert_eq!(html.matches("aria-current").count(), 1);
        assert!(html.contains("<li class=\"sub\"><a href=\"/?platform=tiktok&amp;type=slideshow&amp;q=cat\" aria-current=\"page\">Slideshows"));
        assert!(html.contains("data-type=\"slideshow\""));
        // Stats sits in the panel's Downloads menu, under Downloading.
        let downloads = &html[html.find("<h2>Downloads</h2>").unwrap()..];
        assert!(downloads[..downloads.find("</ul>").unwrap()].contains("href=\"/stats\""));
    }

    #[test]
    fn status_shows_each_platform_in_order() {
        let mut status = SyncStatus::default();
        status.set("tiktok", SyncState::Running);
        status.set(
            "youtube",
            SyncState::Failed {
                at: 0,
                advice: "you're not logged in to YouTube in Brave".into(),
                error: "cookies: \"no session\"\nhere".into(),
                paused: Vec::new(),
            },
        );
        status.set(
            "tiktok",
            SyncState::Done {
                at: 0,
                added: 2,
                paused: Vec::new(),
            },
        );
        let html = status_html(&status);
        assert!(
            html.starts_with("<li class=\"sync-failed\"><span class=\"led\"></span><b>YouTube</b>")
        );
        assert!(html.contains("didn't sync at"));
        assert!(html.contains(": you&#39;re not logged in to YouTube in Brave</span>"));
        // The error itself is there on hover.
        assert!(html.contains("title=\"cookies: &quot;no session&quot;\nhere\""));
        let tiktok = html.find("<li class=\"sync-done\"><span class=\"led\"></span><b>TikTok</b>");
        assert!(tiktok.is_some_and(|at| at > 0));
        assert!(html.contains("2 new"));
        assert!(html.contains("<b>Instagram</b><span class=\"what\">waiting to sync</span>"));
        status.set(
            "instagram",
            SyncState::Done {
                at: 0,
                added: 0,
                paused: vec![Paused {
                    list: "liked",
                    until: 10_800,
                }],
            },
        );
        assert!(status_html(&status).contains(
            "0 new. Liked posts wait until <time data-ts=\"10800\" data-format=\"time\">10800</time>, \
             as Instagram asked us to slow down</span>"
        ));
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
                added_on: "2026-10-12".into(),
            },
            title: "<b>".into(),
        }
    }

    #[test]
    fn media_offers_a_button_only_when_there_is_nothing_saved() {
        let board = Board::default();
        let fresh = video("youtube", None);
        assert!(
            media_html(&fresh, None, None, &board)
                .contains("action=\"/videos/youtube/abc/download\"")
        );

        let failed = db::DownloadState {
            failures: 2,
            last_error: Some("429".into()),
            unavailable_at: None,
            downloaded_at: None,
            folder: None,
            files: Vec::new(),
            size_bytes: None,
        };
        let html = media_html(&fresh, Some(&failed), None, &board);
        assert!(html.contains("2 failed tries, last error: 429"));
        assert!(html.contains("<button"));

        // TikTok said it's gone: no button, and none of yt-dlp's misleading
        // words for it.
        let tiktok = video("tiktok", None);
        let unavailable = db::DownloadState {
            failures: 1,
            last_error: Some("yt-dlp failed: ERROR: Your IP address is blocked".into()),
            unavailable_at: Some(1),
            ..failed.clone()
        };
        let html = media_html(&tiktok, Some(&unavailable), None, &board);
        assert!(!html.contains("<button"));
        assert!(html.contains("not downloaded: TikTok says this video is unavailable"));
        assert!(!html.contains("IP address"));

        let saved = db::DownloadState {
            downloaded_at: Some(1),
            folder: Some("youtube/abc".into()),
            size_bytes: Some(1),
            ..failed
        };
        assert!(!media_html(&fresh, Some(&saved), None, &board).contains("<button"));
        let gone = db::StoredVideo {
            available: false,
            ..fresh
        };
        assert!(!media_html(&gone, None, None, &board).contains("<button"));
    }

    fn downloaded() -> db::DownloadState {
        db::DownloadState {
            failures: 0,
            last_error: None,
            unavailable_at: None,
            downloaded_at: Some(1),
            folder: Some("youtube/abc".into()),
            files: vec!["video.webm".into()],
            size_bytes: Some(1),
        }
    }

    fn playback(file: Option<&str>, method: &str) -> db::PlaybackState {
        db::PlaybackState {
            download_folder: "youtube/abc".into(),
            failures: 0,
            last_error: None,
            finished_at: Some(1),
            file_name: file.map(Into::into),
            size_bytes: Some(2_500_000),
            method: Some(method.into()),
            original_config_json: Some("{}".into()),
        }
    }

    #[test]
    fn media_says_where_the_playback_copy_stands() {
        let board = Board::default();
        let fresh = video("youtube", None);
        let saved = downloaded();
        let line =
            |state: Option<&db::PlaybackState>| media_html(&fresh, Some(&saved), state, &board);
        assert!(line(None).contains("<br>playback copy: waiting its turn"));
        assert!(
            line(Some(&playback(Some("playback.mp4"), "encode")))
                .contains("(re-encoded to H.264), 2.5 MB")
        );
        assert!(
            line(Some(&playback(Some("playback.mp4"), "remux")))
                .contains("the picture as it is, the sound in AAC")
        );
        assert!(
            line(Some(&playback(None, "none"))).contains("plays in every browser as downloaded")
        );
        let failed = db::PlaybackState {
            finished_at: None,
            failures: 1,
            last_error: Some("ffmpeg <failed>".into()),
            ..playback(None, "encode")
        };
        assert!(
            line(Some(&failed)).contains(
                "playback copy not made; 1 failed try, last error: ffmpeg &lt;failed&gt;"
            )
        );
        // Photo posts have nothing to convert.
        let photo = db::StoredVideo {
            kind: "photo".into(),
            ..video("tiktok", None)
        };
        assert!(!media_html(&photo, Some(&saved), None, &board).contains("playback"));
    }

    #[test]
    fn the_copy_belongs_to_the_files_it_was_made_from() {
        let saved = downloaded();
        let made = playback(Some("playback.mp4"), "encode");
        let copy = playback_copy(&saved, Some(&made)).unwrap();
        assert_eq!(copy.file, "playback.mp4");
        assert_eq!(copy.source, Some("{}"));
        let elsewhere = db::DownloadState {
            folder: Some("youtube/other".into()),
            ..downloaded()
        };
        assert!(playback_copy(&elsewhere, Some(&made)).is_none());
        assert!(playback_copy(&saved, Some(&playback(None, "none"))).is_none());
        let unfinished = db::PlaybackState {
            finished_at: None,
            ..made.clone()
        };
        assert!(playback_copy(&saved, Some(&unfinished)).is_none());

        let fresh = video("youtube", None);
        let (screen, playing) = screen_html(&fresh, Some(&saved), Some(&made));
        assert!(playing);
        assert!(screen.contains("data-copy=\"/videos/youtube/abc/media/playback.mp4\""));
        let (screen, _) = screen_html(&fresh, Some(&saved), None);
        assert!(screen.contains("data-convert=\"/videos/youtube/abc/playback\""));
    }

    #[test]
    fn media_and_the_board_show_the_converter() {
        let youtube = video("youtube", None);
        let mut board = Board::default();
        assert!(board_html(&board).contains("<h2>Playback copies</h2>\n<p>Starting.</p>"));
        board.converter.left = Some(0);
        assert!(board_html(&board).contains("Every downloaded video plays in every browser."));

        board.converter = Converter {
            left: Some(1500),
            current: Some(Converting {
                job: job(&youtube),
                started_at: 0,
                progress: Some(crate::transcode::Progress {
                    done: 30.0,
                    total: Some(120.0),
                    speed: Some(2.0),
                }),
            }),
        };
        let html = media_html(&youtube, Some(&downloaded()), None, &board);
        assert!(html.contains("data-busy=\"convert\""));
        assert!(html.contains("making a playback copy: 25% of 2:00, 2.0× real time, 0:45 left"));
        let page = board_html(&board);
        assert!(page.contains("<h2>Playback copies</h2>\n<ul class=\"jobs\">\n<li>YouTube: <a"));
        assert!(page.contains("(1,500 to go)</li>"));

        let update: serde_json::Value = serde_json::from_str(&downloads_json(&board)).unwrap();
        assert_eq!(update["converting"], "youtube/abc");
        assert!(
            update["videos"]["youtube/abc"]
                .as_str()
                .unwrap()
                .starts_with("making a playback copy")
        );
        assert_eq!(update["count"], 0);

        board.recent.push_front(crate::pipeline::Finished {
            job: job(&youtube),
            at: 0,
            outcome: Outcome::Converted {
                bytes: 3_000_000,
                seconds: 75,
                method: "encode",
            },
        });
        assert!(board_html(&board).contains("playback copy made (re-encoded), 3.0 MB in 1:15"));
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
        let html = media_html(&tiktok, None, None, &board);
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
        let html = media_html(&youtube, None, None, &board);
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
                picture: Some("thumbnail.webp".into()),
            },
        });
        let html = board_html(&board);
        assert!(html.contains("Automatic downloads are off."));
        assert!(html.contains("Nothing right now."));
        let downloaded = html.find("downloaded 12.3 MB in 1:15").unwrap();
        let failed = html.find("failed: bad thing").unwrap();
        assert!(downloaded < failed);

        // Only the downloaded video's picture goes to the list.
        let update: serde_json::Value = serde_json::from_str(&downloads_json(&board)).unwrap();
        assert_eq!(
            update["pictures"],
            serde_json::json!({"youtube/abc": "/videos/youtube/abc/media/thumbnail.webp"})
        );
    }

    #[test]
    fn radio_motion_reads() {
        let motion = RADIO_MOTION.as_ref().unwrap();
        for sheet in [&motion.dome, &motion.knob] {
            let [x, y, w, h] = sheet.r#box;
            assert!(x + w <= motion.width && y + h <= motion.height);
        }
        assert!(radio_html().contains("radio-dome-lit.webp"));
    }

    #[tokio::test]
    async fn an_asset_already_had_is_not_sent_again() {
        let Ok(first) = asset(Path("style.css".into()), HeaderMap::new()).await else {
            panic!("style.css isn't served");
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, first.headers()[header::ETAG].clone());
        let Ok(again) = asset(Path("style.css".into()), headers).await else {
            panic!("style.css isn't served");
        };
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(etag(b"a"), etag(b"a"));
        assert_ne!(etag(b"a"), etag(b"b"));
    }

    #[test]
    fn every_linked_asset_is_served() {
        let page = layout(
            &Frame {
                title: "t",
                current: Section::Downloading,
                list: None,
                status: &SyncStatus::default(),
                counts: &[],
                board: &Board::default(),
                footer: String::new(),
                after: 0,
                live: false,
                player: true,
            },
            "",
        );
        assert!(page.contains(concat!("/releases/tag/v", env!("CARGO_PKG_VERSION"))));
        let css = |name| {
            let (.., body) = ASSETS.iter().find(|(file, ..)| *file == name).unwrap();
            std::str::from_utf8(body).unwrap()
        };
        let mut linked = 0;
        for text in [page.as_str(), css("style.css"), css("player.css")] {
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

    #[test]
    fn byte_ranges() {
        let range = |r: &str| byte_range(&HeaderValue::from_str(r).unwrap(), 1000);
        assert_eq!(range("bytes=0-"), Some((0, 999)));
        assert_eq!(range("bytes=100-199"), Some((100, 199)));
        assert_eq!(range("bytes=900-5000"), Some((900, 999)));
        assert_eq!(range("bytes=-100"), Some((900, 999)));
        assert_eq!(range("bytes=-5000"), Some((0, 999)));
        assert_eq!(range("bytes=1000-"), None);
        assert_eq!(range("bytes=5-1"), None);
        assert_eq!(range("items=0-1"), None);
        assert_eq!(byte_range(&HeaderValue::from_static("bytes=0-"), 0), None);
    }

    #[test]
    fn percentages_round_down_but_show_a_start() {
        assert_eq!(percent(0, 200), ("0%".into(), "0.0%".into()));
        assert_eq!(percent(1, 200), ("&lt;1%".into(), "0.5%".into()));
        assert_eq!(percent(1999, 2000), ("99%".into(), "99.9%".into()));
        assert_eq!(percent(2000, 2000), ("100%".into(), "100.0%".into()));
        assert_eq!(percent(0, 0).0, "–");
    }

    #[test]
    fn stats_add_up_the_platforms() {
        let youtube = db::CatchUp {
            platform: "youtube".into(),
            liked: 10,
            gone: 2,
            downloaded: 8,
            videos: 8,
            checked: 8,
            copies: 6,
            ..db::CatchUp::default()
        };
        let tiktok = db::CatchUp {
            platform: "tiktok".into(),
            liked: 1000,
            downloaded: 250,
            failing: 3,
            videos: 200,
            checked: 150,
            copies: 140,
            ..db::CatchUp::default()
        };
        let html = stats_html(&[tiktok, youtube]);
        let all = html.find("All platforms").unwrap();
        let first = html.find("YouTube").unwrap();
        assert!(all < first && first < html.find("TikTok").unwrap());
        assert!(html.contains("<strong>1,010</strong> likes found"));
        assert!(html.contains("2 gone from their platform"));
        assert!(html.contains("258 of 1,008 · 750 to go · 3 failed, trying again later"));
        assert!(html.contains("All downloaded"));
        assert!(
            html.contains("150 of 200 downloaded videos checked · 140 needed a copy · 50 waiting")
        );
        assert!(html.contains("<strong class=\"stat-pct\">25%</strong>"));
        assert!(html.contains("stat-meter done"));
        assert!(stats_html(&[]).contains("No likes found yet"));
    }
}
