//! How loud a downloaded post's sound is, so /tv can play every platform at
//! one level. TikTok, YouTube and Instagram mix their posts very
//! differently: across a sample of the archive, integrated loudness ran
//! from -38 to -5 LUFS (Instagram's sit near -14, the others anywhere), a
//! jump of up to 33 dB between one post and the next. ffmpeg's `ebur128`
//! filter measures each file's integrated loudness and true peak (EBU R128,
//! the broadcast standard), and `gain_db` turns that into the gain the TV
//! page applies through Web Audio, ahead of a limiter that catches the
//! peaks of quiet posts it lifts, as a broadcaster's chain does. Measured
//! across the archive, the gain alone brought 86% of posts within 2 LU of
//! the target; letting the limiter take up to 3 dB off peaks, 93%.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::db::Loudness;

/// The level everything is brought to, in LUFS. Streaming services play at
/// -14; a little under that leaves room to lift quiet posts without
/// limiting them.
const TARGET_LUFS: f64 = -16.0;

/// Where the TV's limiter (`assets/tv.js`) holds peaks, in dBTP, and how
/// far past it a lifted post's true peak may go for the limiter to bring
/// down. More would squash the post audibly.
const PEAK_CEILING_DBTP: f64 = -1.0;
const MOST_LIMITING_DB: f64 = 3.0;

/// How far a post is ever turned down or up, in dB. Turning a nearly silent
/// post up by 40 dB would only bring up its noise.
const MOST_CUT_DB: f64 = -20.0;
const MOST_BOOST_DB: f64 = 12.0;

/// What ebur128 reports for silence: its integration gate's floor.
const SILENCE_LUFS: f64 = -70.0;

/// The file whose sound airs: a video's, or a photo post's sound. None for
/// a photo post without one.
pub fn sound_file(files: &[String]) -> Option<&str> {
    let named = |prefix: &str| {
        files
            .iter()
            .map(String::as_str)
            .find(|file| file.starts_with(prefix))
    };
    named("video.").or_else(|| named("sound."))
}

/// Measure `files`' sound, in `dir`. A post without a sound file, or whose
/// file has no sound track, measures as None.
pub async fn measure(dir: &Path, files: &[String]) -> Result<Loudness> {
    const NONE: Loudness = Loudness {
        integrated_lufs: None,
        true_peak_dbtp: None,
    };
    let Some(file) = sound_file(files) else {
        return Ok(NONE);
    };
    let path = dir.join(file);
    // Only the sound is decoded, at a few hundred times real time.
    let mut cmd = crate::tools::ffmpeg_in_background()?;
    cmd.args(["-nostdin", "-hide_banner", "-nostats", "-i"])
        .arg(&path)
        .args([
            "-map",
            "0:a:0",
            "-af",
            "ebur128=peak=true:framelog=quiet",
            "-f",
            "null",
            "-",
        ]);
    let out = crate::tools::output(&mut cmd)
        .await
        .context("failed to run ffmpeg; is it installed?")?;
    let log = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if log.contains("matches no streams") {
            return Ok(NONE);
        }
        bail!(
            "ffmpeg couldn't measure {}: {}",
            path.display(),
            log.lines().last().unwrap_or("").trim()
        );
    }
    parse_summary(&log)
}

/// Read the summary ebur128 logs once the file ends:
///
/// ```text
///   Integrated loudness:
///     I:         -28.7 LUFS
///   ...
///   True peak:
///     Peak:       -8.6 dBFS
/// ```
fn parse_summary(log: &str) -> Result<Loudness> {
    let summary = log
        .rsplit_once("Summary:")
        .map(|(_, summary)| summary)
        .context("ffmpeg's ebur128 gave no summary")?;
    let value = |label: &str| -> Result<f64> {
        let line = summary
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(label))
            .with_context(|| format!("ebur128's summary has no {label}"))?;
        let number = line[label.len()..]
            .split_whitespace()
            .next()
            .with_context(|| format!("ebur128's {label} has no value"))?;
        number
            .parse()
            .with_context(|| format!("ebur128's {label} isn't a number: {number}"))
    };
    let integrated = value("I:")?;
    if integrated <= SILENCE_LUFS {
        return Ok(Loudness {
            integrated_lufs: None,
            true_peak_dbtp: None,
        });
    }
    // ffmpeg calls the true peak dBFS, measured on the oversampled signal.
    let peak = value("Peak:")?;
    Ok(Loudness {
        integrated_lufs: Some(integrated),
        true_peak_dbtp: peak.is_finite().then_some(peak),
    })
}

/// The gain, in dB, that brings a post measured at `lufs` with a true peak
/// of `peak` to `TARGET_LUFS`, lifting it no further than the limiter can
/// hold its peak.
/// Not measured yet, or silent: left as it is.
pub fn gain_db(lufs: Option<f64>, peak: Option<f64>) -> f64 {
    let Some(lufs) = lufs else {
        return 0.0;
    };
    let mut gain = (TARGET_LUFS - lufs).clamp(MOST_CUT_DB, MOST_BOOST_DB);
    if gain > 0.0
        && let Some(peak) = peak
    {
        gain = gain.min((PEAK_CEILING_DBTP + MOST_LIMITING_DB - peak).max(0.0));
    }
    gain
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
[Parsed_ebur128_0 @ 0x7de4d8003d40] t: 2.9   TARGET:-23 LUFS    M: -30.1 S:-120.7     I: -29.5 LUFS\n\
[Parsed_ebur128_0 @ 0x7de4d8003d40] Summary:\n\
\n\
  Integrated loudness:\n\
    I:         -28.7 LUFS\n\
    Threshold: -39.1 LUFS\n\
\n\
  Loudness range:\n\
    LRA:         5.9 LU\n\
    Threshold: -49.1 LUFS\n\
    LRA low:   -31.9 LUFS\n\
    LRA high:  -26.1 LUFS\n\
\n\
  True peak:\n\
    Peak:       -8.6 dBFS\n\
[out#0/null @ 0x61557f4aa180] video:0KiB audio:43528KiB\n";

    #[test]
    fn reads_the_summary_not_the_running_values() {
        assert_eq!(
            parse_summary(LOG).unwrap(),
            Loudness {
                integrated_lufs: Some(-28.7),
                true_peak_dbtp: Some(-8.6),
            }
        );
        assert!(parse_summary("no summary here").is_err());
    }

    #[test]
    fn silence_has_no_loudness() {
        let silent = LOG
            .replace("I:         -28.7 LUFS", "I:         -70.0 LUFS")
            .replace("-8.6 dBFS", "-inf dBFS");
        assert_eq!(
            parse_summary(&silent).unwrap(),
            Loudness {
                integrated_lufs: None,
                true_peak_dbtp: None,
            }
        );
    }

    #[test]
    fn finds_the_sound_that_airs() {
        let files = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            sound_file(&files(&["thumbnail.jpg", "video.mp4"])),
            Some("video.mp4")
        );
        assert_eq!(
            sound_file(&files(&["image-01.jpg", "sound.m4a"])),
            Some("sound.m4a")
        );
        assert_eq!(sound_file(&files(&["image-01.jpg", "video-02.mp4"])), None);
    }

    #[test]
    fn brings_posts_to_the_target() {
        // A loud TikTok comes down; Instagram's -14 a little.
        assert!((gain_db(Some(-4.7), Some(0.5)) - -11.3).abs() < 1e-9);
        assert!((gain_db(Some(-14.0), Some(-1.0)) - -2.0).abs() < 1e-9);
        // A quiet one goes up as far as the limiter can hold its peak.
        assert!((gain_db(Some(-24.0), Some(-10.0)) - 8.0).abs() < 1e-9);
        assert!((gain_db(Some(-24.0), Some(-4.0)) - 6.0).abs() < 1e-9);
        assert!((gain_db(Some(-24.0), Some(0.3)) - 1.7).abs() < 1e-9);
        // Already past what the limiter takes: not lifted at all.
        assert!(gain_db(Some(-24.0), Some(3.0)).abs() < 1e-9);
        // Nearly silent: lifted at most 12 dB; deafening: cut at most 20.
        assert!((gain_db(Some(-50.0), Some(-30.0)) - 12.0).abs() < 1e-9);
        assert!((gain_db(Some(10.0), None) - -20.0).abs() < 1e-9);
        // Not measured: as it is.
        assert!(gain_db(None, None).abs() < 1e-9);
    }
}
