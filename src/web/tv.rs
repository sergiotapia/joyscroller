//! /tv: the downloaded archive played as television, a channel for all
//! platforms and one for each (`crate::tv` keeps their schedules). The page
//! is a TV screen made of two of the video page's players stacked in one
//! place: one on air, the other loading the next show out of sight, so a
//! show starts the moment the last one ends. `assets/tv.js` runs it, asking
//! `/tv/<channel>/lineup` for what's on as it goes.

use std::fmt::Write as _;

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::{
    App, Failure, Frame, Section, author_label, display_name, layout, not_found, screen_html,
    short_title, video_href,
};
use crate::db;
use crate::tv::{self, Channel, Show};

/// How many shows a lineup request gets by default, and at most: what's on
/// and enough after it that the page never waits on the next request.
const LINEUP: usize = 4;
const MOST_LINEUP: usize = 10;

pub(super) async fn all_page(State(app): State<App>) -> Result<Html<String>, Failure> {
    page(&app, Channel::All)
}

pub(super) async fn channel_page(
    State(app): State<App>,
    Path(slug): Path<String>,
) -> Result<Html<String>, Failure> {
    page(&app, Channel::from_slug(&slug).ok_or_else(not_found)?)
}

fn page(app: &App, channel: Channel) -> Result<Html<String>, Failure> {
    let (counts, after) = app.db(|conn| Ok((db::video_counts(conn)?, db::last_rowid(conn)?)))?;
    let status = app.status.borrow().clone();
    let board = app.pipeline.board();
    Ok(Html(layout(
        &Frame {
            title: "TV",
            current: Section::Tv,
            list: None,
            status: &status,
            counts: &counts,
            board: &board,
            footer: format!(
                "<span id=\"tv-footer\">Channel {} · {}</span>",
                channel.number(),
                channel.name()
            ),
            after,
            live: false,
            player: true,
        },
        &tv_html(channel),
    )))
}

/// The page's TV: the screen (two stages for players, the static shown
/// while tuning, the channel's number in the corner, the power button), the
/// remote's channel buttons, and what's on and next.
fn tv_html(channel: Channel) -> String {
    let href = |c: Channel| match c {
        Channel::All => "/tv".to_owned(),
        _ => format!("/tv/{}", c.slug()),
    };
    let step = |by: usize| {
        let at = Channel::ALL.iter().position(|c| *c == channel).unwrap_or(0);
        Channel::ALL[(at + by) % Channel::ALL.len()]
    };
    let mut buttons = String::new();
    for c in Channel::ALL {
        // Writing to a String can't fail.
        let _ = writeln!(
            buttons,
            "<a class=\"tv-channel\" href=\"{href}\" data-channel=\"{slug}\"{current}>\
             <b>{number}</b>{name}</a>",
            href = href(c),
            slug = c.slug(),
            current = if c == channel {
                " aria-current=\"page\""
            } else {
                ""
            },
            number = c.number(),
            name = c.name(),
        );
    }
    format!(
        "<link rel=\"stylesheet\" href=\"/assets/tv.css\">\n\
         <div class=\"tv tuning\" id=\"tv\" data-channel=\"{slug}\">\n\
         <div class=\"tv-screen\" id=\"tv-screen\">\n\
         <div class=\"tv-stage\"></div>\n\
         <div class=\"tv-stage\" inert></div>\n\
         <div class=\"tv-glass\">\n\
         <div class=\"tv-static\" aria-hidden=\"true\"></div>\n\
         <div class=\"tv-bug\" aria-hidden=\"true\"><b>{number}</b><span>{name}</span></div>\n\
         <p class=\"tv-off\" hidden></p>\n\
         <button class=\"tv-power\" type=\"button\" hidden><span>Turn on</span></button>\n\
         </div>\n\
         </div>\n\
         <nav class=\"tv-remote\" aria-label=\"Channels\">\n\
         <a class=\"tv-step\" href=\"{down}\" data-step=\"-1\" title=\"Channel down (↓)\">CH ▼</a>\n\
         {buttons}\
         <a class=\"tv-step\" href=\"{up}\" data-step=\"1\" title=\"Channel up (↑)\">CH ▲</a>\n\
         </nav>\n\
         <dl class=\"tv-guide\" aria-live=\"polite\">\n\
         <dt>Now</dt><dd class=\"tv-now\">…</dd>\n\
         <dt>Next</dt><dd class=\"tv-next\">…</dd>\n\
         </dl>\n\
         <p class=\"tv-keys\">↑ ↓ or 1–4 change the channel · M mutes · Space pauses · F fills the screen</p>\n\
         </div>\n\
         <script type=\"module\" src=\"/assets/tv.js\"></script>\n",
        slug = channel.slug(),
        number = channel.number(),
        name = channel.name(),
        down = href(step(Channel::ALL.len() - 1)),
        up = href(step(1)),
    )
}

#[derive(Deserialize)]
pub(super) struct LineupQuery {
    /// The first show wanted, by its place on the schedule; what's on now
    /// when left out.
    from: Option<u64>,
    count: Option<usize>,
}

/// What's on `channel` now (or from `from` on), as JSON: the server's clock,
/// and each show's place and times on the schedule, what the guide says
/// about it, the gain that levels its sound, and its player.
pub(super) async fn lineup(
    State(app): State<App>,
    Path(slug): Path<String>,
    Query(query): Query<LineupQuery>,
) -> Result<Response, Failure> {
    let channel = Channel::from_slug(&slug).ok_or_else(not_found)?;
    let shows: Vec<Show> = app.db(|conn| {
        Ok(db::airable(conn)?
            .iter()
            .filter_map(Show::from_airable)
            .collect())
    })?;
    let now = tv::now();
    let count = query.count.unwrap_or(LINEUP).clamp(1, MOST_LINEUP);
    let slots = app.tv.slots(channel, shows, now, query.from, count);
    let mut lineup = Vec::with_capacity(slots.len());
    for slot in slots {
        let (platform, video_id) = (&slot.show.platform, &slot.show.video_id);
        let (found, download, playback) = app.db(|conn| {
            Ok((
                db::video(conn, platform, video_id)?,
                db::download_state(conn, platform, video_id)?,
                db::playback_state(conn, platform, video_id)?,
            ))
        })?;
        let Some((video, _)) = found else {
            continue;
        };
        let (player, playing) = screen_html(&video, download.as_ref(), playback.as_ref());
        if !playing {
            continue;
        }
        lineup.push(json!({
            "seq": slot.seq,
            "starts_at": slot.starts_at,
            "from": slot.from,
            "airtime": slot.airtime,
            "slides": slot.show.slides,
            "gain_db": slot.show.gain_db,
            "href": video_href(&video),
            "title": short_title(&video),
            "author": author_label(&video),
            "platform": display_name(&video.platform),
            "player": player,
        }));
    }
    let body = json!({ "now": now, "channel": channel.slug(), "slots": lineup }).to_string();
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Board;
    use crate::web::SyncStatus;

    #[test]
    fn tv_is_bare() {
        let page = |current| {
            layout(
                &Frame {
                    title: "TV",
                    current,
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
            )
        };
        let tv = page(Section::Tv);
        assert!(tv.contains("<div class=\"shell bare\">"));
        // Still there for the page script's clock and sync list.
        assert!(tv.contains("<aside class=\"panel\">"));
        assert!(page(Section::Downloading).contains("<div class=\"shell\">"));
    }

    #[test]
    fn the_remote_has_every_channel() {
        let html = tv_html(Channel::TikTok);
        for c in Channel::ALL {
            assert!(html.contains(&format!("data-channel=\"{}\"", c.slug())));
        }
        assert!(html.contains(
            "href=\"/tv/tiktok\" data-channel=\"tiktok\" aria-current=\"page\"><b>3</b>TikTok"
        ));
        assert!(html.contains("href=\"/tv\" data-channel=\"all\"><b>1</b>All"));
        // Up and down wrap around.
        assert!(html.contains("href=\"/tv/youtube\" data-step=\"-1\""));
        assert!(html.contains("href=\"/tv/instagram\" data-step=\"1\""));
        let all = tv_html(Channel::All);
        assert!(all.contains("href=\"/tv/instagram\" data-step=\"-1\""));
    }
}
