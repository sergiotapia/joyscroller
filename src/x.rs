//! Lists the posts a user has bookmarked and liked on X, keeping those with
//! pictures, videos or GIFs, their own or a quoted post's. yt-dlp has no
//! extractor for either list, so here we call X's web API (GraphQL) the way
//! its page does, from inside an x.com tab (`headless.rs`) with the user's
//! cookies, and parse the posts ourselves.
//!
//! X's own page signs each API request (`x-client-transaction-id`), but the
//! bookmark and like lists answer without it. The query ids and the long
//! list of feature flags each request carries change whenever X deploys, so
//! they're read from X's page rather than written down here: the page's own
//! request for the bookmarks (`Session::open` loads it once a run, with
//! pictures and video blocked) names the bookmark list's id and the flags,
//! and its `main.js` the like list's id and the token the web app sends.
//!
//! The media links (`pbs.twimg.com`, `video.twimg.com`) aren't signed and
//! don't expire, so unlike TikTok's and Instagram's they stay in the stored
//! post, and the download reads them from there without asking X again.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::browser::{Browser, Cookie, LoggedOut};
use crate::headless::Headless;

/// The bookmarks page. Loading it is how we learn the API's current query
/// ids and flags; it redirects to `/i/history`, which shows bookmarks and likes.
const BOOKMARKS_PAGE: &str = "https://x.com/i/bookmarks";

/// The page to make requests from once we know the API: tiny, and running
/// none of X's app, which would make requests of its own.
const START_PAGE: &str = "https://x.com/robots.txt";

/// The blank tab the browser starts on is also "complete", so check the host too.
const PAGE_READY: &str = "location.hostname === 'x.com' && document.readyState === 'complete'";

/// What the bookmarks page loads that we don't need: every picture and video.
const BLOCKED: [&str; 2] = ["*://pbs.twimg.com/*", "*://video.twimg.com/*"];

/// Where X's page is when it sends each list's requests, which it gives as
/// their referrer.
const BOOKMARKS_REFERRER: &str = "https://x.com/i/history";
const LIKES_REFERRER: &str = "https://x.com/i/history/likes";

/// Posts per page, as X's page asks for.
const PAGE_SIZE: u32 = 20;

/// The pause between pages, as the shortest and longest: a random length in
/// between. X allows 500 requests per list every 15 minutes; this is about a
/// page every few seconds, like someone scrolling.
const PAGE_PAUSE: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(4));

/// How many pages go by between the progress lines a long read logs.
const PROGRESS_EVERY: u32 = 10;

/// Now and then a page fails (a 5xx, a dropped connection) that works a
/// moment later, so each page gets a few tries.
const PAGE_ATTEMPTS: u32 = 4;

/// X's ids are snowflakes: the millisecond they were made, from this epoch,
/// shifted left 22 bits.
const SNOWFLAKE_EPOCH_MS: i64 = 1_288_834_974_657;

/// Ids from before snowflakes (November 2010) were counted up from 1, and
/// stopped short of this.
const FIRST_SNOWFLAKE: i64 = 30_000_000_000;

/// The API as X's page calls it, learned from the page (`discover`) and
/// kept for the run.
static API: Mutex<Option<Api>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq)]
struct Api {
    /// The `authorization` header the web app sends: the app's own token,
    /// the same for everyone, not the user's login.
    authorization: String,
    bookmarks: Operation,
    likes: Operation,
}

/// One GraphQL query: its id, which changes as X deploys, and the flags it's
/// sent with (JSON objects, as text).
#[derive(Clone, Debug, PartialEq)]
struct Operation {
    query_id: String,
    features: String,
    field_toggles: Option<String>,
}

/// A bookmarked or liked post with media.
#[derive(Debug, Clone)]
pub struct Post {
    /// The post's id, as in its URL (`x.com/<handle>/status/<id>`).
    pub id: String,
    /// The post's text, with its links spelled out, often empty.
    pub text: String,
    /// The author's numeric id. Unlike the handle, it never changes.
    pub author_id: Option<String>,
    /// The author's handle, without the @.
    pub author_handle: Option<String>,
    pub duration_seconds: Option<u64>,
    /// Pictures, or more than one picture or video, rather than one video.
    pub is_photo: bool,
    /// When it was posted, in unix seconds.
    pub posted_at: Option<i64>,
    /// The post as X's API gave it, kept so fields we don't parse yet aren't
    /// lost, and read again by the download for its media.
    pub raw: Value,
}

impl Post {
    pub fn url(&self) -> String {
        post_url(&self.id, self.author_handle.as_deref())
    }
}

fn post_url(id: &str, handle: Option<&str>) -> String {
    match handle {
        Some(handle) => format!("https://x.com/{handle}/status/{id}"),
        None => format!("https://x.com/i/status/{id}"),
    }
}

/// A picture or video in a post, at its best quality.
#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub url: String,
    /// A video or GIF (which X serves as a silent MP4), rather than a picture.
    pub is_video: bool,
    /// A video's still: its full size first, then as the post shows it.
    pub poster: Vec<String>,
    pub duration_ms: Option<u64>,
}

/// The post a post quotes, as the video page shows it.
#[derive(Debug, PartialEq)]
pub struct Quoted {
    pub id: String,
    pub author_handle: Option<String>,
    pub author_name: Option<String>,
    pub text: String,
}

impl Quoted {
    pub fn url(&self) -> String {
        post_url(&self.id, self.author_handle.as_deref())
    }
}

/// Checked up front so a user who doesn't use X costs nothing: no browser
/// launch or request, and a clear message instead of a login error.
pub fn check_logged_in(cookies: &[Cookie]) -> Result<()> {
    let has = |name: &str| cookies.iter().any(|c| c.name == name);
    if !has("auth_token") || !has("ct0") {
        return Err(LoggedOut("X").into());
    }
    Ok(())
}

/// Which of X's lists to read.
#[derive(Debug, Clone, Copy)]
pub enum List {
    Bookmarks,
    Likes,
}

impl List {
    fn name(self) -> &'static str {
        match self {
            List::Bookmarks => "bookmarks",
            List::Likes => "likes",
        }
    }
}

/// An x.com tab to read the lists from.
pub struct Session {
    tab: Headless,
    api: Api,
    /// The user's id, which the like list asks for.
    user_id: String,
    /// Whether `api` was learned in this session, rather than kept from an
    /// earlier one, which X may have moved on from since.
    fresh: bool,
}

impl Session {
    /// Open an x.com tab logged in with `cookies`. The first session of a
    /// run loads the bookmarks page to learn the API.
    pub async fn open(browser: Browser, cookies: &[Cookie]) -> Result<Session> {
        check_logged_in(cookies)?;
        let user_id = user_id(cookies)?;
        let mut tab = Headless::launch(browser, cookies).await?;
        let known = API.lock().ok().and_then(|api| api.clone());
        let opened = async {
            match known {
                Some(api) => {
                    tab.goto(START_PAGE, PAGE_READY).await?;
                    Ok((api, false))
                }
                None => Ok((discover(&mut tab).await?, true)),
            }
        }
        .await;
        match opened {
            Ok((api, fresh)) => Ok(Session {
                tab,
                api,
                user_id,
                fresh,
            }),
            Err(e) => {
                tab.close().await;
                Err(e)
            }
        }
    }

    pub async fn close(self) {
        self.tab.close().await;
    }

    /// List the posts with media on `list` page by page, newest first,
    /// starting at `cursor` (None for the start of the list). `on_page` gets
    /// each page's posts and the cursor for the page after it (None after the
    /// last page), and returns whether to go on, so callers can store each
    /// page as it comes and quit once they reach posts they already have.
    pub async fn posts(
        &mut self,
        list: List,
        cursor: Option<&str>,
        mut on_page: impl FnMut(Vec<Post>, Option<&str>) -> Result<bool>,
    ) -> Result<()> {
        let mut cursor = cursor.map(str::to_string);
        let mut listed = 0;
        for pages in 1_u32.. {
            // A full read of a long list takes many minutes; say how it's going.
            if pages % PROGRESS_EVERY == 0 {
                eprintln!("x: {} list, {listed} posts so far", list.name());
            }
            let page = self.fetch_page(list, cursor.as_deref()).await?;
            // A cursor that doesn't move would fetch the same page forever,
            // and a page with no posts at all is X's way of ending a list.
            let next = page
                .next
                .filter(|next| cursor.as_ref() != Some(next) && page.entries > 0);
            listed += page.posts.len();
            if !on_page(page.posts, next.as_deref())? {
                return Ok(());
            }
            match next {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
            tokio::time::sleep(crate::pipeline::between(PAGE_PAUSE.0, PAGE_PAUSE.1)).await;
        }
        Ok(())
    }

    /// Fetch the page of `list` that starts at `cursor`, retrying failures
    /// that may be passing, and learning the API again if X has moved on
    /// from the one we kept.
    async fn fetch_page(&mut self, list: List, cursor: Option<&str>) -> Result<Page> {
        let mut attempt = 1;
        loop {
            let result = async {
                let script = page_request(&self.api, list, &self.user_id, cursor);
                let got = self.tab.eval(&script).await?;
                let status = got["status"].as_u64().unwrap_or(0);
                parse_page(status, got["body"].as_str().unwrap_or(""))
            }
            .await;
            match result {
                Ok(page) => return Ok(page),
                Err(e) if is_outdated(&e) && !self.fresh => {
                    eprintln!("x: the API we knew is out of date ({e:#}); reading it again");
                    forget_api();
                    self.api = discover(&mut self.tab).await?;
                    self.fresh = true;
                }
                Err(e) if attempt < PAGE_ATTEMPTS && !is_final(&e) => {
                    eprintln!("x: {} list page failed ({e:#}); retrying", list.name());
                    tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
                    attempt += 1;
                }
                Err(e) => {
                    let at = cursor.unwrap_or("the start");
                    return Err(e).with_context(|| format!("{} list page at {at}", list.name()));
                }
            }
        }
    }
}

/// The user's id, from the `twid` cookie (`u=<id>`, URL-encoded).
fn user_id(cookies: &[Cookie]) -> Result<String> {
    let twid = cookies
        .iter()
        .find(|c| c.name == "twid")
        .map(|c| c.value.replace("%3D", "=").replace("%3d", "="))
        .ok_or(LoggedOut("X"))?;
    let id = twid
        .trim_matches('"')
        .strip_prefix("u=")
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .with_context(|| format!("X's twid cookie isn't a user id: {twid:?}"))?;
    Ok(id.to_string())
}

fn forget_api() {
    if let Ok(mut api) = API.lock() {
        *api = None;
    }
}

/// Learn the API from X's own page: load the bookmarks page, and take the
/// query id, flags and token from the request it makes for the first page,
/// and the like list's from its `main.js`. Keeps what it learns for the run.
async fn discover(tab: &mut Headless) -> Result<Api> {
    tab.watch_requests(&BLOCKED).await?;
    tab.goto(BOOKMARKS_PAGE, PAGE_READY).await?;
    let request = match tab
        .wait_for_request(|url| url.contains("/i/api/graphql/") && url.contains("/Bookmarks?"))
        .await
    {
        Ok(request) => request,
        Err(e) => {
            // A lapsed login sends the page to X's login screen instead.
            let path = tab.eval("location.pathname").await.unwrap_or(Value::Null);
            if path
                .as_str()
                .is_some_and(|p| p.contains("login") || p.starts_with("/i/flow"))
            {
                return Err(LoggedOut("X").into());
            }
            return Err(e.context("X's bookmarks page didn't ask for the bookmarks"));
        }
    };
    // X's app goes on making requests in the tab; there's no more to learn.
    tab.stop_watching();
    let authorization = request.headers["authorization"]
        .as_str()
        .or_else(|| request.headers["Authorization"].as_str())
        .filter(|auth| auth.starts_with("Bearer "))
        .context("X's page sent its request without a token")?
        .to_string();
    let found = tab.eval(&learn_api(&request.url)).await?;
    let text = |key: &str| {
        found[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .with_context(|| format!("couldn't find {key} in X's page"))
    };
    let api = Api {
        authorization,
        bookmarks: Operation {
            query_id: text("bookmarks")?,
            features: text("features")?,
            field_toggles: text("toggles").ok(),
        },
        likes: Operation {
            query_id: text("likes")?,
            features: text("likesFeatures")?,
            field_toggles: text("likesToggles").ok(),
        },
    };
    for id in [&api.bookmarks.query_id, &api.likes.query_id] {
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("unusual query id {id:?} in X's page");
        }
    }
    if let Ok(mut known) = API.lock() {
        *known = Some(api.clone());
    }
    Ok(api)
}

/// A page script that reads the API from the bookmarks request `url` and
/// from X's `main.js`, which the page has loaded (so it comes from the
/// cache). The like list is sent with the same flags as the bookmark list,
/// as X's page does, in the order `main.js` lists them.
fn learn_api(url: &str) -> String {
    let url = Value::from(url);
    format!(
        r#"(async () => {{
          const url = new URL({url});
          const out = {{
            bookmarks: url.pathname.split('/')[4],
            features: url.searchParams.get('features'),
            toggles: url.searchParams.get('fieldToggles'),
          }};
          const main = [...document.scripts].map(s => s.src).find(s => /\/main\.[^/]+\.js$/.test(s));
          if (!main) return out;
          const js = await (await fetch(main)).text();
          const op = js.match(/queryId:"([^"]+)",operationName:"Likes",operationType:"query",metadata:\{{featureSwitches:\[([^\]]*)\],fieldToggles:\[([^\]]*)\]/)
            || js.match(/queryId:"([^"]+)",operationName:"Likes"/);
          if (!op) return out;
          out.likes = op[1];
          const names = list => (list || '').split(',').map(s => s.replace(/"/g, '')).filter(Boolean);
          const flags = JSON.parse(out.features || '{{}}');
          const wanted = names(op[2]);
          out.likesFeatures = wanted.length
            ? JSON.stringify(Object.fromEntries(wanted.map(n => [n, flags[n] ?? false])))
            : out.features;
          const toggles = names(op[3]);
          if (toggles.length) out.likesToggles = JSON.stringify(Object.fromEntries(toggles.map(n => [n, false])));
          return out;
        }})()"#
    )
}

/// The page script that fetches one page of `list` and returns its HTTP
/// status and body, sent with the headers X's page sends. Values go in as
/// JSON, which is also valid JavaScript.
fn page_request(api: &Api, list: List, user_id: &str, cursor: Option<&str>) -> String {
    let cursor = cursor
        .map(|c| format!(",\"cursor\":{}", Value::from(c)))
        .unwrap_or_default();
    // In the order X's page puts them.
    let (operation, name, variables, referrer) = match list {
        List::Bookmarks => (
            &api.bookmarks,
            "Bookmarks",
            format!("{{\"count\":{PAGE_SIZE}{cursor},\"includePromotedContent\":true}}"),
            BOOKMARKS_REFERRER,
        ),
        List::Likes => (
            &api.likes,
            "Likes",
            format!(
                "{{\"userId\":{},\"count\":{PAGE_SIZE}{cursor},\"includePromotedContent\":false,\
                 \"withClientEventToken\":false,\"withBirdwatchNotes\":false,\"withVoice\":true}}",
                Value::from(user_id)
            ),
            LIKES_REFERRER,
        ),
    };
    let mut params = vec![
        ("variables", variables),
        ("features", operation.features.clone()),
    ];
    if let Some(toggles) = &operation.field_toggles {
        params.push(("fieldToggles", toggles.clone()));
    }
    let params: Vec<String> = params
        .into_iter()
        .map(|(key, value)| format!("[{}, {}]", Value::from(key), Value::from(value)))
        .collect();
    format!(
        r"(async () => {{
          const cookie = name => (document.cookie.match(new RegExp('(?:^|; )' + name + '=([^;]*)')) || [])[1] || '';
          const query = [{params}].map(([k, v]) => k + '=' + encodeURIComponent(v)).join('&');
          const r = await fetch('/i/api/graphql/' + {id} + '/{name}?' + query, {{
            credentials: 'include',
            referrer: {referrer},
            headers: {{
              'authorization': {authorization},
              'content-type': 'application/json',
              'x-csrf-token': cookie('ct0'),
              'x-twitter-active-user': 'yes',
              'x-twitter-auth-type': 'OAuth2Session',
              'x-twitter-client-language': cookie('lang') || 'en',
            }},
          }});
          return {{status: r.status, body: await r.text()}};
        }})()",
        params = params.join(", "),
        id = Value::from(operation.query_id.as_str()),
        referrer = Value::from(referrer),
        authorization = Value::from(api.authorization.as_str()),
    )
}

/// One page of a list.
#[derive(Debug)]
struct Page {
    /// The posts with media.
    posts: Vec<Post>,
    /// How many posts the page had, kept or not (text only, deleted).
    entries: usize,
    /// Where the next page starts.
    next: Option<String>,
}

/// Why X turned a request away.
#[derive(Debug)]
struct Refused {
    message: String,
    why: Refusal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// X is rate limiting us (HTTP 429); asking again only extends it.
    SlowDown,
    /// The query id or flags are out of date (HTTP 400 or 404).
    Outdated,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// Errors a retry won't fix.
fn is_final(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>().is_some() || e.downcast_ref::<LoggedOut>().is_some()
}

/// X telling us to slow down, for other modules' tests.
#[cfg(test)]
pub fn slow_down_for_tests() -> anyhow::Error {
    Refused {
        why: Refusal::SlowDown,
        message: "X is rate limiting us (HTTP 429)".into(),
    }
    .into()
}

/// Whether `e` is X telling us to slow down (HTTP 429).
pub fn is_throttled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>()
        .is_some_and(|r| r.why == Refusal::SlowDown)
}

fn is_outdated(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Refused>()
        .is_some_and(|r| r.why == Refusal::Outdated)
}

fn parse_page(status: u64, body: &str) -> Result<Page> {
    let response: Option<Value> = serde_json::from_str(body).ok();
    // X explains a failure in `errors`, sometimes alongside a 200.
    let message = response
        .as_ref()
        .and_then(|r| r.pointer("/errors/0/message"))
        .and_then(Value::as_str)
        .or_else(|| response.as_ref()?.get("message")?.as_str())
        .unwrap_or("")
        .to_string();
    let detail = if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    };
    match status {
        200 => {}
        // "Could not authenticate you": the login lapsed.
        401 => return Err(anyhow::Error::from(LoggedOut("X")).context(format!("HTTP 401{detail}"))),
        429 => {
            return Err(Refused {
                why: Refusal::SlowDown,
                message: format!("X is rate limiting us (HTTP 429{detail})"),
            }
            .into());
        }
        // "Query not found" for an id X has retired, and a 400 naming the
        // flags it now wants.
        400 | 404 => {
            return Err(Refused {
                why: Refusal::Outdated,
                message: format!("X doesn't take the request any more (HTTP {status}{detail})"),
            }
            .into());
        }
        _ => bail!("X answered HTTP {status}{detail}"),
    }
    let response = response.context("X's answer isn't JSON")?;
    let data = response
        .get("data")
        .filter(|data| !data.is_null())
        .with_context(|| format!("X answered without the list{detail}"))?;
    let instructions = find_key(data, "instructions")
        .and_then(Value::as_array)
        .with_context(|| format!("X's answer has no list in it{detail}"))?;
    let mut page = Page {
        posts: Vec::new(),
        entries: 0,
        next: None,
    };
    for instruction in instructions {
        let entries = instruction
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| entries.iter().collect())
            .or_else(|| instruction.get("entry").map(|entry| vec![entry]))
            .unwrap_or_default();
        for entry in entries {
            read_entry(&entry["content"], &mut page);
        }
    }
    Ok(page)
}

/// Add what a timeline entry holds to `page`: a post, a group of posts, or
/// the cursor for the next page.
fn read_entry(content: &Value, page: &mut Page) {
    match content["entryType"]
        .as_str()
        .or(content["__typename"].as_str())
    {
        Some("TimelineTimelineItem") => read_item(&content["itemContent"], page),
        Some("TimelineTimelineModule") => {
            for item in content["items"].as_array().into_iter().flatten() {
                if let Some(item) = item.pointer("/item/itemContent") {
                    read_item(item, page);
                }
            }
        }
        Some("TimelineTimelineCursor") if content["cursorType"] == "Bottom" => {
            page.next = content["value"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_string);
        }
        _ => {}
    }
}

fn read_item(item: &Value, page: &mut Page) {
    let Some(result) = item.pointer("/tweet_results/result") else {
        return;
    };
    page.entries += 1;
    if let Some(post) = parse_post(result) {
        page.posts.push(post);
    }
}

/// The first value named `key` anywhere inside `value`, breadth first: the
/// list sits at a different depth in each list's answer.
fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut queue = std::collections::VecDeque::from([value]);
    while let Some(value) = queue.pop_front() {
        match value {
            Value::Object(fields) => {
                if let Some(found) = fields.get(key) {
                    return Some(found);
                }
                queue.extend(fields.values());
            }
            Value::Array(items) => queue.extend(items),
            _ => {}
        }
    }
    None
}

/// The post in an API result: the result itself, or the post inside the
/// wrapper X puts round some (a post whose replies are limited, say). None
/// for a deleted or withheld one (a tombstone).
fn tweet_of(result: &Value) -> Option<&Value> {
    let tweet = match result["__typename"].as_str() {
        Some("TweetWithVisibilityResults") => &result["tweet"],
        _ => result,
    };
    tweet.get("legacy").is_some().then_some(tweet)
}

/// A post as we store it: unwrapped, and its quoted post too, so its fields
/// sit at the same place in every post (which `db/migrations` reads).
fn stored(tweet: &Value) -> Value {
    let mut tweet = tweet.clone();
    if let Some(quoted) = tweet.pointer_mut("/quoted_status_result/result") {
        match tweet_of(quoted).cloned() {
            Some(inner) => *quoted = inner,
            None => {
                if let Some(fields) = tweet.as_object_mut() {
                    fields.remove("quoted_status_result");
                }
            }
        }
    }
    tweet
}

/// A post with media, its own or the one it quotes, or None for a deleted
/// post or one that's only text.
fn parse_post(result: &Value) -> Option<Post> {
    let tweet = tweet_of(result)?;
    let raw = stored(tweet);
    let media = post_media(&raw);
    if media.is_empty() {
        return None;
    }
    let id = raw["rest_id"]
        .as_str()
        .or(raw.pointer("/legacy/id_str").and_then(Value::as_str))
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))?
        .to_string();
    let user = &raw["core"]["user_results"]["result"];
    let is_photo = !matches!(media.as_slice(), [only] if only.is_video);
    Some(Post {
        text: post_text(&raw),
        author_id: user["rest_id"].as_str().map(str::to_string),
        author_handle: handle(user),
        duration_seconds: if is_photo {
            None
        } else {
            media[0].duration_ms.map(|ms| (ms + 500) / 1000)
        },
        is_photo,
        posted_at: posted_at(&id).or_else(|| {
            raw.pointer("/legacy/created_at")?
                .as_str()
                .and_then(created_at)
        }),
        id,
        raw,
    })
}

/// A user's handle: under `core` lately, under `legacy` before.
fn handle(user: &Value) -> Option<String> {
    user.pointer("/core/screen_name")
        .or(user.pointer("/legacy/screen_name"))
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

fn display_name(user: &Value) -> Option<String> {
    user.pointer("/core/name")
        .or(user.pointer("/legacy/name"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// When a post was made, from its id, in unix seconds. None for an id from
/// before snowflakes, which doesn't say.
fn posted_at(id: &str) -> Option<i64> {
    let id: i64 = id.parse().ok()?;
    (id >= FIRST_SNOWFLAKE).then(|| ((id >> 22) + SNOWFLAKE_EPOCH_MS) / 1000)
}

/// A post's `created_at` ("Fri Oct 16 01:57:58 +0000 2026") in unix seconds.
fn created_at(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let [_, month, day, time, offset, year] = text.split(' ').collect::<Vec<_>>()[..] else {
        return None;
    };
    let month = i64::try_from(MONTHS.iter().position(|m| *m == month)? + 1).ok()?;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let mut clock = time.split(':').map(|n| n.parse::<i64>().ok());
    let (hours, minutes, seconds) = (clock.next()??, clock.next()??, clock.next()??);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let offset: i64 = offset.get(1..)?.parse().ok()?;
    let offset = sign * (offset / 100 * 3600 + offset % 100 * 60);
    // Days from 1970-01-01 to the date (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hours * 3600 + minutes * 60 + seconds - offset)
}

/// The media of a stored post: its own, then its quoted post's.
fn post_media(tweet: &Value) -> Vec<Media> {
    let mut media = own_media(tweet);
    if let Some(quoted) = tweet.pointer("/quoted_status_result/result") {
        media.extend(own_media(quoted));
    }
    media
}

/// A post's own pictures and videos, in order, each at its best. A video
/// with no MP4 to fetch (a live broadcast's) is left out.
fn own_media(tweet: &Value) -> Vec<Media> {
    let items = tweet
        .pointer("/legacy/extended_entities/media")
        .or(tweet.pointer("/legacy/entities/media"))
        .and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let picture = item["media_url_https"].as_str()?;
            match item["type"].as_str()? {
                "photo" => Some(Media {
                    url: original(picture),
                    is_video: false,
                    poster: Vec::new(),
                    duration_ms: None,
                }),
                "video" | "animated_gif" => {
                    let best = item
                        .pointer("/video_info/variants")?
                        .as_array()?
                        .iter()
                        .filter(|v| v["content_type"] == "video/mp4")
                        .max_by_key(|v| v["bitrate"].as_u64().unwrap_or(0))?;
                    Some(Media {
                        url: best["url"].as_str()?.to_string(),
                        is_video: true,
                        poster: vec![original(picture), picture.to_string()],
                        duration_ms: item
                            .pointer("/video_info/duration_millis")
                            .and_then(Value::as_u64),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// A picture's link at its original size: X's page shows smaller copies, and
/// `name=orig` asks for the one that was uploaded.
fn original(url: &str) -> String {
    if url.contains('?') {
        return url.to_string();
    }
    match url.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') => format!("{stem}?format={ext}&name=orig"),
        _ => url.to_string(),
    }
}

/// A post's text as it reads on X: a long post's whole text, without the
/// @names a reply starts with or the link to its own media, links spelled
/// out, and `&amp;` and the like decoded.
fn post_text(tweet: &Value) -> String {
    if let Some(note) = tweet.pointer("/note_tweet/note_tweet_results/result")
        && let Some(text) = note["text"].as_str()
    {
        return spell_out_links(text, note.pointer("/entity_set/urls"))
            .trim()
            .to_string();
    }
    let legacy = &tweet["legacy"];
    // X escapes &, < and > in the text, but counts the range it shows in the
    // text as it reads, in code points.
    let full: Vec<char> = legacy["full_text"]
        .as_str()
        .unwrap_or("")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .chars()
        .collect();
    let range = legacy["display_text_range"].as_array();
    let bound = |i: usize, default: usize| {
        range
            .and_then(|r| r.get(i))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(default)
            .min(full.len())
    };
    let (start, end) = (bound(0, 0), bound(1, full.len()));
    let shown: String = full[start.min(end)..end].iter().collect();
    let mut text = spell_out_links(&shown, legacy.pointer("/entities/urls"));
    for media in legacy
        .pointer("/entities/media")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(link) = media["url"].as_str() {
            text = text.replace(link, "");
        }
    }
    text.trim().to_string()
}

/// `text` with its t.co links replaced by where they go.
fn spell_out_links(text: &str, urls: Option<&Value>) -> String {
    let mut text = text.to_string();
    for url in urls.and_then(Value::as_array).into_iter().flatten() {
        if let (Some(short), Some(long)) = (url["url"].as_str(), url["expanded_url"].as_str()) {
            text = text.replace(short, long);
        }
    }
    text
}

/// The media of a post as stored (`Post::raw`), for the download.
pub fn stored_media(listing_json: &str) -> Result<Vec<Media>> {
    let tweet: Value = serde_json::from_str(listing_json).context("the stored post isn't JSON")?;
    let media = post_media(&tweet);
    if media.is_empty() {
        bail!("the stored post has no media");
    }
    Ok(media)
}

/// The post a stored post (`Post::raw`) quotes, if it quotes one.
pub fn quoted(listing_json: &str) -> Option<Quoted> {
    let tweet: Value = serde_json::from_str(listing_json).ok()?;
    let quoted = tweet.pointer("/quoted_status_result/result")?;
    let user = &quoted["core"]["user_results"]["result"];
    Some(Quoted {
        id: quoted["rest_id"].as_str()?.to_string(),
        author_handle: handle(user),
        author_name: display_name(user),
        text: post_text(quoted),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn video(id: &str, variants: &Value) -> Value {
        json!({"type": "video", "media_url_https": format!("https://pbs.twimg.com/amplify_video_thumb/{id}/img/t.jpg"),
               "url": "https://t.co/media",
               "video_info": {"duration_millis": 15_395, "variants": variants}})
    }

    fn photo(id: &str) -> Value {
        json!({"type": "photo", "media_url_https": format!("https://pbs.twimg.com/media/{id}.jpg"),
               "url": "https://t.co/media"})
    }

    fn tweet(id: &str, text: &str, media: &[Value]) -> Value {
        json!({
            "__typename": "Tweet",
            "rest_id": id,
            "core": {"user_results": {"result": {"rest_id": "44196397",
                "core": {"screen_name": "someone", "name": "Some One"}}}},
            "legacy": {"full_text": text, "id_str": id,
                       "display_text_range": [0, text.chars().count()],
                       "entities": {"urls": [], "media": media},
                       "extended_entities": {"media": media}},
        })
    }

    fn page(entries: &[Value]) -> String {
        json!({"data": {"bookmark_timeline_v2": {"timeline": {"instructions": [
            {"type": "TimelineAddEntries", "entries": entries}
        ]}}}})
        .to_string()
    }

    fn item(result: &Value) -> Value {
        json!({"entryId": "tweet-1", "content": {"entryType": "TimelineTimelineItem",
            "itemContent": {"tweet_results": {"result": result}}}})
    }

    fn cursor(kind: &str, value: &str) -> Value {
        json!({"entryId": "cursor", "content": {"entryType": "TimelineTimelineCursor",
            "cursorType": kind, "value": value}})
    }

    #[test]
    fn keeps_posts_with_media_and_skips_the_rest() {
        let variants = json!([
            {"content_type": "application/x-mpegURL", "url": "https://video.twimg.com/pl.m3u8"},
            {"bitrate": 256_000, "content_type": "video/mp4", "url": "https://video.twimg.com/270.mp4"},
            {"bitrate": 10_368_000, "content_type": "video/mp4", "url": "https://video.twimg.com/1080.mp4"},
        ]);
        let wrapped = json!({"__typename": "TweetWithVisibilityResults",
            "tweet": tweet("2110913107094606330", "flying cars https://t.co/media", &[video("1", &variants)])});
        let body = page(&[
            item(&wrapped),
            item(&tweet("2", "just words", &[])),
            item(&json!({"__typename": "TweetTombstone", "tombstone": {}})),
            item(&tweet("3", "", &[photo("A"), photo("B")])),
            cursor("Top", "up"),
            cursor("Bottom", "down"),
        ]);
        let page = parse_page(200, &body).unwrap();
        assert_eq!(page.entries, 4);
        assert_eq!(page.next.as_deref(), Some("down"));
        let ids: Vec<&str> = page.posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["2110913107094606330", "3"]);

        let video = &page.posts[0];
        assert!(!video.is_photo);
        assert_eq!(video.text, "flying cars");
        assert_eq!(video.duration_seconds, Some(15));
        assert_eq!(video.author_handle.as_deref(), Some("someone"));
        assert_eq!(video.author_id.as_deref(), Some("44196397"));
        // Fri Oct 16 01:57:58 +0000 2026
        assert_eq!(video.posted_at, Some(1_792_115_878));
        assert_eq!(
            video.url(),
            "https://x.com/someone/status/2110913107094606330"
        );
        // Stored unwrapped, so the download reads the media where it expects.
        let media = stored_media(&video.raw.to_string()).unwrap();
        assert_eq!(media[0].url, "https://video.twimg.com/1080.mp4");
        assert_eq!(
            media[0].poster,
            [
                "https://pbs.twimg.com/amplify_video_thumb/1/img/t?format=jpg&name=orig",
                "https://pbs.twimg.com/amplify_video_thumb/1/img/t.jpg"
            ]
        );

        let photos = &page.posts[1];
        assert!(photos.is_photo);
        assert_eq!(photos.duration_seconds, None);
        assert_eq!(
            stored_media(&photos.raw.to_string()).unwrap()[1].url,
            "https://pbs.twimg.com/media/B?format=jpg&name=orig"
        );
    }

    #[test]
    fn keeps_a_text_post_that_quotes_media_with_the_quoted_post() {
        let mut post = tweet("10", "look at this", &[]);
        post["quoted_status_result"] = json!({"result": {"__typename": "TweetWithVisibilityResults",
            "tweet": tweet("9", "the original &amp; best", &[photo("Q")])}});
        let page = parse_page(200, &page(&[item(&post)])).unwrap();
        let post = &page.posts[0];
        assert!(post.is_photo);
        assert_eq!(post.text, "look at this");
        let raw = post.raw.to_string();
        assert_eq!(
            stored_media(&raw).unwrap()[0].url,
            "https://pbs.twimg.com/media/Q?format=jpg&name=orig"
        );
        assert_eq!(
            quoted(&raw),
            Some(Quoted {
                id: "9".into(),
                author_handle: Some("someone".into()),
                author_name: Some("Some One".into()),
                text: "the original & best".into(),
            })
        );
        // Unwrapped where the search index reads it.
        assert_eq!(
            post.raw.pointer("/quoted_status_result/result/rest_id"),
            Some(&json!("9"))
        );
    }

    #[test]
    fn a_quoted_video_alone_makes_a_video_post() {
        let mut post = tweet("10", "lol", &[]);
        let variants = json!([{"bitrate": 1, "content_type": "video/mp4", "url": "https://video.twimg.com/v.mp4"}]);
        post["quoted_status_result"] = json!({"result": tweet("9", "", &[video("2", &variants)])});
        let page = parse_page(200, &page(&[item(&post)])).unwrap();
        assert!(!page.posts[0].is_photo);
        assert_eq!(page.posts[0].duration_seconds, Some(15));
    }

    #[test]
    fn reads_text_as_it_shows_on_x() {
        let mut reply = tweet(
            "1",
            "@a @b hi &lt;3 https://t.co/x https://t.co/media",
            &[photo("P")],
        );
        reply["legacy"]["display_text_range"] = json!([6, 26]);
        reply["legacy"]["entities"]["urls"] =
            json!([{"url": "https://t.co/x", "expanded_url": "https://example.com/page"}]);
        assert_eq!(post_text(&reply), "hi <3 https://example.com/page");
        // A long post's text is all in its note.
        let mut long = tweet("2", "cut off…", &[]);
        long["note_tweet"] = json!({"note_tweet_results": {"result": {"text": "the whole thing https://t.co/y",
            "entity_set": {"urls": [{"url": "https://t.co/y", "expanded_url": "https://y.example"}]}}}});
        assert_eq!(post_text(&long), "the whole thing https://y.example");
        // Code points, not bytes.
        let mut emoji = tweet("3", "🎮 game https://t.co/media", &[photo("P")]);
        emoji["legacy"]["display_text_range"] = json!([0, 6]);
        assert_eq!(post_text(&emoji), "🎮 game");
    }

    #[test]
    fn dates_posts() {
        assert_eq!(posted_at("2110913107094606330"), Some(1_792_115_878));
        assert_eq!(
            created_at("Fri Oct 16 01:57:58 +0000 2026"),
            Some(1_792_115_878)
        );
        assert_eq!(
            created_at("Tue Jun 02 20:12:29 +0000 2009"),
            Some(1_243_973_549)
        );
        assert_eq!(
            created_at("Tue Jun 02 20:12:29 -0130 2009"),
            Some(1_243_973_549 + 5400)
        );
        // Before snowflakes, the id doesn't say.
        assert_eq!(posted_at("20"), None);
        assert_eq!(created_at("yesterday"), None);
    }

    #[test]
    fn an_empty_page_has_no_posts() {
        let page = parse_page(200, &page(&[cursor("Top", "a"), cursor("Bottom", "b")])).unwrap();
        assert_eq!(page.entries, 0);
        assert!(page.posts.is_empty());
    }

    #[test]
    fn finds_the_list_in_the_like_lists_answer() {
        let body = json!({"data": {"user": {"result": {"timeline": {"timeline": {"instructions": [
            {"type": "TimelineAddEntries", "entries": [item(&tweet("5", "x", &[photo("P")]))]}
        ]}}}}}});
        assert_eq!(parse_page(200, &body.to_string()).unwrap().posts.len(), 1);
    }

    #[test]
    fn tells_refusals_apart() {
        let lapsed = parse_page(
            401,
            r#"{"errors":[{"code":32,"message":"Could not authenticate you."}]}"#,
        )
        .unwrap_err();
        assert!(lapsed.downcast_ref::<LoggedOut>().is_some());
        assert!(is_final(&lapsed));
        let gone = parse_page(404, r#"{"errors":[{"message":"Query not found"}]}"#).unwrap_err();
        assert!(is_outdated(&gone));
        assert!(format!("{gone}").contains("Query not found"));
        let limited = parse_page(429, "Rate limit exceeded").unwrap_err();
        assert!(is_final(&limited) && !is_outdated(&limited));
        assert!(is_throttled(&limited.context("reading the likes")));
        assert!(!is_throttled(&gone));
        // Worth another try.
        assert!(!is_final(&parse_page(503, "").unwrap_err()));
        assert!(!is_final(&parse_page(200, "<html>").unwrap_err()));
        assert!(!is_final(
            &parse_page(200, r#"{"errors":[{"message":"oops"}]}"#).unwrap_err()
        ));
    }

    #[test]
    fn reads_the_user_from_the_twid_cookie() {
        let cookie = |value: &str| Cookie {
            host: ".x.com".into(),
            path: "/".into(),
            secure: true,
            expires: 0,
            name: "twid".into(),
            value: value.into(),
        };
        assert_eq!(
            user_id(&[cookie("u%3D2080863955564572673")]).unwrap(),
            "2080863955564572673"
        );
        assert_eq!(user_id(&[cookie("\"u=42\"")]).unwrap(), "42");
        assert!(user_id(&[cookie("u%3Dabc")]).is_err());
        assert!(user_id(&[]).is_err());
    }

    #[test]
    fn asks_for_pictures_at_their_original_size() {
        assert_eq!(
            original("https://pbs.twimg.com/media/HUrv43LWEAAuS2m.png"),
            "https://pbs.twimg.com/media/HUrv43LWEAAuS2m?format=png&name=orig"
        );
        assert_eq!(
            original("https://pbs.twimg.com/media/a?format=jpg"),
            "https://pbs.twimg.com/media/a?format=jpg"
        );
    }

    #[test]
    fn page_scripts_escape_their_values() {
        let api = Api {
            authorization: "Bearer AAA%3D".into(),
            bookmarks: Operation {
                query_id: "VjWsfItbiMLeajpZ90NJeg".into(),
                features: r#"{"a":true}"#.into(),
                field_toggles: None,
            },
            likes: Operation {
                query_id: "mu4us66Q6RRNOywVQhGtRg".into(),
                features: r#"{"a":true}"#.into(),
                field_toggles: Some(r#"{"withPayments":false}"#.into()),
            },
        };
        let js = page_request(&api, List::Bookmarks, "42", Some("c'\"=="));
        assert!(js.contains(r#"["variables", "{\"count\":20,\"cursor\":\"c'\\\"==\",\"includePromotedContent\":true}"]"#));
        assert!(js.contains("/Bookmarks?"));
        assert!(!js.contains("fieldToggles"));
        let js = page_request(&api, List::Likes, "42", None);
        assert!(js.contains(r#"{\"userId\":\"42\",\"count\":20,\"includePromotedContent\":false"#));
        assert!(js.contains(r#"["fieldToggles", "{\"withPayments\":false}"]"#));
        assert!(js.contains(r#"referrer: "https://x.com/i/history/likes""#));
    }
}
