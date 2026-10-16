//! Lists a user's liked TikTok videos. TikTok's web API wants every request
//! signed by its page script, and answers an unsigned one with an empty
//! 200 response (seen 2026-10-12). So we open tiktok.com in a headless browser (`headless.rs`)
//! logged in with the user's cookies, and call the API with `fetch` from inside
//! the page, where TikTok's script signs it. yt-dlp has no extractor for
//! liked videos, so here we parse TikTok's JSON ourselves.
//!
//! The tests can't show this works against TikTok. To try it:
//! `cargo run -- likes --platform tiktok --limit 5` (no database), or
//! `cargo run -- sync --platform tiktok --db <scratch>.db`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;

use crate::browser::{Browser, Cookie, LoggedOut};
use crate::headless::Headless;

/// Any logged-in page works. This one is light and has no side effects,
/// unlike, say, the inbox, which marks messages read.
const START_PAGE: &str = "https://www.tiktok.com/setting";

/// The page is usable once TikTok's request-signing script (`byted_acrawler`)
/// has loaded and the server-rendered app state is present.
const PAGE_READY: &str =
    "window.byted_acrawler && document.getElementById('__UNIVERSAL_DATA_FOR_REHYDRATION__')";

/// The logged-in user's `secUid`, or null when nobody is logged in.
const SEC_UID: &str = "JSON.parse(document.getElementById('__UNIVERSAL_DATA_FOR_REHYDRATION__')\
     .textContent).__DEFAULT_SCOPE__?.['webapp.app-context']?.user?.secUid ?? null";

/// What the web app asks for. Pages often come back a few videos short of
/// it, which doesn't mean the list has ended; `hasMore` says that. A full
/// read ended after exactly 200 pages (about 5,100 likes, back to January
/// 2026; seen 2026-10-12), which looks like a cap on TikTok's side rather
/// than the true end of the list.
const PAGE_SIZE: usize = 30;

/// Now and then TikTok rejects a page (status 100001, or an empty body) that
/// it serves fine a moment later, so each page gets a few tries.
const PAGE_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone)]
pub struct Video {
    pub id: String,
    pub desc: String,
    /// The author's numeric id. Unlike the handle, it never changes.
    pub author_id: Option<String>,
    /// The author's @handle, without the @.
    pub author_handle: Option<String>,
    /// None for photo posts, which TikTok gives a duration of 0.
    pub duration_seconds: Option<u64>,
    /// A slideshow of images rather than a video.
    pub is_photo: bool,
    /// When it was posted, in unix seconds.
    pub posted_at: Option<i64>,
    /// The item from TikTok's API minus its media links (see
    /// `without_media_links`), kept so fields we don't parse yet aren't lost.
    pub raw: Value,
}

impl Video {
    pub fn url(&self) -> String {
        // TikTok redirects a wrong handle to the right one, but needs one there.
        let handle = self.author_handle.as_deref().unwrap_or("_");
        let kind = if self.is_photo { "photo" } else { "video" };
        format!("https://www.tiktok.com/@{handle}/{kind}/{}", self.id)
    }
}

/// The server-rendered data on a post's page has the full item, signed media
/// URLs included. Null when the page has no post (deleted or private).
const POST_ITEM: &str = "JSON.parse(document.getElementById('__UNIVERSAL_DATA_FOR_REHYDRATION__')\
     .textContent).__DEFAULT_SCOPE__?.['webapp.video-detail']?.itemInfo?.itemStruct ?? null";

/// A photo post (slideshow) with its media, fetched through TikTok's site.
pub struct PhotoPost {
    /// The item, minus its media links, as `listing_json` stores it.
    pub item: Value,
    /// The images in order, at the size they were posted.
    pub images: Vec<Fetched>,
    /// The sound that plays over the slideshow.
    pub sound: Option<Fetched>,
}

/// A file fetched inside the page.
pub struct Fetched {
    pub bytes: Vec<u8>,
    /// The Content-Type it was served with.
    pub mime: String,
}

/// One page of the liked list.
#[derive(Debug)]
struct Page {
    videos: Vec<Video>,
    /// Where the next page starts: the like time, in ms, of this page's last video.
    cursor: String,
    has_more: bool,
}

/// List liked videos page by page, newest like first, starting at `cursor`
/// (None for the start of the list). `on_page` gets each page's videos and
/// the cursor for the page after it (None after the last page), and
/// returns whether to go on, so callers can store each page as it comes and
/// quit once they reach likes they already have.
///
/// A full listing is about one request per 30 likes; 5,000 likes take two
/// minutes.
pub async fn liked_videos(
    browser: Browser,
    cookies: &[Cookie],
    cursor: Option<&str>,
    mut on_page: impl FnMut(Vec<Video>, Option<&str>) -> Result<bool>,
) -> Result<()> {
    // Checked up front so a user who doesn't use TikTok costs nothing: no
    // browser launch, and a clear message instead of a page that never loads.
    if !cookies.iter().any(|c| c.name == "sessionid") {
        return Err(LoggedOut("TikTok").into());
    }
    let mut tab = Headless::launch(browser, cookies).await?;
    let result = list_liked(&mut tab, cursor, &mut on_page).await;
    tab.close().await;
    result
}

async fn list_liked(
    tab: &mut Headless,
    cursor: Option<&str>,
    on_page: &mut impl FnMut(Vec<Video>, Option<&str>) -> Result<bool>,
) -> Result<()> {
    tab.goto(START_PAGE, PAGE_READY).await?;
    let Value::String(sec_uid) = tab.eval(SEC_UID).await? else {
        return Err(LoggedOut("TikTok").into());
    };

    let mut cursor = cursor.unwrap_or("0").to_string();
    loop {
        let page = fetch_page(tab, &sec_uid, &cursor).await?;
        // A cursor that doesn't move would fetch the same page forever.
        let next = (page.has_more && page.cursor != cursor).then_some(page.cursor);
        if !on_page(page.videos, next.as_deref())? {
            return Ok(());
        }
        match next {
            Some(next) => cursor = next,
            None => return Ok(()),
        }
    }
}

/// Fetch a photo post's images and sound. yt-dlp only gets a slideshow's
/// sound, so this opens the post in a headless browser and fetches the rest
/// from inside the page.
pub async fn photo_post(browser: Browser, cookies: &[Cookie], url: &str) -> Result<PhotoPost> {
    if !cookies.iter().any(|c| c.name == "sessionid") {
        return Err(LoggedOut("TikTok").into());
    }
    let mut tab = Headless::launch(browser, cookies).await?;
    let result = fetch_photo_post(&mut tab, url).await;
    tab.close().await;
    result
}

async fn fetch_photo_post(tab: &mut Headless, url: &str) -> Result<PhotoPost> {
    // A post's /photo/ page renders in the browser and carries no item data;
    // the same post's /video/ page has it server-rendered (it then switches
    // the address to /photo/ itself).
    let page = url.replacen("/photo/", "/video/", 1);
    tab.goto(
        &page,
        "document.getElementById('__UNIVERSAL_DATA_FOR_REHYDRATION__')",
    )
    .await?;
    let item = tab.eval(POST_ITEM).await?;
    if item.is_null() {
        bail!("TikTok's page for {url} has no post (deleted or private?)");
    }

    let mut images = Vec::new();
    for image in item
        .pointer("/imagePost/images")
        .and_then(Value::as_array)
        .context("the post has no images")?
    {
        let urls = url_list(image.pointer("/imageURL/urlList"));
        images.push(fetch_in_page(tab, &urls).await?);
    }
    if images.is_empty() {
        bail!("the post has no images");
    }
    let sound_urls = url_list(item.pointer("/music/playUrl"));
    let sound = if sound_urls.is_empty() {
        None
    } else {
        Some(fetch_in_page(tab, &sound_urls).await?)
    };
    Ok(PhotoPost {
        item: without_media_links(&item),
        images,
        sound,
    })
}

/// The URLs in a TikTok `urlList` array, or a lone URL string.
fn url_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(url)) if !url.is_empty() => vec![url.clone()],
        Some(Value::Array(urls)) => urls
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Fetch the first of `urls` (mirrors of one file) that works, from inside
/// the page, and carry it out base64-encoded. One file per call keeps each
/// DevTools message to a few megabytes.
async fn fetch_in_page(tab: &mut Headless, urls: &[String]) -> Result<Fetched> {
    let urls_json = serde_json::to_string(urls)?;
    let script = format!(
        "(async () => {{ \
           let last = 'no URLs'; \
           for (const u of {urls_json}) {{ \
             try {{ \
               const r = await fetch(u); \
               if (!r.ok) {{ last = 'HTTP ' + r.status; continue; }} \
               const b = new Uint8Array(await r.arrayBuffer()); \
               let s = ''; \
               for (let i = 0; i < b.length; i += 0x8000) s += String.fromCharCode(...b.subarray(i, i + 0x8000)); \
               return {{mime: r.headers.get('content-type') || '', data: btoa(s)}}; \
             }} catch (e) {{ last = String(e); }} \
           }} \
           throw new Error('download failed: ' + last); \
         }})()"
    );
    let got = tab.eval(&script).await?;
    let data = got["data"]
        .as_str()
        .context("page fetch returned no data")?;
    Ok(Fetched {
        bytes: base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("page fetch returned bad base64")?,
        mime: got["mime"].as_str().unwrap_or("").to_string(),
    })
}

/// Fetch the page of the liked list that starts at `cursor`, retrying
/// TikTok's occasional rejections.
async fn fetch_page(tab: &mut Headless, sec_uid: &str, cursor: &str) -> Result<Page> {
    let mut attempt = 1;
    loop {
        let result = async {
            let body = tab.eval(&page_request(sec_uid, cursor)).await?;
            match body.as_str() {
                Some("") => bail!("TikTok sent an empty response"),
                Some(body) => parse_page(body),
                None => bail!("the liked list fetch returned no text"),
            }
        }
        .await;
        match result {
            Ok(page) => return Ok(page),
            Err(e) if attempt < PAGE_ATTEMPTS => {
                eprintln!("tiktok: liked list page failed ({e:#}); retrying");
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
                attempt += 1;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("liked list page at cursor {cursor}"));
            }
        }
    }
}

/// The page script that fetches one page of the liked list and returns the
/// response body as text. This is the endpoint the profile's Liked tab uses
/// (TikTok's own name for a like is "favorite").
fn page_request(sec_uid: &str, cursor: &str) -> String {
    // serde_json quotes and escapes the values for use as JS string literals.
    let sec_uid = Value::from(sec_uid);
    let cursor = Value::from(cursor);
    // Without app_name and device_platform, TikTok serves about 20 pages and
    // then fails with status 100001.
    format!(
        "(async () => {{ \
           const q = new URLSearchParams({{aid: '1988', app_name: 'tiktok_web', device_platform: 'web_pc', \
             count: '{PAGE_SIZE}', cursor: {cursor}, secUid: {sec_uid}}}); \
           const r = await fetch('/api/favorite/item_list/?' + q, {{credentials: 'include'}}); \
           return await r.text(); \
         }})()"
    )
}

fn parse_page(body: &str) -> Result<Page> {
    let page: Value = serde_json::from_str(body).context("liked list response isn't JSON")?;
    // Responses carry the status twice, under both spellings.
    let status = ["statusCode", "status_code"]
        .iter()
        .find_map(|key| page.get(key).and_then(Value::as_i64))
        .unwrap_or(0);
    if status != 0 {
        let message = page.get("status_msg").and_then(Value::as_str).unwrap_or("");
        bail!("TikTok answered status {status}: {message}");
    }
    let videos = page
        .get("itemList")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_video).collect())
        .unwrap_or_default();
    // TikTok sends the cursor as a string; take a number too.
    let cursor = match page.get("cursor") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    Ok(Page {
        videos,
        cursor,
        has_more: page
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_video(item: &Value) -> Option<Video> {
    let text = |path: &str| {
        item.pointer(path)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Video {
        id: text("/id")?,
        desc: text("/desc").unwrap_or_default(),
        author_id: text("/author/id"),
        author_handle: text("/author/uniqueId"),
        duration_seconds: item
            .pointer("/video/duration")
            .and_then(Value::as_u64)
            .filter(|&d| d > 0),
        is_photo: item.get("imagePost").is_some(),
        // A number today; some TikTok endpoints send it as a string.
        posted_at: item.get("createTime").and_then(|t| {
            t.as_i64()
                .or_else(|| t.as_str().and_then(|s| s.parse().ok()))
        }),
        raw: without_media_links(item),
    })
}

/// Per-quality stream lists under `video`. Once their URLs are gone they're
/// still ~5 KB of encoder detail per item, and the downloader's own format
/// choice (best quality) is what we keep instead.
const FORMAT_LISTS: [&str; 2] = ["bitrateInfo", "bitrateAudioInfo"];

/// The item as we store it. About three quarters of a raw item (~23 KB) is
/// links to TikTok's CDN: every quality of the video, several sizes of each
/// cover and avatar, caption files. They expire within about two days (seen
/// 2026-10-12), and
/// the media itself is downloaded separately at the best quality, so they're
/// dropped. Everything else is kept, which leaves ~5 KB.
fn without_media_links(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(video) = item.get_mut("video").and_then(Value::as_object_mut) {
        for key in FORMAT_LISTS {
            video.remove(key);
        }
    }
    strip_tiktok_urls(&mut item);
    item
}

/// Remove every TikTok-hosted URL inside `value`, along with any object or
/// array that removing them leaves empty. Matching on the host rather than a
/// list of fields also catches media fields TikTok adds later.
fn strip_tiktok_urls(value: &mut Value) {
    match value {
        Value::Object(fields) => fields.retain(|_, v| keep_after_strip(v)),
        Value::Array(items) => items.retain_mut(keep_after_strip),
        _ => {}
    }
}

fn keep_after_strip(value: &mut Value) -> bool {
    if value.as_str().is_some_and(is_tiktok_url) {
        return false;
    }
    let was_empty = is_empty_container(value);
    strip_tiktok_urls(value);
    // Containers that arrived empty carry meaning (no hashtags, say); only
    // drop the ones that held nothing but links.
    was_empty || !is_empty_container(value)
}

fn is_empty_container(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// A link on tiktok.com or TikTok's CDN. Inside an item these are all media
/// links; permanent ones (a video's page) can be rebuilt from its ids.
/// Third-party links, like an editing app's anchor, aren't matched.
fn is_tiktok_url(s: &str) -> bool {
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
    host == "tiktok.com" || host.ends_with(".tiktok.com") || host.contains("tiktokcdn")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built by hand in the shape of `/api/favorite/item_list/`'s answer: only
    // the fields the parser reads, with made-up values. A live read
    // (`likes --platform tiktok`) went through the same parser on 2026-10-16.
    const PAGE: &str = r#"{
        "statusCode": 0, "cursor": "1791665073097", "hasMore": true,
        "itemList": [
            {"id": "111", "desc": "a cat", "createTime": 1700000000,
             "author": {"id": "9", "uniqueId": "catperson"},
             "video": {"duration": 12}},
            {"id": "222", "desc": "", "author": {"id": "8", "uniqueId": "slides"},
             "video": {"duration": 0}, "imagePost": {"images": []}},
            {"desc": "no id, skipped"}
        ]
    }"#;

    #[test]
    fn parses_a_page() {
        let page = parse_page(PAGE).unwrap();
        assert_eq!(page.cursor, "1791665073097");
        assert!(page.has_more);
        assert_eq!(page.videos.len(), 2);

        let video = &page.videos[0];
        assert_eq!(video.id, "111");
        assert_eq!(video.desc, "a cat");
        assert_eq!(video.author_id.as_deref(), Some("9"));
        assert_eq!(video.duration_seconds, Some(12));
        assert_eq!(video.posted_at, Some(1_700_000_000));
        assert_eq!(video.url(), "https://www.tiktok.com/@catperson/video/111");
    }

    #[test]
    fn photo_posts_have_no_duration() {
        let page = parse_page(PAGE).unwrap();
        let photo = &page.videos[1];
        assert!(photo.is_photo);
        assert_eq!(photo.duration_seconds, None);
        assert_eq!(photo.url(), "https://www.tiktok.com/@slides/photo/222");
    }

    #[test]
    fn last_page_and_numeric_cursor() {
        let page = parse_page(r#"{"cursor": 17, "hasMore": false, "itemList": []}"#).unwrap();
        assert_eq!(page.cursor, "17");
        assert!(!page.has_more);
        assert!(page.videos.is_empty());
    }

    #[test]
    fn error_status_fails() {
        let err = parse_page(r#"{"statusCode": 10201, "status_msg": "nope"}"#).unwrap_err();
        assert!(err.to_string().contains("10201"));
        assert!(parse_page(r#"{"status_code": 5, "itemList": []}"#).is_err());
    }

    #[test]
    fn strips_media_links_and_keeps_the_rest() {
        let item = serde_json::json!({
            "id": "1",
            "desc": "see https://example.com and https://www.tiktok.com/@a",
            "textExtra": [],
            "author": {
                "uniqueId": "a",
                "avatarLarger": "https://p16-sign.tiktokcdn-us.com/a.jpeg?x-expires=1",
            },
            "music": {
                "title": "original sound",
                "playUrl": "https://v16m.tiktokcdn-us.com/abc/6acd/video/x",
                "coverLarge": "https://p16-common.tiktokcdn-us.com/c.jpeg",
            },
            "video": {
                "width": 720,
                "bitrateInfo": [{"Bitrate": 1, "PlayAddr": {}}],
                "bitrateAudioInfo": [],
                "playAddr": "https://v16-webapp-prime.us.tiktok.com/video/?expire=1",
                "zoomCover": {"240": "https://p16-sign.tiktokcdn-us.com/z.jpeg"},
            },
            "anchors": [{
                "schema": "https://www.capcut.com/editor",
                "icon": {"urlList": ["https://p19-common-sign.tiktokcdn-us.com/i.png"]},
            }],
        });
        assert_eq!(
            without_media_links(&item),
            serde_json::json!({
                "id": "1",
                "desc": "see https://example.com and https://www.tiktok.com/@a",
                "textExtra": [],
                "author": {"uniqueId": "a"},
                "music": {"title": "original sound"},
                "video": {"width": 720},
                "anchors": [{"schema": "https://www.capcut.com/editor"}],
            })
        );
    }

    #[test]
    fn reads_url_lists() {
        let list = serde_json::json!(["https://a", 3, "https://b"]);
        assert_eq!(url_list(Some(&list)), ["https://a", "https://b"]);
        assert_eq!(
            url_list(Some(&serde_json::json!("https://c"))),
            ["https://c"]
        );
        assert_eq!(url_list(Some(&serde_json::json!(""))), Vec::<String>::new());
        assert_eq!(url_list(None), Vec::<String>::new());
    }

    #[test]
    fn recognizes_tiktok_hosts() {
        assert!(is_tiktok_url("https://tiktok.com/x"));
        assert!(is_tiktok_url("http://WWW.TikTok.com:443/x"));
        assert!(is_tiktok_url("https://p16-sign-va.tiktokcdn.com/x"));
        assert!(!is_tiktok_url("https://nottiktok.com/x"));
        assert!(!is_tiktok_url("https://tiktok.com.evil.example/x"));
        assert!(!is_tiktok_url("tiktok.com/x"));
    }

    #[test]
    fn page_request_escapes_values() {
        let js = page_request("ab'c\"d", "0");
        assert!(js.contains(r#"secUid: "ab'c\"d""#));
        assert!(js.contains(r#"cursor: "0""#));
    }
}
