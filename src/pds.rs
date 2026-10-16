//! Uploading the archive to the user's account on an AT Protocol PDS (the
//! server that holds a Bluesky account's data), so it also lives somewhere
//! off this machine. It's an XRPC client for the five calls that takes:
//! `createSession` and `refreshSession` (an app password, never the account's
//! own password), `describeServer` (the server's upload limit),
//! `uploadBlob` (one file, the raw bytes as the body) and `putRecord`.
//!
//! Each liked post becomes one record in `COLLECTION`, keyed `<platform>-<id>`
//! (`record_key`), so uploading a post again replaces its record instead of
//! adding one. The record holds what the post is (platform, link, title,
//! author, dates) and every file its download saved, each as a blob. A blob
//! no record points to is deleted by the PDS after an hour or more, so a
//! post's files are uploaded and its record written right after, one post at
//! a time. Everything in a repo is public: anyone can fetch the blobs, and
//! the records go out on the network's firehose.
//!
//! The login is the one exception to logging in through the browser's
//! cookies: a PDS takes a handle and an app password. Neither the password
//! nor the session's tokens are ever logged.
//!
//! Try it against a real account: put `BLUESKY_ACCOUNT_HANDLE` and
//! `BLUESKY_APP_PASSWORD` in `.env`, copy the database somewhere scratch,
//! and run `cargo run -- pds --db <copy> --media data/downloads --limit 1`.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::db::PendingUpload;

/// The records' type. Named under the project's GitHub account, which it
/// owns, as an NSID must be; no lexicon is published, and a PDS accepts
/// records of types it doesn't know.
pub const COLLECTION: &str = "io.github.sergiotapia.joyscroller.video";

/// What an upload sends at the least, in bytes a second, to set how long it
/// may take: about a megabit, a slow home uplink's worst.
const SLOWEST_UPLINK: u64 = 125_000;

/// A logged-in session on a PDS.
pub struct Session {
    /// Where the account's repo lives, from its DID document. For an
    /// account on Bluesky's servers that's one of its PDSes
    /// (`*.host.bsky.network`), not `bsky.social`, where it logs in.
    pub pds: String,
    pub did: String,
    pub handle: String,
    /// What the user logged in with, to log in again when the refresh token
    /// runs out.
    service: String,
    identifier: String,
    app_password: String,
    access_jwt: String,
    refresh_jwt: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("pds", &self.pds)
            .field("did", &self.did)
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

/// What the PDS says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Server {
    /// The largest file it takes. `blobUploadLimit` isn't in the standard
    /// `describeServer`, but Bluesky's PDSes and vlpds send it: 300 MiB on
    /// Bluesky's, 100 MiB on vlpds.jazco.dev (seen 2026-10-16). Without it,
    /// the limit is learned from the first upload refused as too big.
    pub blob_limit_bytes: Option<u64>,
}

/// A file uploaded as a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// The file's name in its download folder.
    pub file: String,
    /// The blob's CID, which is a hash of its bytes.
    pub cid: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// The blob ref as the PDS returned it, for the record.
    pub json: Value,
}

/// A post uploaded: its files and the record that points to them.
#[derive(Debug, Clone)]
pub struct Uploaded {
    pub record_uri: String,
    pub record_cid: String,
    pub blobs: Vec<Blob>,
}

/// The PDS turned the app password away, or the account can't be used. The
/// user has to connect it again in Settings.
#[derive(Debug)]
pub struct LoggedOut(pub String);

impl std::fmt::Display for LoggedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server refused the login: {}", self.0)
    }
}

impl std::error::Error for LoggedOut {}

/// A file is larger than the PDS takes. The post isn't uploaded, and isn't
/// tried again unless the limit changes.
#[derive(Debug)]
pub struct TooBig {
    pub file: String,
    pub size_bytes: u64,
    /// The limit, if the PDS said it.
    pub limit_bytes: Option<u64>,
}

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mib = |b: u64| b.div_ceil(1024 * 1024);
        match self.limit_bytes {
            Some(limit) => write!(
                f,
                "{} is too big for this server ({} MiB; it takes up to {} MiB)",
                self.file,
                mib(self.size_bytes),
                mib(limit)
            ),
            None => write!(
                f,
                "{} is too big for this server ({} MiB)",
                self.file,
                mib(self.size_bytes)
            ),
        }
    }
}

impl std::error::Error for TooBig {}

/// The upload was stopped from this side: paused, or the account
/// disconnected. It isn't the post's fault and isn't counted against it.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the upload was stopped")
    }
}

impl std::error::Error for Stopped {}

/// The PDS said to slow down (HTTP 429). Nothing is uploaded until `until`
/// (unix seconds), when it said its limit resets, if it said.
#[derive(Debug)]
pub struct RateLimited {
    pub until: Option<i64>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server says to slow down (HTTP 429)")
    }
}

impl std::error::Error for RateLimited {}

/// An answer from the PDS, before it's read as success or failure.
struct Reply {
    status: u16,
    /// `ratelimit-reset`, unix seconds, on a 429.
    reset: Option<i64>,
    body: String,
}

impl Reply {
    fn read(mut response: ureq::http::Response<ureq::Body>) -> Result<Reply> {
        let status = response.status().as_u16();
        let reset = response
            .headers()
            .get("ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let body = response
            .body_mut()
            .with_config()
            .limit(16 * 1024 * 1024)
            .read_to_string()
            .context("reading the server's answer")?;
        Ok(Reply {
            status,
            reset,
            body,
        })
    }

    /// The XRPC error's name and message, when it isn't a success.
    fn error(&self) -> (String, String) {
        let parsed: Option<Value> = serde_json::from_str(&self.body).ok();
        let field = |name: &str| {
            parsed
                .as_ref()
                .and_then(|p| p.get(name))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        (field("error"), field("message"))
    }

    /// A 413, or the PDS's own words for a file over its limit. The
    /// reference PDS answers `PayloadTooLarge`; others may only close the
    /// connection, which reads as a failed upload.
    fn is_too_big(&self) -> bool {
        let (error, message) = self.error();
        self.status == 413
            || error.contains("TooLarge")
            || message.to_ascii_lowercase().contains("too large")
    }

    fn is_expired_token(&self) -> bool {
        self.status == 400 && self.error().0 == "ExpiredToken"
    }

    /// The answer's JSON, or why the PDS refused `what`.
    fn json(self, what: &str) -> Result<Value> {
        if (200..300).contains(&self.status) {
            return serde_json::from_str(&self.body)
                .with_context(|| format!("reading the server's answer to {what}"));
        }
        let (error, message) = self.error();
        let words = match (error.is_empty(), message.is_empty()) {
            (false, false) => format!("{error}: {message}"),
            (false, true) => error.clone(),
            (true, false) => message,
            (true, true) => format!("HTTP {}", self.status),
        };
        if self.status == 429 {
            return Err(RateLimited { until: self.reset }.into());
        }
        if self.status == 401
            || matches!(
                error.as_str(),
                "AuthenticationRequired"
                    | "InvalidToken"
                    | "AccountTakedown"
                    | "AccountDeactivated"
            )
        {
            return Err(LoggedOut(words).into());
        }
        bail!("the server refused {what} (HTTP {}): {words}", self.status)
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_mins(5)))
        .timeout_recv_body(Some(Duration::from_mins(1)))
        // A refusal's body says why; `Reply::json` reads it.
        .http_status_as_error(false)
        .build()
        .into()
}

/// The service URL as typed: a host alone means https, and no trailing slash.
pub fn service_url(typed: &str) -> Result<String> {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty() {
        bail!("no server address");
    }
    let url = if typed.contains("://") {
        typed.to_string()
    } else {
        format!("https://{typed}")
    };
    check_url(&url)?;
    Ok(url)
}

/// Only https, except to this machine, where a test PDS runs without it:
/// the app password and tokens go to this address.
fn check_url(url: &str) -> Result<()> {
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|l| {
            url == *l || url.starts_with(&format!("{l}:")) || url.starts_with(&format!("{l}/"))
        });
    if !url.starts_with("https://") && !local {
        bail!("the server address has to start with https:// ({url})");
    }
    Ok(())
}

/// Where a DID document says the account's repo lives.
fn pds_endpoint(did_doc: &Value) -> Option<String> {
    did_doc
        .get("service")?
        .as_array()?
        .iter()
        .find(|s| {
            s.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.ends_with("#atproto_pds"))
        })?
        .get("serviceEndpoint")?
        .as_str()
        .map(|e| e.trim_end_matches('/').to_string())
}

impl Session {
    /// Log in to `service` (the PDS, or an entryway like bsky.social) with a
    /// handle or DID and an app password.
    pub fn log_in(service: &str, identifier: &str, app_password: &str) -> Result<Session> {
        let service = service_url(service)?;
        let agent = agent();
        let answer = create_session(&agent, &service, identifier, app_password)?;
        let field = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("the server's login answer has no {name}"))
        };
        let pds = match answer.get("didDoc").and_then(pds_endpoint) {
            Some(pds) => pds,
            None => service.clone(),
        };
        check_url(&pds).context("the account's DID document points to its server")?;
        Ok(Session {
            did: field("did")?,
            handle: field("handle")?,
            access_jwt: field("accessJwt")?,
            refresh_jwt: field("refreshJwt")?,
            pds,
            service,
            identifier: identifier.to_string(),
            app_password: app_password.to_string(),
            agent,
        })
    }

    /// Renew the access token with the refresh token, or log in again once
    /// that has run out too.
    fn refresh(&mut self) -> Result<()> {
        let reply = self
            .agent
            .post(format!(
                "{}/xrpc/com.atproto.server.refreshSession",
                self.pds
            ))
            .header("Authorization", &format!("Bearer {}", self.refresh_jwt))
            .send_empty()
            .context("renewing the server login")
            .and_then(Reply::read)?;
        let renewed = if reply.is_expired_token() {
            create_session(
                &self.agent,
                &self.service,
                &self.identifier,
                &self.app_password,
            )?
        } else {
            match reply.json("renewing the login") {
                Ok(renewed) => renewed,
                // A revoked refresh token: the app password may still work.
                Err(e) if e.is::<LoggedOut>() => create_session(
                    &self.agent,
                    &self.service,
                    &self.identifier,
                    &self.app_password,
                )?,
                Err(e) => return Err(e),
            }
        };
        let token = |name: &str| {
            renewed
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("the server's renewed login has no {name}"))
        };
        self.access_jwt = token("accessJwt")?;
        self.refresh_jwt = token("refreshJwt")?;
        Ok(())
    }

    /// Send a request made by `request` (given the agent, the PDS and the
    /// Authorization header), renewing the login once if it ran out.
    fn authed(
        &mut self,
        request: impl Fn(&ureq::Agent, &str, &str) -> Result<Reply>,
    ) -> Result<Reply> {
        let reply = request(
            &self.agent,
            &self.pds,
            &format!("Bearer {}", self.access_jwt),
        )?;
        if !reply.is_expired_token() {
            return Ok(reply);
        }
        self.refresh()?;
        request(
            &self.agent,
            &self.pds,
            &format!("Bearer {}", self.access_jwt),
        )
    }

    /// What the account's PDS says about itself.
    pub fn describe_server(&self) -> Result<Server> {
        let answer = self
            .agent
            .get(format!(
                "{}/xrpc/com.atproto.server.describeServer",
                self.pds
            ))
            .call()
            .context("asking the server about itself")
            .and_then(Reply::read)?
            .json("describing itself")?;
        Ok(parse_server(&answer))
    }

    /// Upload one file as a blob, reporting the bytes sent so far.
    /// `sent` gets the bytes sent so far and says whether to go on; false
    /// stops the upload.
    pub fn upload_blob(&mut self, path: &Path, sent: &dyn Fn(u64) -> bool) -> Result<Blob> {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("{} has no file name", path.display()))?
            .to_string();
        let mime_type = mime_type(&file);
        let size_bytes = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        // Long enough for the whole file over a slow uplink.
        let send_time = Duration::from_mins(1) + Duration::from_secs(size_bytes / SLOWEST_UPLINK);
        let reply = self.authed(|agent, pds, auth| {
            let mut body = Counting {
                inner: std::fs::File::open(path)
                    .with_context(|| format!("opening {}", path.display()))?,
                sent: 0,
                report: sent,
            };
            agent
                .post(format!("{pds}/xrpc/com.atproto.repo.uploadBlob"))
                .config()
                .timeout_send_body(Some(send_time))
                .build()
                .header("Authorization", auth)
                .header("Content-Type", mime_type)
                // The PDS wants the exact length; a chunked body is refused.
                .header("Content-Length", &size_bytes.to_string())
                .send(ureq::SendBody::from_reader(&mut body))
                .with_context(|| format!("uploading {file}"))
                .and_then(Reply::read)
        })?;
        if reply.is_too_big() {
            return Err(TooBig {
                file,
                size_bytes,
                limit_bytes: None,
            }
            .into());
        }
        let answer = reply.json(&format!("the upload of {file}"))?;
        let json = answer
            .get("blob")
            .cloned()
            .with_context(|| format!("the server's answer to the upload of {file} has no blob"))?;
        let cid = json
            .pointer("/ref/$link")
            .and_then(Value::as_str)
            .with_context(|| format!("the server's answer to the upload of {file} has no CID"))?
            .to_string();
        Ok(Blob {
            file,
            cid,
            mime_type: mime_type.to_string(),
            size_bytes,
            json,
        })
    }

    /// Write `record` as the account's record `rkey` in `COLLECTION`,
    /// replacing any there. Returns its `at://` URI and CID.
    pub fn put_record(&mut self, rkey: &str, record: &Value) -> Result<(String, String)> {
        let body = json!({
            "repo": self.did,
            "collection": COLLECTION,
            "rkey": rkey,
            "record": record,
        });
        let answer = self
            .authed(|agent, pds, auth| {
                agent
                    .post(format!("{pds}/xrpc/com.atproto.repo.putRecord"))
                    .header("Authorization", auth)
                    .header("Content-Type", "application/json")
                    .send(body.to_string())
                    .context("writing the record")
                    .and_then(Reply::read)
            })?
            .json("writing the record")?;
        let field = |name: &str| {
            answer
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .with_context(|| format!("the server's answer to the record has no {name}"))
        };
        Ok((field("uri")?, field("cid")?))
    }

    /// Upload `post`'s files from under `media_root`, then write its record.
    /// `limit_bytes` is the PDS's upload limit, if known: a post with a file
    /// over it is `TooBig` before anything is sent. `progress` gets the bytes
    /// sent and the post's total, and says whether to go on: false stops the
    /// upload with `Stopped`, between two reads of a file.
    pub fn upload_post(
        &mut self,
        media_root: &Path,
        post: &PendingUpload,
        limit_bytes: Option<u64>,
        progress: &dyn Fn(u64, u64) -> bool,
    ) -> Result<Uploaded> {
        let rkey = record_key(&post.platform, &post.video_id)?;
        let folder = media_root.join(&post.folder);
        let mut sizes = Vec::with_capacity(post.files.len());
        for file in &post.files {
            let path = folder.join(file);
            let size = std::fs::metadata(&path)
                .with_context(|| format!("reading {}", path.display()))?
                .len();
            if let Some(limit) = limit_bytes.filter(|&limit| size > limit) {
                return Err(TooBig {
                    file: file.clone(),
                    size_bytes: size,
                    limit_bytes: Some(limit),
                }
                .into());
            }
            sizes.push(size);
        }
        let total: u64 = sizes.iter().sum();
        let mut done = 0;
        let mut blobs = Vec::with_capacity(post.files.len());
        for (file, size) in post.files.iter().zip(&sizes) {
            if !progress(done, total) {
                return Err(Stopped.into());
            }
            // An upload cut short by `progress` fails as the connection does.
            let blob = self
                .upload_blob(&folder.join(file), &|sent| progress(done + sent, total))
                .map_err(|e| {
                    if progress(done, total) {
                        e
                    } else {
                        Stopped.into()
                    }
                })?;
            done += size;
            blobs.push(blob);
        }
        let (record_uri, record_cid) = self.put_record(&rkey, &record(post, &blobs))?;
        Ok(Uploaded {
            record_uri,
            record_cid,
            blobs,
        })
    }
}

fn create_session(
    agent: &ureq::Agent,
    service: &str,
    identifier: &str,
    app_password: &str,
) -> Result<Value> {
    let body = json!({"identifier": identifier.trim(), "password": app_password.trim()});
    agent
        .post(format!("{service}/xrpc/com.atproto.server.createSession"))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .with_context(|| format!("logging in to {service}"))
        .and_then(Reply::read)?
        .json("the login")
}

fn parse_server(answer: &Value) -> Server {
    Server {
        blob_limit_bytes: answer.get("blobUploadLimit").and_then(Value::as_u64),
    }
}

/// A post's record key: `<platform>-<id>`. Every platform's ids fit the
/// record-key syntax, but one that didn't would be refused, not mangled.
pub fn record_key(platform: &str, video_id: &str) -> Result<String> {
    let key = format!("{platform}-{video_id}");
    let valid = key.len() <= 512
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:~".contains(&b));
    if !valid {
        bail!("{platform} post {video_id} has an id a record key can't hold");
    }
    Ok(key)
}

/// A post's record. Fields with nothing in them are left out, as records do.
fn record(post: &PendingUpload, blobs: &[Blob]) -> Value {
    let mut record = json!({
        "$type": COLLECTION,
        "platform": post.platform,
        "postId": post.video_id,
        "kind": post.kind,
        "url": post.url,
        "addedAt": datetime(post.added_at),
        "likeOrder": post.like_order,
        "files": blobs
            .iter()
            .map(|b| json!({"name": b.file, "file": b.json}))
            .collect::<Vec<_>>(),
        "createdAt": datetime(crate::db::now()),
    });
    let posted_at = post.posted_at.map(datetime);
    for (name, value) in [
        ("title", &post.title),
        ("author", &post.author_name),
        ("postedAt", &posted_at),
    ] {
        if let Some(value) = value {
            record[name] = json!(value);
        }
    }
    record
}

/// Unix seconds as the RFC 3339 time records use, in UTC to the
/// millisecond: `2026-10-16T15:50:00.000Z`.
fn datetime(unix: i64) -> String {
    // Howard Hinnant's days-to-civil, from 1970-01-01.
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// What a file is, by its extension: every kind the downloads save.
fn mime_type(file: &str) -> &'static str {
    let ext = file.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// A reader that reports how much has been read through it, and fails
/// when the report says to stop.
struct Counting<'a, R> {
    inner: R,
    sent: u64,
    report: &'a dyn Fn(u64) -> bool,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !(self.report)(self.sent) {
            return Err(std::io::Error::other(Stopped));
        }
        let n = self.inner.read(buf)?;
        self.sent += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_urls_default_to_https() {
        assert_eq!(service_url("bsky.social").unwrap(), "https://bsky.social");
        assert_eq!(
            service_url(" https://vlpds.jazco.dev/ ").unwrap(),
            "https://vlpds.jazco.dev"
        );
        assert_eq!(
            service_url("http://localhost:2583").unwrap(),
            "http://localhost:2583"
        );
        assert!(service_url("http://example.com").is_err());
        assert!(service_url("http://localhost.example.com").is_err());
        assert!(service_url("").is_err());
    }

    #[test]
    fn finds_the_pds_in_a_did_document() {
        // Hand-built in the shape createSession returned for an account on
        // Bluesky's servers (seen 2026-10-16).
        let doc = json!({
            "id": "did:plc:abc",
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": "https://phellinus.us-west.host.bsky.network"
            }]
        });
        assert_eq!(
            pds_endpoint(&doc).as_deref(),
            Some("https://phellinus.us-west.host.bsky.network")
        );
        assert_eq!(pds_endpoint(&json!({"service": []})), None);
    }

    #[test]
    fn reads_the_upload_limit() {
        // vlpds.jazco.dev's answer, trimmed (seen 2026-10-16).
        let vlpds = json!({
            "did": "did:web:vlpds.jazco.dev",
            "blobUploadLimit": 104_857_600,
            "vlpds": {"spaces": true}
        });
        assert_eq!(
            parse_server(&vlpds),
            Server {
                blob_limit_bytes: Some(104_857_600)
            }
        );
        assert_eq!(
            parse_server(&json!({"did": "did:web:x"})),
            Server::default()
        );
    }

    #[test]
    fn record_keys_hold_every_platforms_ids() {
        assert_eq!(record_key("youtube", "a-B_c").unwrap(), "youtube-a-B_c");
        assert_eq!(
            record_key("tiktok", "7634911511458172191").unwrap(),
            "tiktok-7634911511458172191"
        );
        assert!(record_key("x", "a/b").is_err());
        assert!(record_key("x", &"9".repeat(600)).is_err());
    }

    fn post() -> PendingUpload {
        PendingUpload {
            platform: "tiktok".into(),
            video_id: "1".into(),
            kind: "video".into(),
            url: "https://www.tiktok.com/@a/video/1".into(),
            title: Some("hi".into()),
            author_name: None,
            posted_at: None,
            added_at: 1_790_000_000,
            like_order: 7,
            folder: "tiktok/1".into(),
            files: vec!["video.mp4".into()],
            downloaded_at: 1_790_000_000,
            size_bytes: 10,
        }
    }

    #[test]
    fn records_leave_out_what_isnt_known() {
        let blob = Blob {
            file: "video.mp4".into(),
            cid: "bafk".into(),
            mime_type: "video/mp4".into(),
            size_bytes: 3,
            json: json!({"$type": "blob", "ref": {"$link": "bafk"}, "mimeType": "video/mp4", "size": 3}),
        };
        let r = record(&post(), &[blob]);
        assert_eq!(r["$type"], COLLECTION);
        assert_eq!(r["title"], "hi");
        assert_eq!(r["files"][0]["name"], "video.mp4");
        assert_eq!(r["files"][0]["file"]["ref"]["$link"], "bafk");
        assert!(r.get("author").is_none());
        assert!(r.get("postedAt").is_none());
    }

    #[test]
    fn reads_refusals() {
        let reply = |status, body: &str| Reply {
            status,
            reset: Some(1_800_000_000),
            body: body.into(),
        };
        let limited = reply(429, "{}").json("x").unwrap_err();
        assert_eq!(
            limited.downcast_ref::<RateLimited>().unwrap().until,
            Some(1_800_000_000)
        );
        let refused = reply(
            401,
            r#"{"error": "AuthenticationRequired", "message": "Invalid identifier or password"}"#,
        );
        assert!(refused.json("x").unwrap_err().is::<LoggedOut>());
        assert!(reply(413, "").is_too_big());
        assert!(reply(400, r#"{"error": "PayloadTooLarge"}"#).is_too_big());
        assert!(reply(400, r#"{"error": "ExpiredToken"}"#).is_expired_token());
        let other = reply(500, r#"{"error": "InternalServerError"}"#).json("the upload");
        assert!(format!("{:#}", other.unwrap_err()).contains("InternalServerError"));
    }

    /// A session with no PDS at its address: a request would fail
    /// differently from what the tests look for.
    fn test_session() -> Session {
        Session {
            pds: "http://127.0.0.1:9".into(),
            did: "did:plc:abc".into(),
            handle: "a.test".into(),
            service: "http://127.0.0.1:9".into(),
            identifier: "a.test".into(),
            app_password: String::new(),
            access_jwt: String::new(),
            refresh_jwt: String::new(),
            agent: agent(),
        }
    }

    #[test]
    fn a_post_over_the_limit_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("tiktok/1");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("video.mp4"), [0; 10]).unwrap();
        let mut session = test_session();
        let e = session
            .upload_post(dir.path(), &post(), Some(5), &|_, _| true)
            .unwrap_err();
        let too_big = e.downcast_ref::<TooBig>().unwrap();
        assert_eq!((too_big.size_bytes, too_big.limit_bytes), (10, Some(5)));
    }

    #[test]
    fn a_stop_ends_the_upload_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("tiktok/1");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("video.mp4"), [0; 10]).unwrap();
        let mut session = test_session();
        let e = session
            .upload_post(dir.path(), &post(), None, &|_, _| false)
            .unwrap_err();
        assert!(e.is::<Stopped>());
        // Read through a stopped report, a file fails.
        let mut reader = Counting {
            inner: &[0_u8; 4][..],
            sent: 0,
            report: &|_| false,
        };
        assert!(reader.read(&mut [0; 4]).is_err());
    }

    #[test]
    fn writes_times_as_records_want_them() {
        assert_eq!(datetime(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(datetime(951_782_400), "2000-02-29T00:00:00.000Z");
        assert_eq!(datetime(1_792_165_800), "2026-10-16T15:50:00.000Z");
    }

    #[test]
    fn names_every_kind_of_file_the_downloads_save() {
        for (file, mime) in [
            ("thumbnail.jpg", "image/jpeg"),
            ("thumbnail.webp", "image/webp"),
            ("image-01.png", "image/png"),
            ("video.mp4", "video/mp4"),
            ("video.webm", "video/webm"),
            ("video.mkv", "video/x-matroska"),
            ("sound.m4a", "audio/mp4"),
            ("sound.mp3", "audio/mpeg"),
        ] {
            assert_eq!(mime_type(file), mime, "{file}");
        }
    }
}
