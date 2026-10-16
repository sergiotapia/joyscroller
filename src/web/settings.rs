//! /settings: connecting an AT Protocol PDS account, which the uploader
//! (`pipeline.rs`) copies each download to. The page says plainly that
//! everything uploaded is public before anything is. Connecting logs in
//! with the handle and app password typed, keeps the password in the OS
//! keyring (`keyring.rs`) and the rest in the database; disconnecting
//! forgets both and leaves what was uploaded where it is. Once connected,
//! it shows a bar of files uploaded and left (refreshed as uploads finish),
//! which platforms upload, and Pause, which stops an upload under way.

use std::fmt::Write as _;

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::{
    App, Failure, Frame, PLATFORMS, Section, display_name, escape, from_this_ui, layout,
    meter_html, thousands, uploader_html,
};
use crate::{db, keyring, pds};

/// The connect form. A checkbox's field is there only when it's ticked.
#[derive(Deserialize)]
pub(super) struct ConnectForm {
    service: String,
    handle: String,
    app_password: String,
    #[serde(flatten)]
    platforms: PlatformsForm,
}

#[derive(Deserialize)]
pub(super) struct PlatformsForm {
    youtube: Option<String>,
    tiktok: Option<String>,
    instagram: Option<String>,
    x: Option<String>,
}

impl PlatformsForm {
    fn ticked(&self) -> Vec<&'static str> {
        [
            ("youtube", &self.youtube),
            ("tiktok", &self.tiktok),
            ("instagram", &self.instagram),
            ("x", &self.x),
        ]
        .into_iter()
        .filter(|(_, ticked)| ticked.is_some())
        .map(|(name, _)| name)
        .collect()
    }
}

pub(super) async fn page(State(app): State<App>) -> Result<Html<String>, Failure> {
    render(&app, None, None)
}

/// Log in to the PDS, keep the password and connect the account, or show
/// the form again with what went wrong.
pub(super) async fn connect(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<ConnectForm>,
) -> Result<Response, Failure> {
    refuse_other_sites(&headers)?;
    match connect_account(&app, &form).await {
        Ok(()) => Ok(back_to_settings()),
        Err(e) => {
            eprintln!("pds: connecting {} failed: {e:#}", form.handle.trim());
            let said = if e.is::<pds::LoggedOut>() {
                format!(
                    "The server turned that handle and app password away. Check them, and that the \
                     app password hasn't been revoked. ({e:#})"
                )
            } else {
                format!("Couldn't connect: {e:#}")
            };
            Ok(render(&app, Some(&said), Some(&form))?.into_response())
        }
    }
}

async fn connect_account(app: &App, form: &ConnectForm) -> anyhow::Result<()> {
    let platforms = form.platforms.ticked();
    if platforms.is_empty() {
        anyhow::bail!("pick at least one platform to upload");
    }
    let service = pds::service_url(&form.service)?;
    let (handle, password) = (
        form.handle.trim().trim_start_matches('@'),
        form.app_password.trim(),
    );
    let (session, server) = {
        let (service, handle, password) = (service.clone(), handle.to_owned(), password.to_owned());
        tokio::task::spawn_blocking(move || {
            let session = pds::Session::log_in(&service, &handle, &password)?;
            let server = session.describe_server()?;
            anyhow::Ok((session, server))
        })
        .await??
    };
    let before = app.db(db::pds_account)?;
    keyring::save(&session.did, &session.handle, password).await?;
    app.db(|conn| {
        db::set_pds_account(
            conn,
            &db::PdsAccount {
                did: session.did.clone(),
                handle: session.handle.clone(),
                service_url: service,
                pds_url: session.pds.clone(),
                platforms: platforms.iter().map(ToString::to_string).collect(),
                blob_limit_bytes: server.blob_limit_bytes,
                paused: false,
            },
        )
    })?;
    // Connecting another account replaces the one before; its password
    // isn't needed anymore.
    if let Some(before) = before.filter(|b| b.did != session.did)
        && let Err(e) = keyring::forget(&before.did).await
    {
        eprintln!(
            "pds: couldn't forget {}'s app password: {e:#}",
            before.handle
        );
    }
    eprintln!(
        "pds: connected {} ({}) on {}",
        session.handle, session.did, session.pds
    );
    app.pipeline.account_connected();
    Ok(())
}

pub(super) async fn platforms(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<PlatformsForm>,
) -> Result<Response, Failure> {
    refuse_other_sites(&headers)?;
    let ticked = form.ticked();
    app.db(|conn| db::set_pds_platforms(conn, &ticked))?;
    app.pipeline.uploads_changed();
    Ok(back_to_settings())
}

pub(super) async fn disconnect(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    refuse_other_sites(&headers)?;
    if let Some(account) = app.db(db::pds_account)? {
        if let Err(e) = keyring::forget(&account.did).await {
            eprintln!(
                "pds: couldn't forget {}'s app password: {e:#}",
                account.handle
            );
        }
        app.db(db::forget_pds_account)?;
        eprintln!("pds: disconnected {}", account.handle);
    }
    app.pipeline.stop_uploads();
    Ok(back_to_settings())
}

/// Pause uploading, stopping an upload under way, until it's resumed.
pub(super) async fn pause(State(app): State<App>, headers: HeaderMap) -> Result<Response, Failure> {
    pause_or_resume(&app, &headers, true)
}

pub(super) async fn resume(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    pause_or_resume(&app, &headers, false)
}

fn pause_or_resume(app: &App, headers: &HeaderMap, paused: bool) -> Result<Response, Failure> {
    refuse_other_sites(headers)?;
    app.db(|conn| db::set_pds_paused(conn, paused))?;
    eprintln!(
        "pds: uploading {}",
        if paused { "paused" } else { "resumed" }
    );
    if paused {
        app.pipeline.stop_uploads();
    } else {
        app.pipeline.uploads_changed();
    }
    Ok(back_to_settings())
}

fn refuse_other_sites(headers: &HeaderMap) -> Result<(), Failure> {
    if from_this_ui(headers) {
        Ok(())
    } else {
        Err(Failure(
            StatusCode::FORBIDDEN,
            "refusing a settings change from another site".into(),
        ))
    }
}

fn back_to_settings() -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, HeaderValue::from_static("/settings"))],
    )
        .into_response()
}

fn render(
    app: &App,
    error: Option<&str>,
    typed: Option<&ConnectForm>,
) -> Result<Html<String>, Failure> {
    let (counts, after, account, progress) = app.db(|conn| {
        Ok((
            db::video_counts(conn)?,
            db::last_rowid(conn)?,
            db::pds_account(conn)?,
            db::upload_progress(conn)?,
        ))
    })?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    let error = error
        .map(|e| format!("<p class=\"notice notice-error\">{}</p>\n", escape(e)))
        .unwrap_or_default();
    let body = match &account {
        Some(account) => connected_html(account, progress, &board.uploader),
        None => connect_form_html(typed),
    };
    let main = format!(
        "<h2>Back up to a Personal Data Server</h2>\n\
         <p class=\"intro\">Joyscroller can also upload what it downloads to your account on a \
         Personal Data Server, the server that holds a Bluesky (AT Protocol) account's data, \
         so the archive lives somewhere off this computer too. Each liked post becomes one record in your \
         account, with its files, title, author and link, uploaded one at a time, newest \
         first, as it downloads.</p>\n\
         <p class=\"notice\"><b>Everything uploaded is public.</b> Anyone can download the \
         videos, and anyone can read the list of what you liked: a Personal Data Server shares every record \
         with the whole network. They don't show up in anyone's Bluesky feed.</p>\n\
         <p class=\"notice\">On Bluesky's own servers (a bsky.social account), this would use \
         Bluesky's free storage as a video archive, which they may not welcome. A Personal Data \
         Server of your own, or one that invites it, is a better home for it.</p>\n\
         {error}{body}"
    );
    Ok(Html(layout(
        &Frame {
            title: "Settings",
            current: Section::Settings,
            list: None,
            status: &status,
            counts: &counts,
            board: &board,
            footer: account.as_ref().map_or_else(
                || "No Personal Data Server connected".into(),
                |a| format!("Uploading to {}", escape(&a.handle)),
            ),
            after,
            live: false,
            player: false,
        },
        &main,
    )))
}

/// The platforms' checkboxes, ticked as `ticked` says.
fn checkboxes_html(ticked: &dyn Fn(&str) -> bool) -> String {
    let mut html = String::new();
    for (name, label) in PLATFORMS {
        let checked = if ticked(name) { " checked" } else { "" };
        let _ = writeln!(
            html,
            "<label class=\"check\"><input type=\"checkbox\" name=\"{name}\"{checked}> {label}</label>"
        );
    }
    html
}

fn connect_form_html(typed: Option<&ConnectForm>) -> String {
    let service = typed.map_or("bsky.social", |t| t.service.as_str());
    let handle = typed.map_or("", |t| t.handle.as_str());
    let ticked = |name: &str| typed.is_none_or(|t| t.platforms.ticked().contains(&name));
    format!(
        "<form class=\"settings\" method=\"post\" action=\"/settings/pds\">\n\
         <fieldset class=\"group\"><legend>Connect an account</legend>\n\
         <label>Server <input type=\"text\" name=\"service\" value=\"{service}\" required \
         placeholder=\"bsky.social\" aria-describedby=\"service-hint\"></label>\n\
         <p class=\"hint\" id=\"service-hint\">Where your account lives. For an account you \
         made on bsky.app, leave it as <b>bsky.social</b>. If your account is on another \
         server, type its address, such as <b>vlpds.jazco.dev</b>.</p>\n\
         <label>Handle <input type=\"text\" name=\"handle\" value=\"{handle}\" required \
         autocomplete=\"username\" placeholder=\"you.bsky.social\" \
         aria-describedby=\"handle-hint\"></label>\n\
         <p class=\"hint\" id=\"handle-hint\">Your username, as it appears on your profile, \
         without the @: <b>you.bsky.social</b>, or your own domain if you use one.</p>\n\
         <label>App password <input type=\"password\" name=\"app_password\" required \
         autocomplete=\"off\" placeholder=\"xxxx-xxxx-xxxx-xxxx\" \
         aria-describedby=\"password-hint\"></label>\n\
         <p class=\"hint\" id=\"password-hint\">Not the password you log in with. An app \
         password is a separate one just for Joyscroller, which you can revoke any time. \
         To make one, open bsky.app and go to Settings → Privacy and security → \
         App passwords → Add App Password, name it \"Joyscroller\", leave direct messages \
         off, and paste it here. Joyscroller keeps it in this computer's keyring.</p>\n\
         <p>Upload likes from:</p>\n{checks}\
         <p class=\"hint\">Untick a platform to keep its likes off the server. You can change \
         this later.</p>\n\
         <p><button class=\"button\" type=\"submit\">Connect and start uploading</button></p>\n\
         </fieldset>\n</form>\n",
        service = escape(service),
        handle = escape(handle),
        checks = checkboxes_html(&ticked),
    )
}

fn connected_html(
    account: &db::PdsAccount,
    progress: db::UploadProgress,
    uploader: &crate::pipeline::Uploader,
) -> String {
    let limit = account
        .blob_limit_bytes
        .map(|b| format!(" It takes files up to {} MiB.", b >> 20))
        .unwrap_or_default();
    let ticked = |name: &str| account.platforms.iter().any(|p| p == name);
    let platforms = if account.platforms.is_empty() {
        "no platform's".to_string()
    } else {
        account
            .platforms
            .iter()
            .map(|p| display_name(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (pause_action, pause_label, pause_note) = if account.paused {
        (
            "resume",
            "Resume uploading",
            "Paused. Nothing uploads until you resume, which carries on where it stopped.",
        )
    } else {
        (
            "pause",
            "Pause uploading",
            "Stops the upload under way right away. Resuming carries on where it stopped.",
        )
    };
    format!(
        "<fieldset class=\"group\"><legend>Connected</legend>\n\
         <p>Uploading {platforms} likes to <b>{handle}</b> on {pds} (<code>{did}</code>).{limit}</p>\n\
         <div id=\"upload-progress\" data-refresh=\"/settings\">\n{progress}</div>\n\
         <form method=\"post\" action=\"/settings/pds/{pause_action}\">\
         <p><button class=\"button\" type=\"submit\">{pause_label}</button> \
         <span class=\"hint\">{pause_note}</span></p></form>\n\
         </fieldset>\n\
         <form class=\"settings\" method=\"post\" action=\"/settings/pds/platforms\">\n\
         <fieldset class=\"group\"><legend>Platforms</legend>\n{checks}\
         <p><button class=\"button\" type=\"submit\">Save</button></p>\n\
         </fieldset>\n</form>\n\
         <form class=\"settings\" method=\"post\" action=\"/settings/pds/disconnect\">\n\
         <fieldset class=\"group\"><legend>Disconnect</legend>\n\
         <p>Stops uploading for good and forgets the app password. What's uploaded stays on \
         the server.</p>\n\
         <p><button class=\"button\" type=\"submit\">Disconnect</button></p>\n\
         </fieldset>\n</form>\n",
        handle = escape(&account.handle),
        pds = escape(&account.pds_url),
        did = escape(&account.did),
        progress = progress_html(&account.did, progress, uploader),
        checks = checkboxes_html(&ticked),
    )
}

/// The completion bar: files uploaded of every file to upload, then what the
/// uploader is doing. The page's script fetches it again as uploads finish.
fn progress_html(did: &str, p: db::UploadProgress, uploader: &crate::pipeline::Uploader) -> String {
    let posts = |n: usize| {
        if n == 1 {
            "1 post".to_string()
        } else {
            format!("{} posts", thousands(n))
        }
    };
    let too_big = if p.too_big_posts > 0 {
        format!(
            " {} too big for this server, so not uploaded.",
            posts(p.too_big_posts)
        )
    } else {
        String::new()
    };
    let note = format!(
        "{} files uploaded ({}), {} to go ({}).{too_big} \
         <a href=\"https://pdsls.dev/at://{did}/{collection}\">See them on pdsls.dev</a>",
        thousands(p.uploaded_files),
        posts(p.uploaded_posts),
        thousands(p.left_files),
        posts(p.left_posts),
        did = escape(did),
        collection = pds::COLLECTION,
    );
    format!(
        "{}<p>{}</p>\n",
        meter_html(
            "Files uploaded",
            p.uploaded_files,
            p.uploaded_files + p.left_files,
            &note
        ),
        uploader_html(uploader, &|job| escape(&job.title), false),
    )
}
