//! Lists a user's liked TikTok videos. TikTok's web API wants every request
//! signed by its page script, and answers an unsigned one with an empty
//! 200 response. So we open tiktok.com in a headless browser (`headless.rs`)
//! logged in with the user's cookies, and call the API with `fetch` from inside
//! the page, where TikTok's script signs it. yt-dlp has no extractor for
//! liked videos, so here we parse TikTok's JSON ourselves.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::browser::{Browser, Cookie};
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
/// it, which doesn't mean the list has ended; `hasMore` says that.
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

/// One page of the liked list.
#[derive(Debug)]
struct Page {
    videos: Vec<Video>,
    /// Where the next page starts: the like time, in ms, of this page's last video.
    cursor: String,
    has_more: bool,
}

/// List liked videos, newest like first. `limit` of 0 means all of them.
/// `keep_going` sees each video as it arrives; returning false stops the
/// listing there (that video is still included), so callers can quit once
/// they reach likes they already have.
///
/// A full listing is about one request per 30 likes; 5,000 likes take two
/// minutes.
pub async fn liked_videos(
    browser: Browser,
    cookies: &[Cookie],
    limit: usize,
    mut keep_going: impl FnMut(&Video) -> bool,
) -> Result<Vec<Video>> {
    // Checked up front so a user who doesn't use TikTok costs nothing: no
    // browser launch, and a clear message instead of a page that never loads.
    if !cookies.iter().any(|c| c.name == "sessionid") {
        bail!("not logged in to TikTok in this browser profile");
    }
    let mut tab = Headless::launch(browser, cookies).await?;
    let result = list_liked(&mut tab, limit, &mut keep_going).await;
    tab.close().await;
    result
}

async fn list_liked(
    tab: &mut Headless,
    limit: usize,
    keep_going: &mut impl FnMut(&Video) -> bool,
) -> Result<Vec<Video>> {
    tab.goto(START_PAGE, PAGE_READY).await?;
    let Value::String(sec_uid) = tab.eval(SEC_UID).await? else {
        bail!("not logged in to TikTok in this browser profile");
    };

    let mut videos = Vec::new();
    let mut cursor = "0".to_string();
    loop {
        let page = fetch_page(tab, &sec_uid, &cursor).await?;
        for video in page.videos {
            let go_on = keep_going(&video);
            videos.push(video);
            if !go_on || videos.len() == limit {
                return Ok(videos);
            }
        }
        // A cursor that doesn't move would fetch the same page forever.
        if !page.has_more || page.cursor == cursor {
            return Ok(videos);
        }
        cursor = page.cursor;
    }
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
/// cover and avatar, caption files. They expire within about two days, and
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
