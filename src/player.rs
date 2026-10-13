//! The player on a video's page, once its media is downloaded: Video.js 10's
//! elements around the saved files. A video gets a screen with a strip of
//! controls under it; a TikTok photo post gets a slideshow of its pictures
//! over the same strip, playing the post's sound, which starts by itself and
//! loops as it does on TikTok. Video.js is built into `assets/videojs.js`
//! (see `assets/videojs/`), `assets/player.css` dresses it in the clock
//! radio's plastic, and `assets/player.js` runs the slideshow.

use std::fmt::Write;

use crate::web::escape;

/// What a download saved that the page can play, as file names in its folder.
#[derive(Debug, PartialEq, Eq)]
pub enum Playable<'a> {
    Video {
        video: &'a str,
        poster: Option<&'a str>,
    },
    Photos {
        images: Vec<&'a str>,
        sound: Option<&'a str>,
    },
}

/// Sort a download's files (see `download.rs` for their names) into
/// something to play, if there's anything.
pub fn playable(files: &[String]) -> Option<Playable<'_>> {
    let named = |prefix: &str| {
        files
            .iter()
            .map(String::as_str)
            .find(|file| file.starts_with(prefix))
    };
    if let Some(video) = named("video.") {
        return Some(Playable::Video {
            video,
            poster: named("thumbnail."),
        });
    }
    // The download names them image-01, image-02, …, and the list is sorted.
    let images: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .filter(|file| file.starts_with("image-"))
        .collect();
    if images.is_empty() {
        return None;
    }
    Some(Playable::Photos {
        images,
        sound: named("sound."),
    })
}

/// The player for `playable`, whose files are served under `base`.
pub fn player_html(base: &str, playable: &Playable) -> String {
    let src = |file: &str| format!("{base}/{}", escape(file));
    match playable {
        Playable::Video { video, poster } => {
            let poster = poster
                .map(|file| {
                    format!(
                        "<media-poster class=\"player-poster\"><img src=\"{}\" alt=\"\"></media-poster>\n",
                        src(file)
                    )
                })
                .unwrap_or_default();
            format!(
                "<video-player>\n\
                 <media-container class=\"player\">\n\
                 <div class=\"player-screen\">\n\
                 <video src=\"{video}\" preload=\"metadata\" playsinline></video>\n\
                 {poster}\
                 <media-buffering-indicator class=\"player-busy\"><media-icon name=\"spinner\"></media-icon></media-buffering-indicator>\n\
                 <media-play-button class=\"player-start\"><media-icon name=\"play\"></media-icon></media-play-button>\n\
                 {error}\
                 </div>\n\
                 <media-gesture type=\"tap\" action=\"togglePaused\"></media-gesture>\n\
                 <media-gesture type=\"doubletap\" action=\"toggleFullscreen\"></media-gesture>\n\
                 {hotkeys}\
                 <media-hotkey keys=\"f\" action=\"toggleFullscreen\" target=\"document\"></media-hotkey>\n\
                 <media-hotkey keys=\"ArrowLeft\" action=\"seekStep\" target=\"document\"></media-hotkey>\n\
                 <media-hotkey keys=\"ArrowRight\" action=\"seekStep\" target=\"document\"></media-hotkey>\n\
                 <media-controls>\n<media-controls-content class=\"player-strip\" data-interactive>\n\
                 {controls}\
                 <media-fullscreen-button class=\"player-button\">\
                 <media-icon name=\"fullscreen-enter\" class=\"when-windowed\"></media-icon>\
                 <media-icon name=\"fullscreen-exit\" class=\"when-fullscreen\"></media-icon>\
                 </media-fullscreen-button>\n\
                 </media-controls-content>\n</media-controls>\n\
                 </media-container>\n\
                 </video-player>\n",
                video = src(video),
                error = cant_play(&src(video)),
                hotkeys = HOTKEYS,
                controls = CONTROLS,
            )
        }
        Playable::Photos { images, sound } => {
            let count = images.len();
            let mut slides = String::new();
            let mut dots = String::new();
            for (i, file) in images.iter().enumerate() {
                let n = i + 1;
                // The rest load as the browser gets to them, after the first.
                let (lazy, current) = if n == 1 {
                    ("", " aria-current=\"true\"")
                } else {
                    (" loading=\"lazy\"", "")
                };
                // Writing to a String can't fail.
                let _ = write!(
                    slides,
                    "<img src=\"{}\" alt=\"Photo {n} of {count}\" draggable=\"false\"{lazy}>",
                    src(file)
                );
                let _ = write!(
                    dots,
                    "<button class=\"slide-dot\" type=\"button\" aria-label=\"Photo {n}\"{current}></button>"
                );
            }
            let slideshow = format!(
                "<div class=\"player-screen slideshow\" role=\"region\" aria-roledescription=\"carousel\" aria-label=\"Photos\">\n\
                 <div class=\"slides\">{slides}</div>\n\
                 <button class=\"slide-turn slide-prev\" type=\"button\" aria-label=\"Previous photo\" disabled>{ARROW}</button>\n\
                 <button class=\"slide-turn slide-next\" type=\"button\" aria-label=\"Next photo\"{last}>{ARROW}</button>\n\
                 <div class=\"slide-count\" aria-live=\"polite\"><span class=\"slide-at\">1</span>/{count}</div>\n\
                 <div class=\"slide-dots\">{dots}</div>\n\
                 </div>\n",
                last = if count == 1 { " disabled" } else { "" },
            );
            let Some(sound) = sound else {
                return format!("<div class=\"player\">\n{slideshow}</div>\n");
            };
            format!(
                "<audio-player>\n\
                 <media-container class=\"player\">\n\
                 {slideshow}\
                 <audio src=\"{sound}\" preload=\"auto\" autoplay loop></audio>\n\
                 {error}\
                 {hotkeys}\
                 <div class=\"player-strip\">\n{controls}</div>\n\
                 </media-container>\n\
                 </audio-player>\n",
                sound = src(sound),
                error = cant_play(&src(sound)),
                hotkeys = HOTKEYS,
                controls = CONTROLS,
            )
        }
    }
}

/// Shown by `player.js` if the browser can't play the file. TikTok's videos
/// are HEVC, which Chromium plays only where the GPU can decode it.
fn cant_play(href: &str) -> String {
    format!(
        "<p class=\"player-error\" hidden>This browser can't play this file. \
         <a href=\"{href}\">Open it</a> to play it in another app.</p>\n"
    )
}

/// The shortcuts both players share. They work anywhere on the page, as on
/// YouTube, since the player is the only thing on it to control.
const HOTKEYS: &str = "\
<media-hotkey keys=\"Space\" action=\"togglePaused\" target=\"document\"></media-hotkey>\n\
<media-hotkey keys=\"k\" action=\"togglePaused\" target=\"document\"></media-hotkey>\n\
<media-hotkey keys=\"m\" action=\"toggleMuted\" target=\"document\"></media-hotkey>\n";

/// The strip's controls both players share: play, seek, the time on an LCD,
/// mute and volume. Video.js marks each with its state (`data-paused`,
/// `data-volume-level`, …), which `player.css` uses to pick the icon.
const CONTROLS: &str = "\
<media-play-button class=\"player-button player-play\">\
<media-icon name=\"play\" class=\"when-paused\"></media-icon>\
<media-icon name=\"pause\" class=\"when-playing\"></media-icon>\
<media-icon name=\"restart\" class=\"when-ended\"></media-icon>\
</media-play-button>\n\
<media-time-slider class=\"player-seek\">\
<media-slider-track><media-slider-buffer></media-slider-buffer><media-slider-fill></media-slider-fill></media-slider-track>\
<media-slider-thumb></media-slider-thumb>\
<media-slider-preview overflow=\"clamp\"><media-slider-value type=\"pointer\"></media-slider-value></media-slider-preview>\
</media-time-slider>\n\
<media-time-group class=\"player-lcd\">\
<media-time type=\"current\"></media-time><media-time-separator>/</media-time-separator><media-time type=\"duration\"></media-time>\
</media-time-group>\n\
<media-mute-button class=\"player-button\">\
<media-icon name=\"volume-off\" class=\"when-off\"></media-icon>\
<media-icon name=\"volume-low\" class=\"when-low\"></media-icon>\
<media-icon name=\"volume-high\" class=\"when-high\"></media-icon>\
</media-mute-button>\n\
<media-volume-slider class=\"player-volume\">\
<media-slider-track><media-slider-fill></media-slider-fill></media-slider-track><media-slider-thumb></media-slider-thumb>\
</media-volume-slider>\n";

/// The slideshow's turn buttons, a chevron pointing right; `player.css`
/// flips it for the previous one.
const ARROW: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\">\
<path d=\"M9 4.5 16.5 12 9 19.5\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"3.5\" \
stroke-linecap=\"round\" stroke-linejoin=\"round\"/></svg>";

#[cfg(test)]
mod tests {
    use super::*;

    fn names(files: &[&str]) -> Vec<String> {
        files.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn finds_what_to_play() {
        let video = names(&["thumbnail.webp", "video.webm"]);
        assert_eq!(
            playable(&video),
            Some(Playable::Video {
                video: "video.webm",
                poster: Some("thumbnail.webp")
            })
        );
        let photos = names(&["image-01.jpg", "image-02.webp", "sound.mp3"]);
        assert_eq!(
            playable(&photos),
            Some(Playable::Photos {
                images: vec!["image-01.jpg", "image-02.webp"],
                sound: Some("sound.mp3")
            })
        );
        assert_eq!(playable(&names(&["thumbnail.jpg"])), None);
        assert_eq!(playable(&[]), None);
    }

    #[test]
    fn a_video_gets_a_screen_and_controls() {
        let html = player_html(
            "/videos/youtube/abc/media",
            &Playable::Video {
                video: "video.webm",
                poster: Some("thumbnail.webp"),
            },
        );
        assert!(html.contains("<video src=\"/videos/youtube/abc/media/video.webm\""));
        assert!(html.contains("<img src=\"/videos/youtube/abc/media/thumbnail.webp\""));
        assert!(html.contains("<media-fullscreen-button"));
        assert!(html.contains("<media-time-slider"));
        assert!(!html.contains("autoplay"));
    }

    #[test]
    fn photos_get_a_slideshow_and_their_sound() {
        let html = player_html(
            "/videos/tiktok/1/media",
            &Playable::Photos {
                images: vec!["image-01.jpg", "image-02.jpg"],
                sound: Some("sound.mp3"),
            },
        );
        assert!(html.starts_with("<audio-player>"));
        assert!(html.contains(
            "<audio src=\"/videos/tiktok/1/media/sound.mp3\" preload=\"auto\" autoplay loop>"
        ));
        assert!(html.contains("alt=\"Photo 2 of 2\" draggable=\"false\" loading=\"lazy\""));
        assert!(html.contains("<span class=\"slide-at\">1</span>/2"));
        assert_eq!(html.matches("class=\"slide-dot\"").count(), 2);
        assert!(!html.contains("<media-fullscreen-button"));

        let silent = player_html(
            "/m",
            &Playable::Photos {
                images: vec!["image-01.jpg"],
                sound: None,
            },
        );
        assert!(!silent.contains("<audio"));
        assert!(silent.contains("aria-label=\"Next photo\" disabled"));
    }

    #[test]
    fn file_names_are_escaped() {
        let html = player_html(
            "/m",
            &Playable::Video {
                video: "video.\"><b>",
                poster: None,
            },
        );
        assert!(html.contains("/m/video.&quot;&gt;&lt;b&gt;"));
        assert!(!html.contains("<media-poster"));
    }
}
