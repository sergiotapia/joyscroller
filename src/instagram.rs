//! Lists the videos a user has saved on Instagram (Instagram's "favorite":
//! the bookmark under a post). The web app's own endpoint for the Saved tab
//! needs nothing but the login cookies and a couple of headers, but it's
//! called with `fetch` from inside an instagram.com tab (`headless.rs`)
//! anyway, so the request comes from a real browser with the user's cookies,
//! like the web app's. yt-dlp has no extractor for saved posts, so here we
//! parse Instagram's JSON ourselves; it downloads each video afterwards.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::browser::{Browser, Cookie};
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

/// Instagram is quick to answer bursts of requests with "Please wait a few
/// minutes". The web app loads a page per scroll; this keeps us near that.
const PAGE_PAUSE: Duration = Duration::from_secs(1);

/// Now and then a page fails (a 5xx, a dropped connection) that works a
/// moment later, so each page gets a few tries.
const PAGE_ATTEMPTS: u32 = 4;

/// Instagram's `media_type` for a video. Photos are 1 and carousels 8.
const VIDEO: u64 = 2;

#[derive(Debug, Clone)]
pub struct Video {
    /// The post's shortcode, as in its URL (`instagram.com/reel/<code>/`).
    pub code: String,
    /// The caption, often empty.
    pub caption: String,
    /// The author's numeric id. Unlike the handle, it never changes.
    pub author_id: Option<String>,
    /// The author's username, without the @.
    pub author_handle: Option<String>,
    pub duration_seconds: Option<u64>,
    /// A reel, rather than a video posted to the feed. Only the URL differs.
    pub is_reel: bool,
    /// When it was posted, in unix seconds.
    pub posted_at: Option<i64>,
    /// The media item from Instagram's API minus its media links (see
    /// `without_media_links`), kept so fields we don't parse yet aren't lost.
    pub raw: Value,
}

impl Video {
    pub fn url(&self) -> String {
        let kind = if self.is_reel { "reel" } else { "p" };
        format!("https://www.instagram.com/{kind}/{}/", self.code)
    }
}

/// One page of the saved list.
#[derive(Debug)]
struct Page {
    videos: Vec<Video>,
    /// Where the next page starts, when there is one.
    next_max_id: Option<String>,
}

/// List saved videos, newest save first. `limit` of 0 means all of them.
/// Saved photos and carousels are skipped. `keep_going` sees each video as it
/// arrives; returning false stops the listing there (that video is still
/// included), so callers can quit once they reach saves they already have.
pub async fn saved_videos(
    browser: Browser,
    cookies: &[Cookie],
    limit: usize,
    mut keep_going: impl FnMut(&Video) -> bool,
) -> Result<Vec<Video>> {
    // Checked up front so a user who doesn't use Instagram costs nothing: no
    // browser launch, and a clear message instead of a login error.
    if !cookies.iter().any(|c| c.name == "sessionid") {
        bail!("not logged in to Instagram in this browser profile");
    }
    let mut tab = Headless::launch(browser, cookies).await?;
    let result = list_saved(&mut tab, limit, &mut keep_going).await;
    tab.close().await;
    result
}

async fn list_saved(
    tab: &mut Headless,
    limit: usize,
    keep_going: &mut impl FnMut(&Video) -> bool,
) -> Result<Vec<Video>> {
    tab.goto(START_PAGE, PAGE_READY).await?;
    let mut videos = Vec::new();
    let mut max_id: Option<String> = None;
    loop {
        let page = fetch_page(tab, max_id.as_deref()).await?;
        for video in page.videos {
            let go_on = keep_going(&video);
            videos.push(video);
            if !go_on || videos.len() == limit {
                return Ok(videos);
            }
        }
        // A cursor that doesn't move would fetch the same page forever.
        match page.next_max_id {
            Some(next) if max_id.as_ref() != Some(&next) => max_id = Some(next),
            _ => return Ok(videos),
        }
        tokio::time::sleep(PAGE_PAUSE).await;
    }
}

/// Fetch the page of the saved list that starts at `max_id` (the first page
/// when None), retrying failures that may be passing.
async fn fetch_page(tab: &mut Headless, max_id: Option<&str>) -> Result<Page> {
    let mut attempt = 1;
    loop {
        let result = async {
            let got = tab.eval(&page_request(max_id)).await?;
            let status = got["status"].as_u64().unwrap_or(0);
            let body = got["body"].as_str().unwrap_or("");
            parse_page(status, body)
        }
        .await;
        match result {
            Ok(page) => return Ok(page),
            Err(e) if attempt < PAGE_ATTEMPTS && !is_final(&e) => {
                eprintln!("instagram: saved list page failed ({e:#}); retrying");
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
                attempt += 1;
            }
            Err(e) => {
                let at = max_id.unwrap_or("the start");
                return Err(e).with_context(|| format!("saved list page at {at}"));
            }
        }
    }
}

/// Errors a retry won't fix, which retrying would only make worse.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn is_final(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>().is_some()
}

/// The page script that fetches one page of the saved list and returns its
/// HTTP status and body. This is the endpoint the profile's Saved tab uses.
fn page_request(max_id: Option<&str>) -> String {
    // serde_json quotes and escapes the value for use as a JS string literal.
    let query = match max_id {
        Some(max_id) => format!(
            "'?' + new URLSearchParams({{max_id: {}}})",
            Value::from(max_id)
        ),
        None => "''".to_string(),
    };
    // The API checks the CSRF header against the csrftoken cookie, as for
    // any request from the web app.
    format!(
        "(async () => {{ \
           const csrf = (document.cookie.match(/(?:^|; )csrftoken=([^;]*)/) || [])[1] || ''; \
           const r = await fetch('/api/v1/feed/saved/posts/' + {query}, {{ \
             credentials: 'include', \
             headers: {{'x-ig-app-id': '{WEB_APP_ID}', 'x-csrftoken': csrf, 'x-requested-with': 'XMLHttpRequest'}} \
           }}); \
           return {{status: r.status, body: await r.text()}}; \
         }})()"
    )
}

fn parse_page(status: u64, body: &str) -> Result<Page> {
    let page: Option<Value> = serde_json::from_str(body).ok();
    let message = page
        .as_ref()
        .and_then(|p| p.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A lapsed login comes back as a 401 or 403 (`login_required`), or as a
    // redirect to the login page, which fetch follows to an HTML page.
    if matches!(status, 401 | 403) || message == "login_required" {
        return Err(Refused(format!(
            "Instagram wants a login (HTTP {status}{}); log in to Instagram in the browser",
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        ))
        .into());
    }
    // "Please wait a few minutes before you try again": more requests now
    // only extend the wait.
    if status == 429 {
        return Err(Refused(format!(
            "Instagram is rate limiting us (HTTP 429): {message}"
        ))
        .into());
    }
    if status != 200 {
        bail!("Instagram answered HTTP {status}: {message}");
    }
    let page = page.context("saved list response isn't JSON (logged out?)")?;
    if page.get("status").and_then(Value::as_str) != Some("ok") {
        bail!(
            "Instagram answered status {:?}: {message}",
            page.get("status")
        );
    }
    let videos = page
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("media"))
                .filter_map(parse_video)
                .collect()
        })
        .unwrap_or_default();
    let more = page
        .get("more_available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Page {
        videos,
        next_max_id: page
            .get("next_max_id")
            .and_then(Value::as_str)
            .filter(|id| more && !id.is_empty())
            .map(str::to_string),
    })
}

/// A saved video, or None for a photo, a carousel, or an item missing its code.
fn parse_video(media: &Value) -> Option<Video> {
    if media.get("media_type").and_then(Value::as_u64) != Some(VIDEO) {
        return None;
    }
    let text = |path: &str| {
        media
            .pointer(path)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Video {
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
        "num_results": 4, "more_available": true, "next_max_id": "QVFC==", "status": "ok",
        "items": [
            {"media": {"code": "Dc343v6yWwz", "media_type": 2, "product_type": "clips",
                       "taken_at": 1788541773, "video_duration": 17.996,
                       "caption": {"text": "a cat"},
                       "user": {"pk": "29071464037", "username": "catperson"}}},
            {"media": {"code": "DbPhoto", "media_type": 1, "product_type": "feed"}},
            {"media": {"code": "DbFeed", "media_type": 2, "product_type": "feed",
                       "caption": null, "user": {"pk": 7, "username": "feedvid"}}},
            {"media": {"media_type": 2}}
        ]
    }"#;

    #[test]
    fn parses_a_page_of_videos() {
        let page = parse_page(200, PAGE).unwrap();
        assert_eq!(page.next_max_id.as_deref(), Some("QVFC=="));
        // The photo and the item without a code are skipped.
        assert_eq!(page.videos.len(), 2);

        let reel = &page.videos[0];
        assert_eq!(reel.code, "Dc343v6yWwz");
        assert_eq!(reel.caption, "a cat");
        assert_eq!(reel.author_id.as_deref(), Some("29071464037"));
        assert_eq!(reel.author_handle.as_deref(), Some("catperson"));
        assert_eq!(reel.duration_seconds, Some(18));
        assert_eq!(reel.posted_at, Some(1_788_541_773));
        assert_eq!(reel.url(), "https://www.instagram.com/reel/Dc343v6yWwz/");

        let feed = &page.videos[1];
        assert_eq!(feed.caption, "");
        assert_eq!(feed.author_id.as_deref(), Some("7"));
        assert_eq!(feed.duration_seconds, None);
        assert_eq!(feed.url(), "https://www.instagram.com/p/DbFeed/");
    }

    #[test]
    fn last_page_has_no_cursor() {
        let body = r#"{"status": "ok", "more_available": false, "next_max_id": "x", "items": []}"#;
        let page = parse_page(200, body).unwrap();
        assert!(page.next_max_id.is_none());
        assert!(page.videos.is_empty());
    }

    #[test]
    fn login_and_rate_limit_errors_are_final() {
        let login = parse_page(401, r#"{"message": "login_required", "status": "fail"}"#);
        assert!(is_final(&login.unwrap_err()));
        let redirected = parse_page(200, r#"{"message": "login_required", "status": "fail"}"#);
        assert!(is_final(&redirected.unwrap_err()));
        let limited = parse_page(429, r#"{"message": "Please wait a few minutes"}"#);
        assert!(is_final(&limited.unwrap_err()));
        // Worth another try.
        assert!(!is_final(&parse_page(500, "").unwrap_err()));
        assert!(!is_final(&parse_page(200, "<html>").unwrap_err()));
        assert!(!is_final(
            &parse_page(200, r#"{"status": "fail"}"#).unwrap_err()
        ));
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

    #[test]
    fn page_request_escapes_the_cursor() {
        assert!(page_request(None).contains("'/api/v1/feed/saved/posts/' + ''"));
        let js = page_request(Some("ab'c\"d"));
        assert!(js.contains(r#"new URLSearchParams({max_id: "ab'c\"d"})"#));
    }
}
