//! Lists the posts a user has saved (the bookmark under a post) and liked on
//! Instagram, and fetches a photo post's pictures and music for the
//! download. yt-dlp has no extractor for either list, so here we parse
//! Instagram's JSON ourselves; it downloads each video afterwards.
//!
//! The two lists come from different places:
//! - The saved list is the web app's own endpoint for the Saved tab. It needs
//!   nothing but the login cookies and a couple of headers, but it's called
//!   with `fetch` from inside an instagram.com tab (`headless.rs`) anyway, so
//!   the request comes from a real browser with the user's cookies, like the
//!   web app's.
//! - The liked list only exists in the phone app's API: with a browser's user
//!   agent it answers "useragent mismatch", and the web shows likes only in a
//!   Bloks screen with no links to read. So it's asked for directly, with the
//!   browser's cookies and the Android app's user agent, which a page's
//!   `fetch` can't set.
//!
//! Photo posts and carousels are kept as `photo` posts. yt-dlp can't fetch
//! their pictures, and both lists only offer them at 720 or 1080 pixels wide,
//! so `photo_post` reads the full-size ones from the post's own page.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::browser::{Browser, Cookie, LoggedOut};
use crate::headless::Headless;

/// Any same-origin page will do, since the API needs no page script. This one
/// is tiny and runs none of Instagram's app, which would make requests of its
/// own (marking stories seen, say).
const START_PAGE: &str = "https://www.instagram.com/robots.txt";

/// The blank tab the browser starts on is also "complete", so check the host too.
const PAGE_READY: &str =
    "location.hostname === 'www.instagram.com' && document.readyState === 'complete'";

/// The id the web app sends as `x-ig-app-id`. The API refuses requests without it.
const WEB_APP_ID: &str = "936619743392459";

/// The liked list, in the phone app's API.
const LIKED_URL: &str = "https://i.instagram.com/api/v1/feed/liked/";

/// The Android app's user agent and app id, which the liked list wants in
/// place of a browser's.
const APP_USER_AGENT: &str = "Instagram 361.0.0.46.88 Android (34/14; 480dpi; 1080x2340; \
     samsung; SM-S911B; dm1q; qcom; en_US; 674675155)";
const ANDROID_APP_ID: &str = "567067343352427";

/// Instagram is quick to answer bursts of requests with "Please wait a few
/// minutes". The apps load a page per scroll; this keeps us near that, for
/// both lists.
const PAGE_PAUSE: Duration = Duration::from_secs(1);

/// How many pages go by between the progress lines a long read logs.
const PROGRESS_EVERY: u32 = 10;

/// Now and then a page fails (a 5xx, a dropped connection) that works a
/// moment later, so each page gets a few tries.
const PAGE_ATTEMPTS: u32 = 4;

/// Instagram's `media_type`s.
const PHOTO: u64 = 1;
const VIDEO: u64 = 2;
const CAROUSEL: u64 = 8;

/// A saved or liked post.
#[derive(Debug, Clone)]
pub struct Post {
    /// The post's shortcode, as in its URL (`instagram.com/reel/<code>/`).
    pub code: String,
    /// The caption, often empty.
    pub caption: String,
    /// The author's numeric id. Unlike the handle, it never changes.
    pub author_id: Option<String>,
    /// The author's username, without the @.
    pub author_handle: Option<String>,
    pub duration_seconds: Option<u64>,
    /// A photo, or a carousel of photos and videos, rather than a video.
    pub is_photo: bool,
    /// A reel, rather than a post to the feed. Only the URL differs.
    pub is_reel: bool,
    /// When it was posted, in unix seconds.
    pub posted_at: Option<i64>,
    /// The media item from Instagram's API minus its media links (see
    /// `without_media_links`), kept so fields we don't parse yet aren't lost.
    pub raw: Value,
}

impl Post {
    pub fn url(&self) -> String {
        let kind = if self.is_reel { "reel" } else { "p" };
        format!("https://www.instagram.com/{kind}/{}/", self.code)
    }
}

/// One page of a list.
#[derive(Debug)]
struct Page {
    posts: Vec<Post>,
    /// Where the next page starts, when there is one.
    next_max_id: Option<String>,
}

/// Where a list's pages come from.
enum Feed<'a> {
    /// The saved list, fetched from inside an instagram.com tab.
    Saved(&'a mut Headless),
    /// The liked list, fetched from the phone app's API.
    Liked(&'a AppLogin),
}

impl Feed<'_> {
    fn name(&self) -> &'static str {
        match self {
            Feed::Saved(_) => "saved",
            Feed::Liked(_) => "liked",
        }
    }

    /// Fetch the page starting at `max_id` (the first when None); returns the
    /// HTTP status and body.
    async fn request(&mut self, max_id: Option<&str>) -> Result<(u64, String)> {
        match self {
            Feed::Saved(tab) => {
                let got = tab.eval(&saved_page_request(max_id)).await?;
                let status = got["status"].as_u64().unwrap_or(0);
                Ok((status, got["body"].as_str().unwrap_or("").to_string()))
            }
            Feed::Liked(login) => liked_page_request(login, max_id).await,
        }
    }
}

/// The login as the app's API takes it: the browser's cookies in a `Cookie`
/// header, and the CSRF token, which it checks against the cookie.
#[derive(Clone)]
struct AppLogin {
    cookie_header: String,
    csrf_token: String,
}

impl AppLogin {
    fn new(cookies: &[Cookie]) -> Self {
        let cookie_header = cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        let csrf_token = cookies
            .iter()
            .find(|c| c.name == "csrftoken")
            .map(|c| c.value.clone())
            .unwrap_or_default();
        AppLogin {
            cookie_header,
            csrf_token,
        }
    }
}

/// Checked up front so a user who doesn't use Instagram costs nothing: no
/// browser launch or request, and a clear message instead of a login error.
pub fn check_logged_in(cookies: &[Cookie]) -> Result<()> {
    if !cookies.iter().any(|c| c.name == "sessionid") {
        return Err(LoggedOut("Instagram").into());
    }
    Ok(())
}

/// Which of Instagram's lists to read.
#[derive(Debug, Clone, Copy)]
pub enum List {
    /// The posts the user saved.
    Saved,
    /// The posts the user liked.
    Liked,
}

/// List the posts on `list` page by page, newest first, starting at
/// `cursor` (None for the start of the list). `on_page` gets each page's
/// posts and the cursor for the page after it (None after the last page),
/// and returns whether to go on, so callers can store each page as it comes
/// and quit once they reach posts they already have.
pub async fn posts(
    list: List,
    browser: Browser,
    cookies: &[Cookie],
    cursor: Option<&str>,
    mut on_page: impl FnMut(Vec<Post>, Option<&str>) -> Result<bool>,
) -> Result<()> {
    check_logged_in(cookies)?;
    match list {
        List::Saved => {
            let mut tab = Headless::launch(browser, cookies).await?;
            let result = async {
                tab.goto(START_PAGE, PAGE_READY).await?;
                read(Feed::Saved(&mut tab), cursor, &mut on_page).await
            }
            .await;
            tab.close().await;
            result
        }
        List::Liked => {
            let login = AppLogin::new(cookies);
            read(Feed::Liked(&login), cursor, &mut on_page).await
        }
    }
}

async fn read(
    mut feed: Feed<'_>,
    cursor: Option<&str>,
    on_page: &mut impl FnMut(Vec<Post>, Option<&str>) -> Result<bool>,
) -> Result<()> {
    let mut max_id = cursor.map(str::to_string);
    let mut listed = 0;
    for pages in 1_u32.. {
        // A full read of a long list takes many minutes; say how it's going.
        if pages % PROGRESS_EVERY == 0 {
            eprintln!("instagram: {} list, {listed} posts so far", feed.name());
        }
        let page = fetch_page(&mut feed, max_id.as_deref()).await?;
        // A cursor that doesn't move would fetch the same page forever.
        let next = page
            .next_max_id
            .filter(|next| max_id.as_ref() != Some(next));
        listed += page.posts.len();
        if !on_page(page.posts, next.as_deref())? {
            return Ok(());
        }
        match next {
            Some(next) => max_id = Some(next),
            None => return Ok(()),
        }
        tokio::time::sleep(PAGE_PAUSE).await;
    }
    Ok(())
}

/// Fetch the page of `feed` that starts at `max_id` (the first page when
/// None), retrying failures that may be passing.
async fn fetch_page(feed: &mut Feed<'_>, max_id: Option<&str>) -> Result<Page> {
    let mut attempt = 1;
    loop {
        let result = async {
            let (status, body) = feed.request(max_id).await?;
            parse_page(status, &body)
        }
        .await;
        match result {
            Ok(page) => return Ok(page),
            Err(e) if attempt < PAGE_ATTEMPTS && !is_final(&e) => {
                eprintln!(
                    "instagram: {} list page failed ({e:#}); retrying",
                    feed.name()
                );
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
                attempt += 1;
            }
            Err(e) => {
                let at = max_id.unwrap_or("the start");
                return Err(e).with_context(|| format!("{} list page at {at}", feed.name()));
            }
        }
    }
}

/// Errors a retry won't fix, which retrying would only make worse.
#[derive(Debug)]
struct Refused {
    message: String,
    why: Refusal,
}

/// Why Instagram turned a request away, for what the UI tells the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The login lapsed: log in again in the browser.
    LoggedOut,
    /// Instagram wants the user to confirm it's them, in the browser.
    Checkpoint,
    /// Instagram is telling us to slow down (HTTP 429, or its
    /// `feedback_required` block). Asking again before it lifts only makes
    /// it last longer, so the list it came from should sit out a while.
    SlowDown,
    /// Anything else, such as "useragent mismatch".
    Other,
}

impl Refused {
    fn new(why: Refusal, message: String) -> Self {
        Self { message, why }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

fn is_final(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>().is_some()
}

/// Why Instagram turned away the request `e` came from, if it did.
pub fn refusal(e: &anyhow::Error) -> Option<Refusal> {
    e.downcast_ref::<Refused>().map(|r| r.why)
}

/// Whether `e` is Instagram telling us to slow down.
pub fn is_throttled(e: &anyhow::Error) -> bool {
    refusal(e) == Some(Refusal::SlowDown)
}

/// Instagram telling us to slow down, for other modules' tests.
#[cfg(test)]
pub fn slow_down_for_tests() -> anyhow::Error {
    Refused::new(
        Refusal::SlowDown,
        "Instagram is rate limiting us (HTTP 429)".into(),
    )
    .into()
}

/// The page script that fetches one page of the saved list and returns its
/// HTTP status and body. This is the endpoint the profile's Saved tab uses.
fn saved_page_request(max_id: Option<&str>) -> String {
    // serde_json quotes and escapes the value for use as a JS string literal.
    let query = match max_id {
        Some(max_id) => format!(
            "'?' + new URLSearchParams({{max_id: {}}})",
            Value::from(max_id)
        ),
        None => "''".to_string(),
    };
    api_fetch(&format!("'/api/v1/feed/saved/posts/' + {query}"))
}

/// A page script that fetches the web API's `url` (a JS expression) and
/// returns its HTTP status and body, sent the way the web app sends it.
fn api_fetch(url: &str) -> String {
    // The API checks the CSRF header against the csrftoken cookie, as for
    // any request from the web app.
    format!(
        "(async () => {{ \
           const csrf = (document.cookie.match(/(?:^|; )csrftoken=([^;]*)/) || [])[1] || ''; \
           const r = await fetch({url}, {{ \
             credentials: 'include', \
             headers: {{'x-ig-app-id': '{WEB_APP_ID}', 'x-csrftoken': csrf, 'x-requested-with': 'XMLHttpRequest'}} \
           }}); \
           return {{status: r.status, body: await r.text()}}; \
         }})()"
    )
}

/// Fetch one page of the liked list from the phone app's API.
async fn liked_page_request(login: &AppLogin, max_id: Option<&str>) -> Result<(u64, String)> {
    let login = login.clone();
    let max_id = max_id.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_mins(1)))
            // A refusal's body says why; parse_page reads it.
            .http_status_as_error(false)
            .build()
            .into();
        let mut request = agent
            .get(LIKED_URL)
            .header("User-Agent", APP_USER_AGENT)
            .header("X-IG-App-ID", ANDROID_APP_ID)
            .header("X-CSRFToken", &login.csrf_token)
            .header("Cookie", &login.cookie_header);
        if let Some(max_id) = &max_id {
            request = request.query("max_id", max_id);
        }
        let mut response = request.call().context("requesting the liked list")?;
        let status = u64::from(response.status().as_u16());
        // A page of 21 posts is about a megabyte.
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .context("reading the liked list")?;
        Ok((status, body))
    })
    .await
    .context("the liked list request stopped")?
}

fn parse_page(status: u64, body: &str) -> Result<Page> {
    let page = parse_response(status, body, "list")?;
    let posts = page
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                // The saved list wraps each post as `{"media": …}`; the liked
                // list doesn't.
                .map(|item| item.get("media").unwrap_or(item))
                .filter_map(parse_post)
                .collect()
        })
        .unwrap_or_default();
    let more = page
        .get("more_available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The saved list's cursor is a string, the liked list's a number.
    let next_max_id = match page.get("next_max_id") {
        Some(Value::String(id)) => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    };
    Ok(Page {
        posts,
        next_max_id: next_max_id.filter(|id| more && !id.is_empty()),
    })
}

/// An API response's JSON, or why Instagram refused the request. `what` names
/// the response in errors.
fn parse_response(status: u64, body: &str, what: &str) -> Result<Value> {
    let page: Option<Value> = serde_json::from_str(body).ok();
    let message = page
        .as_ref()
        .and_then(|p| p.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    };
    // A lapsed login comes back as a 401 or 403 (`login_required`), or as a
    // redirect to the login page, which fetch follows to an HTML page.
    if matches!(status, 401 | 403) || message == "login_required" {
        return Err(Refused::new(
            Refusal::LoggedOut,
            format!(
            "Instagram wants a login (HTTP {status}{detail}); log in to Instagram in the browser"
        ),
        )
        .into());
    }
    // Instagram wants the user to confirm it's them before it answers again.
    if message == "checkpoint_required" || message == "challenge_required" {
        return Err(Refused::new(
            Refusal::Checkpoint,
            format!(
            "Instagram wants you to confirm it's you (HTTP {status}{detail}); open Instagram in the browser"
        ),
        )
        .into());
    }
    // "Please wait a few minutes before you try again": more requests now
    // only extend the wait.
    if status == 429 {
        return Err(Refused::new(
            Refusal::SlowDown,
            format!("Instagram is rate limiting us (HTTP 429{detail})"),
        )
        .into());
    }
    // "Try Again Later. We restrict certain activity to protect our
    // community": Instagram's block on what looks automated. It lifts after
    // hours or a day, sooner if left alone.
    if message == "feedback_required" {
        return Err(Refused::new(
            Refusal::SlowDown,
            format!("Instagram is limiting how fast we read (HTTP {status}{detail})"),
        )
        .into());
    }
    // A request Instagram won't take, such as "useragent mismatch" when it
    // stops accepting the app's user agent. Asking again changes nothing.
    if status == 400 {
        return Err(Refused::new(
            Refusal::Other,
            format!("Instagram refused the request (HTTP 400{detail})"),
        )
        .into());
    }
    if status != 200 {
        bail!("Instagram answered HTTP {status}{detail}");
    }
    let page = page.with_context(|| format!("the {what} response isn't JSON (logged out?)"))?;
    if page.get("status").and_then(Value::as_str) != Some("ok") {
        bail!("Instagram answered status {:?}{detail}", page.get("status"));
    }
    Ok(page)
}

/// A saved or liked post, or None for a kind we don't keep (Instagram has
/// others, such as guides) or an item missing its code.
fn parse_post(media: &Value) -> Option<Post> {
    let media_type = media.get("media_type").and_then(Value::as_u64)?;
    if ![PHOTO, VIDEO, CAROUSEL].contains(&media_type) {
        return None;
    }
    let text = |path: &str| {
        media
            .pointer(path)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Post {
        code: text("/code")?,
        caption: text("/caption/text").unwrap_or_default(),
        // Instagram sends ids as strings, but `pk` was once a number.
        author_id: text("/user/pk").or_else(|| {
            media
                .pointer("/user/pk")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
        }),
        author_handle: text("/user/username"),
        duration_seconds: duration_seconds(media),
        is_photo: media_type != VIDEO,
        is_reel: media.get("product_type").and_then(Value::as_str) == Some("clips"),
        posted_at: media.get("taken_at").and_then(Value::as_i64),
        raw: without_media_links(media),
    })
}

/// The video's length in whole seconds. Instagram sends it as a fraction
/// (17.996); round to the nearest.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "checked positive and finite; clips are minutes long, far inside u64"
)]
fn duration_seconds(media: &Value) -> Option<u64> {
    media
        .get("video_duration")
        .and_then(Value::as_f64)
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| d.round() as u64)
}

/// A photo post or carousel's media, found through Instagram's site.
pub struct PhotoPost {
    /// The post from the API, minus its media links, as the download's metadata.
    pub item: Value,
    /// The pictures and videos in order, each at its largest size.
    pub slides: Vec<Slide>,
    /// The first video's cover, for a carousel that opens with a video: the
    /// picture the library shows for the post.
    pub cover: Option<String>,
    /// The music that plays over the pictures.
    pub sound: Option<Sound>,
}

/// One picture or video of a post.
#[derive(Debug, PartialEq)]
pub struct Slide {
    pub url: String,
    pub is_video: bool,
}

/// A post's music: the whole track, and the part of it the post plays.
#[derive(Debug, PartialEq)]
pub struct Sound {
    pub url: String,
    /// Where the post's part starts and how long it is, in milliseconds.
    pub clip: Option<(u64, u64)>,
}

/// Find a photo post's pictures (and a carousel's videos) at full size, and
/// its music. The media links expire within days, so this is done at
/// download time, and the caller fetches the files straight away.
pub async fn photo_post(
    browser: Browser,
    cookies: &[Cookie],
    url: &str,
    code: &str,
) -> Result<PhotoPost> {
    check_logged_in(cookies)?;
    let mut tab = Headless::launch(browser, cookies).await?;
    let result = find_photo_post(&mut tab, url, code).await;
    tab.close().await;
    result
}

async fn find_photo_post(tab: &mut Headless, url: &str, code: &str) -> Result<PhotoPost> {
    // The post's page carries the post in its server-rendered data, with
    // every picture at the size it was posted. The API only offers 720 or
    // 1080 pixels wide.
    tab.goto(url, PAGE_READY).await?;
    let mut media = Value::Null;
    for _ in 0..3 {
        media = tab.eval(&post_in_page(code)).await?;
        if !media.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    if media.is_null() {
        bail!("Instagram's page for {url} has no post (deleted or private?)");
    }
    let slides = slides(&media)?;
    let cover = if slides[0].is_video {
        let first = media.pointer("/carousel_media/0").unwrap_or(&media);
        largest(first.pointer("/image_versions2/candidates"))
    } else {
        None
    };

    // The page's data has no music; the post's API entry does.
    let pk = match media.get("pk") {
        Some(Value::String(pk)) => pk.clone(),
        Some(Value::Number(pk)) => pk.to_string(),
        _ => bail!("Instagram's page for {url} has no post id"),
    };
    if !pk.chars().all(|c| c.is_ascii_digit()) {
        bail!("unusual post id {pk:?}");
    }
    let got = tab
        .eval(&api_fetch(&format!("'/api/v1/media/{pk}/info/'")))
        .await?;
    let info = parse_response(
        got["status"].as_u64().unwrap_or(0),
        got["body"].as_str().unwrap_or(""),
        "post",
    )?;
    let item = info
        .pointer("/items/0")
        .context("Instagram's entry for the post is empty")?;
    Ok(PhotoPost {
        item: without_media_links(item),
        slides,
        cover,
        sound: sound(item),
    })
}

/// A page script that finds the post `code` in the page's server-rendered
/// data (JSON in `<script>` tags, the post somewhere deep inside), or null.
fn post_in_page(code: &str) -> String {
    let code = Value::from(code);
    format!(
        "(() => {{ \
           const code = {code}; \
           const find = (o) => {{ \
             if (!o || typeof o !== 'object') return null; \
             if (o.code === code && (o.image_versions2 || o.carousel_media)) return o; \
             for (const v of Object.values(o)) {{ const found = find(v); if (found) return found; }} \
             return null; \
           }}; \
           for (const s of document.querySelectorAll('script[type=\"application/json\"]')) {{ \
             if (!s.textContent.includes(code)) continue; \
             try {{ const found = find(JSON.parse(s.textContent)); if (found) return found; }} catch {{}} \
           }} \
           return null; \
         }})()"
    )
}

/// The pictures and videos of `media` (a photo, or a carousel), in order,
/// each at its largest size.
fn slides(media: &Value) -> Result<Vec<Slide>> {
    let items: Vec<&Value> = match media.get("carousel_media").and_then(Value::as_array) {
        Some(children) => children.iter().collect(),
        None => vec![media],
    };
    let slides = items
        .into_iter()
        .map(|item| {
            let is_video = item.get("media_type").and_then(Value::as_u64) == Some(VIDEO);
            let versions = if is_video {
                item.get("video_versions")
            } else {
                item.pointer("/image_versions2/candidates")
            };
            let url = largest(versions).with_context(|| {
                let kind = if is_video { "video" } else { "picture" };
                format!("a {kind} in the post has no link")
            })?;
            Ok(Slide { url, is_video })
        })
        .collect::<Result<Vec<_>>>()?;
    if slides.is_empty() {
        bail!("the post has no pictures");
    }
    Ok(slides)
}

/// The link of the largest of `versions` (one picture or video at several
/// sizes), by pixels. Width alone isn't enough: a picture's versions include
/// square crops for the profile grid, as wide as the original.
fn largest(versions: Option<&Value>) -> Option<String> {
    versions?
        .as_array()?
        .iter()
        .filter_map(|v| {
            let size = |key| v.get(key).and_then(Value::as_u64);
            Some((size("width")? * size("height")?, v.get("url")?.as_str()?))
        })
        // `max_by_key` keeps the last of equals; the API lists the original first.
        .rev()
        .max_by_key(|(pixels, _)| *pixels)
        .map(|(_, url)| url.to_string())
}

/// The music a post plays, from its API entry. Photo posts carry it under
/// `music_metadata`. Some list a song with no link (its rights don't allow
/// it), and those play silent, as they do on Instagram's web.
fn sound(item: &Value) -> Option<Sound> {
    let music = item.pointer("/music_metadata/music_info")?;
    let url = music
        .pointer("/music_asset_info/progressive_download_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())?
        .to_string();
    let ms = |key: &str| {
        music
            .pointer(&format!("/music_consumption_info/{key}"))
            .and_then(Value::as_u64)
    };
    let clip = match (
        ms("audio_asset_start_time_in_ms"),
        ms("overlap_duration_in_ms"),
    ) {
        (Some(start), Some(length)) if length > 0 => Some((start, length)),
        _ => None,
    };
    Some(Sound { url, clip })
}

/// The per-quality lists of videos and images. Without their links they're
/// only encoder detail, and the downloader's own choice (best quality) is
/// what we keep instead.
const FORMAT_FIELDS: [&str; 2] = ["video_versions", "image_versions2"];

/// DASH manifests (the video's, and its original sound's under
/// `clips_metadata`) are XML documents in a string, full of CDN links. Any
/// field whose name ends with this is dropped, wherever it is.
const DASH_MANIFEST: &str = "dash_manifest";

/// The item as we store it. Two thirds of a raw item (~45 KB) is links to
/// Instagram's CDN: every quality of the video, covers, avatars, the song.
/// They expire within days, and the media itself is downloaded separately at
/// the best quality, so they're dropped. Everything else is kept, ~13 KB.
fn without_media_links(media: &Value) -> Value {
    let mut media = media.clone();
    if let Some(fields) = media.as_object_mut() {
        for key in FORMAT_FIELDS {
            fields.remove(key);
        }
    }
    strip_cdn_urls(&mut media);
    media
}

/// Remove every CDN URL and DASH manifest inside `value`, along with any
/// object or array that removing them leaves empty. Matching on the host
/// rather than a list of fields also catches media fields Instagram adds later.
fn strip_cdn_urls(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, v| !key.ends_with(DASH_MANIFEST) && keep_after_strip(v));
        }
        Value::Array(items) => items.retain_mut(keep_after_strip),
        _ => {}
    }
}

fn keep_after_strip(value: &mut Value) -> bool {
    if value.as_str().is_some_and(is_cdn_url) {
        return false;
    }
    let was_empty = is_empty_container(value);
    strip_cdn_urls(value);
    // Containers that arrived empty carry meaning (no tagged users, say);
    // only drop the ones that held nothing but links.
    was_empty || !is_empty_container(value)
}

fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// A link to Instagram's media CDN (`*.cdninstagram.com`, or Facebook's
/// `*.fbcdn.net`, which also serves it). Links to posts and profiles on
/// instagram.com are permanent and kept.
fn is_cdn_url(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    else {
        return false;
    };
    // A URL followed by more words is text that starts with a link.
    if s.contains(char::is_whitespace) {
        return false;
    }
    let host = rest
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    ["cdninstagram.com", "fbcdn.net"]
        .iter()
        .any(|cdn| host == *cdn || host.ends_with(&format!(".{cdn}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"{
        "num_results": 5, "more_available": true, "next_max_id": "QVFC==", "status": "ok",
        "items": [
            {"media": {"code": "Dc343v6yWwz", "media_type": 2, "product_type": "clips",
                       "taken_at": 1788541773, "video_duration": 17.996,
                       "caption": {"text": "a cat"},
                       "user": {"pk": "29071464037", "username": "catperson"}}},
            {"media": {"code": "DbPhoto", "media_type": 1, "product_type": "feed"}},
            {"media": {"code": "DbFeed", "media_type": 2, "product_type": "feed",
                       "caption": null, "user": {"pk": 7, "username": "feedvid"}}},
            {"media": {"code": "DbGuide", "media_type": 11}},
            {"media": {"media_type": 2}}
        ]
    }"#;

    #[test]
    fn parses_a_page_of_saved_posts() {
        let page = parse_page(200, PAGE).unwrap();
        assert_eq!(page.next_max_id.as_deref(), Some("QVFC=="));
        // The guide and the item without a code are skipped.
        assert_eq!(page.posts.len(), 3);

        let reel = &page.posts[0];
        assert_eq!(reel.code, "Dc343v6yWwz");
        assert_eq!(reel.caption, "a cat");
        assert_eq!(reel.author_id.as_deref(), Some("29071464037"));
        assert_eq!(reel.author_handle.as_deref(), Some("catperson"));
        assert_eq!(reel.duration_seconds, Some(18));
        assert_eq!(reel.posted_at, Some(1_788_541_773));
        assert!(!reel.is_photo);
        assert_eq!(reel.url(), "https://www.instagram.com/reel/Dc343v6yWwz/");

        let photo = &page.posts[1];
        assert!(photo.is_photo);
        assert_eq!(photo.duration_seconds, None);
        assert_eq!(photo.url(), "https://www.instagram.com/p/DbPhoto/");

        let feed = &page.posts[2];
        assert_eq!(feed.caption, "");
        assert_eq!(feed.author_id.as_deref(), Some("7"));
        assert_eq!(feed.duration_seconds, None);
        assert_eq!(feed.url(), "https://www.instagram.com/p/DbFeed/");
    }

    #[test]
    fn parses_a_page_of_liked_posts() {
        // Unwrapped items, and a numeric cursor.
        let body = r#"{
            "num_results": 2, "more_available": true, "next_max_id": 4007916203002916272, "status": "ok",
            "items": [
                {"code": "DecKsrPFC18", "media_type": 8, "product_type": "carousel_container",
                 "carousel_media": [{"media_type": 1}, {"media_type": 2}]},
                {"code": "DeeokNKCCBQ", "media_type": 2, "product_type": "clips"}
            ]
        }"#;
        let page = parse_page(200, body).unwrap();
        assert_eq!(page.next_max_id.as_deref(), Some("4007916203002916272"));
        let codes: Vec<(&str, bool)> = page
            .posts
            .iter()
            .map(|p| (p.code.as_str(), p.is_photo))
            .collect();
        assert_eq!(codes, [("DecKsrPFC18", true), ("DeeokNKCCBQ", false)]);
    }

    #[test]
    fn last_page_has_no_cursor() {
        let body = r#"{"status": "ok", "more_available": false, "next_max_id": "x", "items": []}"#;
        let page = parse_page(200, body).unwrap();
        assert!(page.next_max_id.is_none());
        assert!(page.posts.is_empty());
    }

    #[test]
    fn slowing_down_is_told_apart_through_context() {
        let blocked = parse_page(400, r#"{"message": "feedback_required", "status": "fail"}"#)
            .unwrap_err()
            .context("liked list page at 1")
            .context("reading the liked list");
        assert!(is_throttled(&blocked));
        let limited = parse_page(429, "")
            .unwrap_err()
            .context("saved list page at 1");
        assert!(is_throttled(&limited));
        let agent = parse_page(400, r#"{"message": "useragent mismatch"}"#);
        assert!(!is_throttled(&agent.unwrap_err()));
        let login = parse_page(401, r#"{"message": "login_required"}"#);
        assert!(!is_throttled(&login.unwrap_err()));
        assert!(!is_throttled(&parse_page(500, "").unwrap_err()));
    }

    #[test]
    fn refusals_are_final() {
        let login = parse_page(401, r#"{"message": "login_required", "status": "fail"}"#);
        assert!(is_final(&login.unwrap_err()));
        let redirected = parse_page(200, r#"{"message": "login_required", "status": "fail"}"#);
        assert!(is_final(&redirected.unwrap_err()));
        let limited = parse_page(429, r#"{"message": "Please wait a few minutes"}"#);
        assert!(is_final(&limited.unwrap_err()));
        let agent = parse_page(
            400,
            r#"{"message": "useragent mismatch", "status": "fail"}"#,
        );
        assert!(is_final(&agent.unwrap_err()));
        let blocked = parse_page(400, r#"{"message": "feedback_required", "status": "fail"}"#);
        assert!(is_final(&blocked.unwrap_err()));
        let checkpoint = parse_page(400, r#"{"message": "checkpoint_required"}"#).unwrap_err();
        assert_eq!(refusal(&checkpoint), Some(Refusal::Checkpoint));
        let lapsed = parse_page(401, r#"{"message": "login_required"}"#).unwrap_err();
        assert_eq!(
            refusal(&lapsed.context("reading")),
            Some(Refusal::LoggedOut)
        );
        assert!(format!("{checkpoint}").contains("confirm it's you"));
        // Worth another try.
        assert!(!is_final(&parse_page(500, "").unwrap_err()));
        assert!(!is_final(&parse_page(200, "<html>").unwrap_err()));
        assert!(!is_final(
            &parse_page(200, r#"{"status": "fail"}"#).unwrap_err()
        ));
    }

    #[test]
    fn finds_each_slide_at_its_largest() {
        // The original, a smaller copy, and a square crop as wide as it.
        let photo = serde_json::json!({"image_versions2": {"candidates": [
            {"width": 1080, "height": 1440, "url": "https://cdn/original.jpg"},
            {"width": 720, "height": 960, "url": "https://cdn/720.jpg"},
            {"width": 1080, "height": 1080, "url": "https://cdn/square.jpg"},
        ]}});
        assert_eq!(
            slides(&photo).unwrap(),
            [Slide {
                url: "https://cdn/original.jpg".into(),
                is_video: false
            }]
        );

        let carousel = serde_json::json!({"carousel_media": [
            {"media_type": 1, "image_versions2": {"candidates": [{"width": 1080, "height": 1350, "url": "https://cdn/a.jpg"}]}},
            {"media_type": 2,
             "image_versions2": {"candidates": [{"width": 1080, "height": 1920, "url": "https://cdn/cover.jpg"}]},
             "video_versions": [{"width": 480, "height": 854, "url": "https://cdn/480.mp4"},
                                {"width": 720, "height": 1280, "url": "https://cdn/720.mp4"}]},
        ]});
        assert_eq!(
            slides(&carousel).unwrap(),
            [
                Slide {
                    url: "https://cdn/a.jpg".into(),
                    is_video: false
                },
                Slide {
                    url: "https://cdn/720.mp4".into(),
                    is_video: true
                },
            ]
        );

        assert!(slides(&serde_json::json!({"carousel_media": []})).is_err());
        assert!(slides(&serde_json::json!({"image_versions2": {"candidates": []}})).is_err());
    }

    #[test]
    fn finds_the_music_and_the_part_the_post_plays() {
        let item = serde_json::json!({"music_metadata": {"music_info": {
            "music_asset_info": {"progressive_download_url": "https://cdn/song.m4a", "duration_in_ms": 193_557},
            "music_consumption_info": {"audio_asset_start_time_in_ms": 3000, "overlap_duration_in_ms": 30000},
        }}});
        assert_eq!(
            sound(&item),
            Some(Sound {
                url: "https://cdn/song.m4a".into(),
                clip: Some((3000, 30000))
            })
        );
        // A song whose rights don't allow playing it here, and no song.
        let no_link = serde_json::json!({"music_metadata": {"music_info": {
            "music_asset_info": {"progressive_download_url": null}}}});
        assert_eq!(sound(&no_link), None);
        let silent = serde_json::json!({"music_metadata": {"music_info": null}});
        assert_eq!(sound(&silent), None);
    }

    #[test]
    fn page_scripts_escape_their_values() {
        assert!(saved_page_request(None).contains("fetch('/api/v1/feed/saved/posts/' + ''"));
        let js = saved_page_request(Some("ab'c\"d"));
        assert!(js.contains(r#"new URLSearchParams({max_id: "ab'c\"d"})"#));
        assert!(post_in_page("a'b").contains(r#"const code = "a'b";"#));
    }

    #[test]
    fn strips_media_links_and_keeps_the_rest() {
        let media = serde_json::json!({
            "code": "abc",
            "usertags": {"in": []},
            "video_versions": [{"width": 720, "url": "https://scontent.cdninstagram.com/v.mp4"}],
            "image_versions2": {"candidates": [{"width": 720}]},
            "video_dash_manifest": "<MPD>https://scontent.cdninstagram.com/a</MPD>",
            "caption": {"text": "see https://example.com"},
            "user": {
                "username": "a",
                "profile_pic_url": "https://scontent-mia5-1.cdninstagram.com/p.jpg",
                "hd_profile_pic_versions": [{"url": "https://instagram.fmia1-1.fna.fbcdn.net/p.jpg"}],
            },
            "clips_metadata": {
                "music_info": {"music_asset_info": {
                    "title": "song",
                    "progressive_download_url": "https://video.xx.fbcdn.net/s.m4a",
                }},
                "original_sound_info": {"dash_manifest": "<MPD/>", "duration_in_ms": 5},
            },
        });
        assert_eq!(
            without_media_links(&media),
            serde_json::json!({
                "code": "abc",
                "usertags": {"in": []},
                "caption": {"text": "see https://example.com"},
                "user": {"username": "a"},
                "clips_metadata": {
                    "music_info": {"music_asset_info": {"title": "song"}},
                    "original_sound_info": {"duration_in_ms": 5},
                },
            })
        );
    }

    #[test]
    fn recognizes_cdn_hosts() {
        assert!(is_cdn_url("https://scontent-mia5-1.cdninstagram.com/x"));
        assert!(is_cdn_url("https://video.xx.fbcdn.net/x"));
        assert!(is_cdn_url("http://CDNINSTAGRAM.com:443/x"));
        assert!(!is_cdn_url("https://www.instagram.com/reel/abc/"));
        assert!(!is_cdn_url("https://notcdninstagram.com/x"));
        assert!(!is_cdn_url("https://fbcdn.net.evil.example/x"));
        assert!(!is_cdn_url("cdninstagram.com/x"));
    }
}
