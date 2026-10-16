//! /tv: the downloaded archive played as television, a channel for all
//! platforms and one for each (`crate::tv` keeps their schedules). The page
//! is a television set, rendered in the clock radio's plastic
//! (assets/blender/tv.py), with its tube's face cut out: under it, two of
//! the video page's players stacked in one place, one on air and the other
//! loading the next show out of sight, so a show starts the moment the last
//! one ends, and over them the canvas the picture tube is drawn on
//! (assets/crt.js). The set's own buttons, knob and channel LCD sit where
//! the render put them, from `assets/tv.json`. `assets/tv.js` runs it,
//! asking `/tv/<channel>/lineup` for what's on as it goes; between shows
//! the station airs its bumpers (`assets/bumpers.json`).

use std::fmt::Write as _;
use std::sync::LazyLock;

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    App, Failure, Frame, Section, author_label, display_name, escape, layout, not_found,
    screen_html, short_title, video_href,
};
use crate::db;
use crate::tv::{self, Bumper, Channel, Program, Show};

/// How many shows a lineup request gets by default, and at most: what's on
/// and enough after it that the page never waits on the next request.
const LINEUP: usize = 4;
const MOST_LINEUP: usize = 10;

const SET_JSON: &str = include_str!("../../assets/tv.json");
const BUMPERS_JSON: &str = include_str!("../../assets/bumpers.json");

/// Where the set's parts are in its render (`tv.webp`), in its pixels: boxes
/// are x, y, width, height from the top left.
#[derive(Deserialize)]
struct Set {
    width: f64,
    height: f64,
    /// The set's front face, which the page lays out by: the ears above it
    /// and the shadow below hang outside.
    front: [f64; 4],
    /// The tube's face, where the picture goes, and its corners' radius.
    screen: [f64; 4],
    screen_radius: f64,
    /// `tv-glass.webp`, the face's reflections.
    glass: [f64; 4],
    lcd: [f64; 4],
    ch_down: [f64; 4],
    ch_up: [f64; 4],
    power: [f64; 4],
    led: [f64; 4],
    knob: Knob,
}

/// The VOLUME knob's sprite: `frames` turns from `from` to `to` degrees.
#[derive(Deserialize)]
struct Knob {
    r#box: [f64; 4],
    from: f64,
    to: f64,
    frames: u32,
}

/// `SET_JSON` read, or `None` (said once) if it couldn't be, which leaves
/// the page a bare screen. A test checks it reads.
static SET: LazyLock<Option<Set>> = LazyLock::new(|| {
    serde_json::from_str(SET_JSON)
        .inspect_err(|err| eprintln!("warning: assets/tv.json: {err}"))
        .ok()
});

/// One of the station's bumpers as its render describes it: its file in
/// the assets, its length, and where and when the page sets the next show's
/// name on it, if it's an "up next" card.
#[derive(Deserialize, Clone)]
struct BumperFile {
    name: String,
    file: String,
    seconds: f64,
    #[serde(default)]
    text: Option<Value>,
}

static BUMPERS: LazyLock<Vec<BumperFile>> = LazyLock::new(|| {
    serde_json::from_str(BUMPERS_JSON)
        .inspect_err(|err| eprintln!("warning: assets/bumpers.json: {err}"))
        .unwrap_or_default()
});

/// The bumpers the channels air in their breaks.
pub(super) fn bumpers() -> Vec<Bumper> {
    BUMPERS
        .iter()
        .map(|b| Bumper {
            name: b.name.clone(),
            seconds: b.seconds,
        })
        .collect()
}

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

/// `style` placing the pixel box `at` (x, y, width, height in the render)
/// over the set's front face, in percentages of it, so it scales with the
/// set.
fn place(set: &Set, at: [f64; 4]) -> String {
    let [fx, fy, fw, fh] = set.front;
    format!(
        "left:{:.3}%;top:{:.3}%;width:{:.3}%;height:{:.3}%",
        (at[0] - fx) / fw * 100.0,
        (at[1] - fy) / fh * 100.0,
        at[2] / fw * 100.0,
        at[3] / fh * 100.0,
    )
}

/// The set: the tube's face (two stages for players, the canvas the tube
/// draws on, and over the glass the fallback's static and channel number,
/// what's wrong if nothing can air, and the prompt to turn it on), the
/// render over it, the glass's reflections, the channel LCD, and its
/// buttons and knob.
fn set_html(channel: Channel) -> String {
    let screen_inside = format!(
        "<div class=\"tv-stage\"></div>\n\
         <div class=\"tv-stage\" inert></div>\n\
         <canvas class=\"tv-crt\" aria-hidden=\"true\"></canvas>\n\
         <div class=\"tv-glass\">\n\
         <div class=\"tv-static\" aria-hidden=\"true\"></div>\n\
         <div class=\"tv-bug\" aria-hidden=\"true\"><b>{number}</b><span>{name}</span></div>\n\
         <p class=\"tv-off\" hidden></p>\n\
         <button class=\"tv-wake\" type=\"button\" hidden>Press <b>Power</b> to turn it on</button>\n\
         </div>\n",
        number = channel.number(),
        name = channel.name(),
    );
    let Some(set) = &*SET else {
        return format!(
            "<div class=\"tv-set bare\" id=\"tv-set\">\n\
             <div class=\"tv-screen\" id=\"tv-screen\" data-radius=\"0.02\">\n{screen_inside}</div>\n</div>\n"
        );
    };
    let knob = &set.knob;
    format!(
        "<div class=\"tv-set\" id=\"tv-set\" style=\"--set-ratio:{ratio:.5}\">\n\
         <div class=\"tv-screen\" id=\"tv-screen\" style=\"{screen}\" data-radius=\"{radius:.5}\">\n{screen_inside}</div>\n\
         <img class=\"tv-body\" src=\"/assets/tv.webp\" alt=\"\" style=\"{body}\">\n\
         <img class=\"tv-glare\" src=\"/assets/tv-glass.webp\" alt=\"\" style=\"{glass}\">\n\
         <div class=\"tv-lcd\" style=\"{lcd}\" aria-hidden=\"true\"><span class=\"tv-lcd-unlit\">88</span><span class=\"tv-lcd-lit\"></span></div>\n\
         <button class=\"tv-key tv-ch-down\" type=\"button\" data-step=\"-1\" style=\"{down}\" title=\"Channel down (↓)\"><span>Channel down</span></button>\n\
         <button class=\"tv-key tv-ch-up\" type=\"button\" data-step=\"1\" style=\"{up}\" title=\"Channel up (↑)\"><span>Channel up</span></button>\n\
         <div class=\"tv-knob\" role=\"slider\" tabindex=\"0\" aria-label=\"Volume\" aria-valuemin=\"0\" \
         aria-valuemax=\"100\" aria-valuenow=\"100\" title=\"Volume (+ −)\" data-from=\"{from}\" data-to=\"{to}\" \
         data-frames=\"{frames}\" style=\"{knob_at}\"></div>\n\
         <button class=\"tv-key tv-power-key\" type=\"button\" aria-pressed=\"false\" style=\"{power}\" title=\"Power (P)\"><span>Power</span></button>\n\
         <span class=\"tv-led\" style=\"{led}\" aria-hidden=\"true\"></span>\n\
         </div>\n",
        ratio = set.front[2] / set.front[3],
        screen = place(set, set.screen),
        radius = set.screen_radius / set.screen[2],
        body = place(set, [0.0, 0.0, set.width, set.height]),
        glass = place(set, set.glass),
        lcd = place(set, set.lcd),
        down = place(set, set.ch_down),
        up = place(set, set.ch_up),
        from = knob.from,
        to = knob.to,
        frames = knob.frames,
        knob_at = place(set, knob.r#box),
        power = place(set, set.power),
        led = place(set, set.led),
    )
}

/// The page's TV: the set, then beside it the remote's channel buttons,
/// what's on and next, and the keys.
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
         <div class=\"tv off tuning\" id=\"tv\" data-channel=\"{slug}\">\n\
         {set}\
         <div class=\"tv-side\">\n\
         <nav class=\"tv-remote\" aria-label=\"Channels\">\n\
         <a class=\"tv-step\" href=\"{down}\" data-step=\"-1\" title=\"Channel down (↓)\">CH ▼</a>\n\
         {buttons}\
         <a class=\"tv-step\" href=\"{up}\" data-step=\"1\" title=\"Channel up (↑)\">CH ▲</a>\n\
         </nav>\n\
         <dl class=\"tv-guide\" aria-live=\"polite\">\n\
         <dt>Now</dt><dd class=\"tv-now\">…</dd>\n\
         <dt>Next</dt><dd class=\"tv-next\">…</dd>\n\
         </dl>\n\
         <p class=\"tv-keys\">↑ ↓ or 1–4 change the channel · + − turn the volume · M mutes · \
         P switches the set off and on · F fills the screen</p>\n\
         </div>\n\
         </div>\n\
         <script type=\"module\" src=\"/assets/tv.js\"></script>\n",
        set = set_html(channel),
        slug = channel.slug(),
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

/// A bumper's player: its clip, in the same markup the video page's
/// player has, so tv.js treats it as any other show.
fn bumper_player(file: &str) -> String {
    format!(
        "<div class=\"player\"><div class=\"player-screen\">\
         <video src=\"/assets/{}\" preload=\"auto\" playsinline></video>\
         <p class=\"player-error\" hidden></p></div></div>",
        escape(file)
    )
}

/// What's on `channel` now (or from `from` on), as JSON: the server's clock,
/// and each show's place and times on the schedule, what the guide says
/// about it, the gain that levels its sound, and its player. A break says
/// which bumper airs and, for an "up next" card, what follows it.
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
    // One past what's asked for, so a break at the end can say what's next.
    let slots = app.tv.slots(channel, shows, now, query.from, count + 1);
    let mut lineup: Vec<Value> = Vec::with_capacity(slots.len());
    for slot in slots {
        let show = match &slot.program {
            Program::Show(show) => show,
            Program::Break(bumper) => {
                let Some(file) = BUMPERS.iter().find(|b| b.name == bumper.name) else {
                    continue;
                };
                lineup.push(json!({
                    "seq": slot.seq,
                    "starts_at": slot.starts_at,
                    "from": 0.0,
                    "airtime": slot.airtime,
                    "slides": 0,
                    "gain_db": 0.0,
                    "href": null,
                    "title": "Joyscroller TV",
                    "author": null,
                    "platform": "station break",
                    "bumper": { "name": file.name, "text": file.text },
                    "player": bumper_player(&file.file),
                }));
                continue;
            }
        };
        let (platform, video_id) = (&show.platform, &show.video_id);
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
            "slides": show.slides,
            "gain_db": show.gain_db,
            "href": video_href(&video),
            "title": short_title(&video),
            "author": author_label(&video),
            "platform": display_name(&video.platform),
            "player": player,
        }));
    }
    // A break's card names the show after it.
    for at in 0..lineup.len().saturating_sub(1) {
        if lineup[at].get("bumper").is_some() && lineup[at + 1].get("bumper").is_none() {
            let next = json!({
                "title": lineup[at + 1]["title"],
                "author": lineup[at + 1]["author"],
                "platform": lineup[at + 1]["platform"],
            });
            lineup[at]["next"] = next;
        }
    }
    lineup.truncate(count);
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

    #[test]
    fn the_set_reads_and_its_parts_are_on_it() {
        let set = SET.as_ref().expect("assets/tv.json reads");
        let inside = |[x, y, w, h]: [f64; 4]| {
            x >= 0.0 && y >= 0.0 && x + w <= set.width && y + h <= set.height
        };
        for part in [
            set.front,
            set.screen,
            set.glass,
            set.lcd,
            set.ch_down,
            set.ch_up,
            set.power,
            set.led,
            set.knob.r#box,
        ] {
            assert!(inside(part), "{part:?}");
        }
        // A 4:3 tube, as the canvas draws it.
        let [_, _, w, h] = set.screen;
        assert!((w / h - 4.0 / 3.0).abs() < 0.01, "{}", w / h);
        assert!(set.knob.frames > 1 && set.knob.from < set.knob.to);
        let html = set_html(Channel::All);
        assert!(html.contains("class=\"tv-body\""));
        assert!(html.contains(&format!("data-frames=\"{}\"", set.knob.frames)));
    }

    #[test]
    fn bumpers_read_and_are_built_in() {
        assert!(!BUMPERS.is_empty());
        for bumper in BUMPERS.iter() {
            assert!(
                bumper.seconds > 1.0 && bumper.seconds < 15.0,
                "{}",
                bumper.name
            );
            assert!(
                super::super::ASSETS
                    .iter()
                    .any(|(file, kind, _)| *file == bumper.file && kind.starts_with("video/")),
                "{} isn't in ASSETS",
                bumper.file
            );
        }
        assert_eq!(bumpers().len(), BUMPERS.len());
    }
}
