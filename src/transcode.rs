//! Makes a playback copy of a downloaded video, `playback.mp4`, next to the
//! original, for the browsers and devices that can't play the original.
//!
//! The original stays exactly as downloaded, at the best quality on offer.
//! That's usually AV1 (YouTube), HEVC (TikTok) or VP9 (Instagram), and none
//! of those plays everywhere: Safari plays AV1 only with a hardware decoder
//! (iPhone 15 Pro, M3 Macs and newer), Chromium and Firefox play HEVC only
//! where the GPU decodes it, and a Tesla decodes AV1 in software and
//! stutters. H.264 and AAC in MP4 play in every browser, so the copy is that,
//! at the original's resolution and frame rate: it changes the codec, never
//! the picture. Re-encoding to AV1 to save space was measured and doesn't
//! help: the originals are already about as small as the encoders get them.
//!
//! A video that already plays everywhere (H.264 and AAC in MP4) gets no copy.
//! One whose picture is H.264 but whose container or sound isn't (YouTube's
//! H.264 with Opus in `.mkv`) gets its picture copied as it is and only its
//! sound converted.
//!
//! Only the app's pipeline runs this, so to try it, run a scratch copy of the
//! app (AGENTS.md, "Trying the whole app") and watch /downloading. To try an
//! encode setting by hand, use a clip of a few seconds (`ffmpeg -t 5`), not a
//! whole video: encodes run on the user's own machine.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// The copy's file name, in the video's folder.
pub const PLAYBACK_FILE: &str = "playback.mp4";

/// What the copy is written as until it's finished. Hidden, never served,
/// and overwritten by the next try if the app stops halfway.
const PARTIAL_FILE: &str = ".playback.mp4.partial";

/// x264's quality and speed. CRF 23 is its default; 20 looks a little better
/// (a VMAF of 93 to 99 against the originals in testing) for a bigger file,
/// and the files' size isn't a concern. `medium` was as fast as SVT-AV1's
/// preset 8 in testing: about 2 CPU-minutes per minute of 1080p30 video.
const CRF: &str = "20";
const PRESET: &str = "medium";

/// The copy's bitrate ceiling at 1080p, in kbit/s, so a busy scene doesn't
/// spike past what a phone or a car can stream. It scales with the picture's
/// pixel count (40 Mbit/s at 4K), with a floor for small videos.
const MAX_KBPS_1080P: u64 = 10_000;
const MIN_MAX_KBPS: u64 = 2_000;

/// AAC for sound that has to be converted (YouTube's Opus).
const AUDIO_BITRATE: &str = "160k";

/// HDR (PQ or HLG) mapped down to SDR BT.709. An HDR picture squeezed into
/// 8-bit without this looks washed out, and H.264 HDR plays almost nowhere.
const TONEMAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
     tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

/// What ffprobe says about a video file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Probe {
    /// The file's extension, lowercased: `mp4`, `webm`, `mkv`.
    pub container: String,
    pub video: Option<Picture>,
    pub audio: Option<Sound>,
    /// Seconds.
    pub duration: Option<f64>,
    /// The whole file's, in bits per second.
    pub bit_rate: Option<u64>,
}

/// The file's (first) video stream.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Picture {
    pub codec: String,
    pub profile: Option<String>,
    /// As ffprobe gives it: H.264's `level_idc` (40 for 4.0), HEVC's times 30
    /// (150 for 5.0), AV1's `seq_level_idx` (8 for 4.0).
    pub level: Option<i64>,
    pub pix_fmt: String,
    pub width: u64,
    pub height: u64,
    pub fps: Option<f64>,
    pub color_transfer: Option<String>,
    pub bit_rate: Option<u64>,
}

/// The file's (first) audio stream.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sound {
    pub codec: String,
    pub channels: Option<u64>,
}

/// What a video needs to play everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Nothing: the original plays everywhere already.
    Ready,
    Make {
        picture: Video,
        sound: Audio,
    },
}

/// What happens to the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Video {
    /// Copied as it is, already H.264.
    Copy,
    /// Re-encoded to H.264.
    Encode,
}

/// What happens to the sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    /// There's none.
    None,
    Copy,
    /// Converted to AAC, and to stereo when `downmix`.
    Aac {
        downmix: bool,
    },
}

impl Plan {
    /// How the copy is made, as the database stores it.
    pub fn method(self) -> &'static str {
        match self {
            Plan::Ready => "none",
            Plan::Make {
                picture: Video::Copy,
                ..
            } => "remux",
            Plan::Make {
                picture: Video::Encode,
                ..
            } => "encode",
        }
    }
}

/// Decide what `probe`'s file needs to play everywhere.
pub fn plan(probe: &Probe) -> Result<Plan> {
    let Some(video) = &probe.video else {
        bail!("the file has no picture");
    };
    let sound = match &probe.audio {
        None => Audio::None,
        // MP3 plays everywhere too, though no platform sends it in a video.
        Some(audio) if matches!(audio.codec.as_str(), "aac" | "mp3") => Audio::Copy,
        Some(audio) => Audio::Aac {
            downmix: audio.channels.is_some_and(|c| c > 2),
        },
    };
    let picture_plays = video.codec == "h264"
        && matches!(video.pix_fmt.as_str(), "yuv420p" | "yuvj420p")
        && !is_hdr(video);
    let container_plays = matches!(probe.container.as_str(), "mp4" | "m4v");
    Ok(match (picture_plays, sound) {
        (true, Audio::None | Audio::Copy) if container_plays => Plan::Ready,
        (true, _) => Plan::Make {
            picture: Video::Copy,
            sound,
        },
        (false, _) => Plan::Make {
            picture: Video::Encode,
            sound,
        },
    })
}

fn is_hdr(video: &Picture) -> bool {
    matches!(
        video.color_transfer.as_deref(),
        Some("smpte2084" | "arib-std-b67")
    )
}

/// What `make` did.
#[derive(Debug, Clone, PartialEq)]
pub struct Made {
    /// The copy's file name in the folder; None when the original plays
    /// everywhere, so there's no copy.
    pub file: Option<&'static str>,
    pub bytes: u64,
    /// `Plan::method`.
    pub method: &'static str,
    /// The original's `source_config`, when there's a copy to choose instead.
    pub source: Option<String>,
}

/// How far ffmpeg is through a video.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Seconds of the video done.
    pub done: f64,
    /// The video's length in seconds, when known.
    pub total: Option<f64>,
    /// How many times faster than real time it's going.
    pub speed: Option<f64>,
}

/// Make the playback copy of the video in `dir`, whose files are `files`,
/// if it needs one. `progress` hears from ffmpeg about once a second.
pub async fn make(
    dir: &Path,
    files: &[String],
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Made> {
    let original = files
        .iter()
        .find(|file| file.starts_with("video."))
        .context("the download has no video file")?;
    let input = dir.join(original);
    let container = Path::new(original)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let probe = probe(&input, container).await?;
    let made_plan = plan(&probe)?;
    let Plan::Make { picture, sound } = made_plan else {
        return Ok(Made {
            file: None,
            bytes: 0,
            method: made_plan.method(),
            source: None,
        });
    };
    let partial = dir.join(PARTIAL_FILE);
    let result = run_ffmpeg(
        ffmpeg_args(picture, sound, &probe, &input, &partial),
        probe.duration,
        progress,
    )
    .await;
    if let Err(e) = result {
        if let Err(cleanup) = std::fs::remove_file(&partial)
            && cleanup.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("warning: couldn't remove {}: {cleanup}", partial.display());
        }
        return Err(e);
    }
    let output = dir.join(PLAYBACK_FILE);
    std::fs::rename(&partial, &output)
        .with_context(|| format!("moving the playback copy into {}", output.display()))?;
    let bytes = std::fs::metadata(&output)
        .with_context(|| format!("reading {}", output.display()))?
        .len();
    Ok(Made {
        file: Some(PLAYBACK_FILE),
        bytes,
        method: made_plan.method(),
        source: source_config(&probe).map(|config| config.to_string()),
    })
}

/// Ask ffprobe about the file at `path`, whose extension is `container`.
async fn probe(path: &Path, container: &str) -> Result<Probe> {
    let mut cmd = crate::tools::ffprobe()?;
    cmd.args(["-v", "error", "-of", "json", "-show_entries"])
        .arg(
            "format=duration,bit_rate:stream=codec_type,codec_name,profile,level,pix_fmt,\
             width,height,avg_frame_rate,r_frame_rate,color_transfer,channels,bit_rate:\
             stream_disposition=attached_pic",
        )
        .arg(path);
    let out = crate::tools::output(&mut cmd)
        .await
        .context("failed to run ffprobe; is it installed?")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout), container)
}

/// Read ffprobe's JSON (`-of json`) about a file whose extension is
/// `container`. Numbers come as JSON numbers or as strings, depending on
/// the field.
fn parse_probe(text: &str, container: &str) -> Result<Probe> {
    let value: Value = serde_json::from_str(text).context("ffprobe's output isn't JSON")?;
    let float = |v: &Value| {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
        .filter(|n: &f64| n.is_finite() && *n > 0.0)
    };
    let whole = |v: &Value| match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    };
    let text = |v: &Value| v.as_str().map(ToString::to_string);
    let mut probe = Probe {
        container: container.to_ascii_lowercase(),
        duration: float(&value["format"]["duration"]),
        bit_rate: whole(&value["format"]["bit_rate"]),
        ..Probe::default()
    };
    for stream in value["streams"].as_array().into_iter().flatten() {
        match stream["codec_type"].as_str() {
            // A cover picture is stored as a one-frame video stream.
            Some("video")
                if probe.video.is_none() && stream["disposition"]["attached_pic"] != 1 =>
            {
                probe.video = Some(Picture {
                    codec: text(&stream["codec_name"]).unwrap_or_default(),
                    profile: text(&stream["profile"]),
                    level: stream["level"].as_i64().filter(|&l| l > 0),
                    pix_fmt: text(&stream["pix_fmt"]).unwrap_or_default(),
                    width: whole(&stream["width"]).unwrap_or(0),
                    height: whole(&stream["height"]).unwrap_or(0),
                    fps: frame_rate(&stream["avg_frame_rate"])
                        .or_else(|| frame_rate(&stream["r_frame_rate"])),
                    color_transfer: text(&stream["color_transfer"]),
                    bit_rate: whole(&stream["bit_rate"]),
                });
            }
            Some("audio") if probe.audio.is_none() => {
                probe.audio = Some(Sound {
                    codec: text(&stream["codec_name"]).unwrap_or_default(),
                    channels: whole(&stream["channels"]),
                });
            }
            _ => {}
        }
    }
    Ok(probe)
}

/// A frame rate written as a fraction, `30000/1001`. None for ffprobe's
/// `0/0`, which means it doesn't know.
fn frame_rate(value: &Value) -> Option<f64> {
    let (num, den) = value.as_str()?.split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    let fps = num / den;
    (fps.is_finite() && fps > 0.0).then_some(fps)
}

/// ffmpeg's arguments to write the copy of `input` to `output`.
fn ffmpeg_args(
    picture: Video,
    sound: Audio,
    probe: &Probe,
    input: &Path,
    output: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let push = |args: &mut Vec<OsString>, items: &[&str]| {
        args.extend(items.iter().map(OsString::from));
    };
    // Progress as key=value lines on stdout, once a second.
    push(&mut args, &["-v", "error", "-nostdin", "-y", "-nostats"]);
    push(
        &mut args,
        &["-progress", "pipe:1", "-stats_period", "1", "-i"],
    );
    args.push(input.into());
    // The first picture and sound only: no subtitles, data or chapters,
    // which MP4 may not take.
    push(
        &mut args,
        &["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"],
    );
    push(&mut args, &["-map_chapters", "-1"]);
    match (picture, &probe.video) {
        (Video::Encode, Some(video)) => {
            if let Some(chain) = filters(video) {
                push(&mut args, &["-vf", &chain]);
            }
            push(
                &mut args,
                &["-c:v", "libx264", "-preset", PRESET, "-crf", CRF],
            );
            push(&mut args, &["-profile:v", "high", "-pix_fmt", "yuv420p"]);
            // x264 picks the level from the size and frame rate: 4.2 for
            // 1080p60, 5.1 or 5.2 for 4K, 6 for 8K.
            let kbps = max_kbps(video);
            push(&mut args, &["-maxrate", &format!("{kbps}k")]);
            push(
                &mut args,
                &["-bufsize", &format!("{}k", kbps.saturating_mul(2))],
            );
            // A keyframe every 2 seconds, so seeking lands quickly.
            let gop = (video.fps.unwrap_or(30.0) * 2.0).clamp(1.0, 600.0);
            push(&mut args, &["-g", &format!("{gop:.0}")]);
        }
        _ => push(&mut args, &["-c:v", "copy"]),
    }
    match sound {
        Audio::None => {}
        Audio::Copy => push(&mut args, &["-c:a", "copy"]),
        Audio::Aac { downmix } => {
            push(&mut args, &["-c:a", "aac", "-b:a", AUDIO_BITRATE]);
            if downmix {
                push(&mut args, &["-ac", "2"]);
            }
        }
    }
    // The index up front, so playback starts before the whole file loads.
    push(&mut args, &["-movflags", "+faststart", "-f", "mp4"]);
    args.push(output.into());
    args
}

/// The `-vf` chain the picture needs before x264, if any.
fn filters(video: &Picture) -> Option<String> {
    let mut chain = Vec::new();
    // 4:2:0 needs even sizes, so an odd one loses its last row or column.
    if video.width % 2 == 1 || video.height % 2 == 1 {
        chain.push("crop=trunc(iw/2)*2:trunc(ih/2)*2");
    }
    if is_hdr(video) {
        chain.push(TONEMAP);
    }
    (!chain.is_empty()).then(|| chain.join(","))
}

/// The copy's bitrate ceiling in kbit/s, scaled from 1080p's by pixel count.
fn max_kbps(video: &Picture) -> u64 {
    let pixels = video.width.saturating_mul(video.height);
    (MAX_KBPS_1080P.saturating_mul(pixels) / (1920 * 1080)).max(MIN_MAX_KBPS)
}

/// Run ffmpeg with `args`, passing its progress to `progress`.
async fn run_ffmpeg(
    args: Vec<OsString>,
    total: Option<f64>,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<()> {
    let mut cmd = crate::tools::ffmpeg_in_background()?;
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child =
        crate::tools::Running::spawn(&mut cmd).context("failed to run ffmpeg; is it installed?")?;
    let stdout = child.stdout.take().context("no stdout from ffmpeg")?;
    let mut stderr = child.stderr.take().context("no stderr from ffmpeg")?;
    // Read both pipes at once: ffmpeg blocks if either fills up.
    let read_stdout = async {
        let mut reader = ProgressReader::default();
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some((done, speed)) = reader.line(&line) {
                progress(Progress { done, total, speed });
            }
        }
        Ok::<_, std::io::Error>(())
    };
    let read_stderr = async {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok(bytes)
    };
    let ((), errors) =
        tokio::try_join!(read_stdout, read_stderr).context("reading ffmpeg's output")?;
    let status = child.wait().await.context("waiting for ffmpeg")?;
    if !status.success() {
        let errors = String::from_utf8_lossy(&errors);
        let lines: Vec<&str> = errors.lines().collect();
        let last = &lines[lines.len().saturating_sub(5)..];
        bail!("ffmpeg failed ({status}): {}", last.join(" / "));
    }
    Ok(())
}

/// Reads ffmpeg's `-progress` output: a block of `key=value` lines per
/// update, ending in `progress=continue` (or `end`).
#[derive(Default)]
struct ProgressReader {
    done: f64,
    speed: Option<f64>,
}

impl ProgressReader {
    /// Take one line. At the end of a block, returns the seconds done and the
    /// speed.
    fn line(&mut self, line: &str) -> Option<(f64, Option<f64>)> {
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        match key.trim() {
            // Microseconds, or N/A before the first frame.
            "out_time_us" => {
                if let Ok(us) = value.parse::<f64>()
                    && us.is_finite()
                    && us >= 0.0
                {
                    self.done = us / 1e6;
                }
            }
            "speed" => {
                self.speed = value
                    .trim_end_matches('x')
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0);
            }
            "progress" => return Some((self.done, self.speed)),
            _ => {}
        }
        None
    }
}

/// The original's picture as `navigator.mediaCapabilities.decodingInfo`
/// takes it (`contentType`, size, bitrate, frame rate), so the player can
/// ask the browser whether it plays the original smoothly and without
/// draining the battery, and play the copy otherwise.
pub fn source_config(probe: &Probe) -> Option<Value> {
    let video = probe.video.as_ref()?;
    let bitrate = video
        .bit_rate
        .or(probe.bit_rate)
        .filter(|&b| b > 0)
        .unwrap_or(5_000_000);
    let container = match probe.container.as_str() {
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        _ => "video/mp4",
    };
    Some(json!({
        "contentType": format!("{container}; codecs=\"{}\"", codecs(video)),
        "width": video.width,
        "height": video.height,
        "bitrate": bitrate,
        "framerate": video.fps.unwrap_or(30.0),
    }))
}

/// The picture's codec as a `codecs=` parameter (RFC 6381), from what
/// ffprobe says. Values it doesn't give are filled with common ones.
fn codecs(video: &Picture) -> String {
    let ten_bit = video.pix_fmt.contains("10");
    let profile = video.profile.as_deref().unwrap_or("");
    match video.codec.as_str() {
        "h264" => {
            let profile = match profile {
                "Baseline" => "4200",
                "Constrained Baseline" => "42E0",
                "Main" => "4D40",
                "High 10" => "6E00",
                "High 4:2:2" => "7A00",
                "High 4:4:4 Predictive" => "F400",
                _ => "6400",
            };
            format!("avc1.{profile}{:02X}", video.level.unwrap_or(40))
        }
        "hevc" => {
            let profile = if ten_bit || profile == "Main 10" {
                "2.4"
            } else {
                "1.6"
            };
            format!("hvc1.{profile}.L{}.B0", video.level.unwrap_or(120))
        }
        "av1" => {
            let profile = match profile {
                "High" => 1,
                "Professional" => 2,
                _ => 0,
            };
            format!(
                "av01.{profile}.{:02}M.{}",
                video.level.unwrap_or(8),
                if ten_bit { "10" } else { "08" }
            )
        }
        "vp9" => {
            let profile = profile
                .strip_prefix("Profile ")
                .and_then(|n| n.parse::<u8>().ok())
                .unwrap_or(0);
            // ffprobe doesn't know VP9's level, so it's the one the size needs.
            let pixels = video.width.saturating_mul(video.height);
            let level = match pixels {
                0..=921_600 => "31",
                921_601..=2_228_224 => "41",
                2_228_225..=8_912_896 => "51",
                _ => "61",
            };
            format!(
                "vp09.{profile:02}.{level}.{}",
                if ten_bit { "10" } else { "08" }
            )
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffprobe's output for a TikTok video, trimmed.
    const TIKTOK: &str = r#"{
        "streams": [
            {"codec_name": "hevc", "profile": "Main", "codec_type": "video", "width": 1080,
             "height": 1920, "pix_fmt": "yuv420p", "level": 150, "color_transfer": "bt709",
             "r_frame_rate": "30/1", "avg_frame_rate": "30/1", "bit_rate": "548040",
             "disposition": {"attached_pic": 0}},
            {"codec_name": "aac", "profile": "HE-AAC", "codec_type": "audio", "channels": 2,
             "r_frame_rate": "0/0", "avg_frame_rate": "0/0", "bit_rate": "63983",
             "disposition": {"attached_pic": 0}}
        ],
        "format": {"duration": "5.566667", "bit_rate": "973941"}
    }"#;

    /// And for a YouTube one, which gives no stream bitrates.
    const YOUTUBE: &str = r#"{
        "streams": [
            {"codec_name": "av1", "profile": "Main", "codec_type": "video", "width": 1920,
             "height": 1080, "pix_fmt": "yuv420p", "level": 9, "color_transfer": "bt709",
             "r_frame_rate": "60/1", "avg_frame_rate": "60/1", "disposition": {"attached_pic": 0}},
            {"codec_name": "opus", "codec_type": "audio", "channels": 2,
             "r_frame_rate": "0/0", "avg_frame_rate": "0/0", "disposition": {"attached_pic": 0}}
        ],
        "format": {"duration": "9.521000", "bit_rate": "1336098"}
    }"#;

    fn picture(codec: &str, width: u64, height: u64) -> Picture {
        Picture {
            codec: codec.into(),
            pix_fmt: "yuv420p".into(),
            width,
            height,
            fps: Some(30.0),
            ..Picture::default()
        }
    }

    fn file(container: &str, video: Picture, audio: Option<&str>) -> Probe {
        Probe {
            container: container.into(),
            video: Some(video),
            audio: audio.map(|codec| Sound {
                codec: codec.into(),
                channels: Some(2),
            }),
            ..Probe::default()
        }
    }

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn reads_ffprobe_output() {
        let probe = parse_probe(TIKTOK, "MP4").unwrap();
        assert_eq!(probe.container, "mp4");
        assert_eq!(probe.duration, Some(5.566_667));
        assert_eq!(probe.bit_rate, Some(973_941));
        let video = probe.video.unwrap();
        assert_eq!(
            video,
            Picture {
                codec: "hevc".into(),
                profile: Some("Main".into()),
                level: Some(150),
                pix_fmt: "yuv420p".into(),
                width: 1080,
                height: 1920,
                fps: Some(30.0),
                color_transfer: Some("bt709".into()),
                bit_rate: Some(548_040),
            }
        );
        assert_eq!(
            probe.audio,
            Some(Sound {
                codec: "aac".into(),
                channels: Some(2),
            })
        );

        let youtube = parse_probe(YOUTUBE, "webm").unwrap();
        assert_eq!(youtube.video.unwrap().bit_rate, None);
        assert_eq!(youtube.audio.unwrap().codec, "opus");
    }

    #[test]
    fn skips_cover_pictures_and_unknown_frame_rates() {
        let json = r#"{"streams": [
            {"codec_type": "video", "codec_name": "mjpeg", "disposition": {"attached_pic": 1}},
            {"codec_type": "video", "codec_name": "h264", "avg_frame_rate": "0/0",
             "r_frame_rate": "30000/1001", "level": -99, "disposition": {"attached_pic": 0}}
        ], "format": {}}"#;
        let probe = parse_probe(json, "mp4").unwrap();
        let video = probe.video.unwrap();
        assert_eq!(video.codec, "h264");
        assert_eq!(video.level, None);
        assert!((video.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(probe.audio, None);
        assert_eq!(probe.duration, None);
    }

    #[test]
    fn only_h264_and_aac_in_mp4_plays_everywhere() {
        let h264 = picture("h264", 1920, 1080);
        assert_eq!(
            plan(&file("mp4", h264.clone(), Some("aac"))).unwrap(),
            Plan::Ready
        );
        assert_eq!(plan(&file("mp4", h264.clone(), None)).unwrap(), Plan::Ready);
        // The same picture in another container, or with other sound, only
        // needs remuxing.
        assert_eq!(
            plan(&file("mkv", h264.clone(), Some("opus"))).unwrap(),
            Plan::Make {
                picture: Video::Copy,
                sound: Audio::Aac { downmix: false }
            }
        );
        assert_eq!(
            plan(&file("webm", h264.clone(), Some("aac"))).unwrap(),
            Plan::Make {
                picture: Video::Copy,
                sound: Audio::Copy
            }
        );
        // Every other picture is encoded.
        for codec in ["hevc", "av1", "vp9"] {
            assert_eq!(
                plan(&file("mp4", picture(codec, 1080, 1920), Some("aac"))).unwrap(),
                Plan::Make {
                    picture: Video::Encode,
                    sound: Audio::Copy
                }
            );
        }
        let ten_bit = Picture {
            pix_fmt: "yuv420p10le".into(),
            ..h264.clone()
        };
        assert_eq!(
            plan(&file("mp4", ten_bit, None)).unwrap().method(),
            "encode"
        );
        let hdr = Picture {
            color_transfer: Some("smpte2084".into()),
            ..h264
        };
        assert_eq!(plan(&file("mp4", hdr, None)).unwrap().method(), "encode");

        let surround = Probe {
            audio: Some(Sound {
                codec: "opus".into(),
                channels: Some(6),
            }),
            ..file("webm", picture("av1", 3840, 2160), None)
        };
        assert_eq!(
            plan(&surround).unwrap(),
            Plan::Make {
                picture: Video::Encode,
                sound: Audio::Aac { downmix: true }
            }
        );
        assert!(plan(&Probe::default()).is_err());
    }

    #[test]
    fn encodes_at_the_original_size() {
        let probe = file(
            "webm",
            Picture {
                fps: Some(60.0),
                ..picture("av1", 3840, 2160)
            },
            Some("opus"),
        );
        let args = ffmpeg_args(
            Video::Encode,
            Audio::Aac { downmix: false },
            &probe,
            Path::new("in/video.webm"),
            Path::new("out.mp4"),
        );
        let args = strings(&args);
        let joined = args.join(" ");
        assert!(joined.contains("-i in/video.webm -map 0:v:0 -map 0:a:0?"));
        assert!(
            joined.contains("-c:v libx264 -preset medium -crf 20 -profile:v high -pix_fmt yuv420p")
        );
        assert!(joined.contains("-maxrate 40000k -bufsize 80000k -g 120"));
        assert!(joined.contains("-c:a aac -b:a 160k -movflags +faststart -f mp4"));
        // No scaling, and no level that would cap the size.
        assert!(!args.contains(&"-vf"));
        assert!(!args.contains(&"-level:v"));
        assert_eq!(args.last(), Some(&"out.mp4"));
    }

    #[test]
    fn remuxing_copies_the_picture() {
        let probe = file("mkv", picture("h264", 1280, 720), Some("opus"));
        let args = ffmpeg_args(
            Video::Copy,
            Audio::Aac { downmix: true },
            &probe,
            Path::new("video.mkv"),
            Path::new("out.mp4"),
        );
        let joined = strings(&args).join(" ");
        assert!(joined.contains("-c:v copy -c:a aac -b:a 160k -ac 2"));
        assert!(!joined.contains("libx264"));

        let silent = ffmpeg_args(
            Video::Copy,
            Audio::None,
            &probe,
            Path::new("video.mkv"),
            Path::new("out.mp4"),
        );
        assert!(!strings(&silent).contains(&"-c:a"));
    }

    #[test]
    fn filters_only_what_x264_needs() {
        assert_eq!(filters(&picture("vp9", 1080, 1920)), None);
        assert_eq!(
            filters(&picture("vp9", 1081, 1920)).as_deref(),
            Some("crop=trunc(iw/2)*2:trunc(ih/2)*2")
        );
        let hdr = Picture {
            color_transfer: Some("arib-std-b67".into()),
            ..picture("av1", 3840, 2160)
        };
        assert_eq!(filters(&hdr).as_deref(), Some(TONEMAP));
    }

    #[test]
    fn the_bitrate_ceiling_scales_with_the_picture() {
        assert_eq!(max_kbps(&picture("av1", 1920, 1080)), 10_000);
        assert_eq!(max_kbps(&picture("av1", 1080, 1920)), 10_000);
        assert_eq!(max_kbps(&picture("av1", 3840, 2160)), 40_000);
        assert_eq!(max_kbps(&picture("av1", 7680, 4320)), 160_000);
        assert_eq!(max_kbps(&picture("av1", 480, 270)), MIN_MAX_KBPS);
    }

    #[test]
    fn reads_ffmpeg_progress() {
        let mut reader = ProgressReader::default();
        let mut updates = Vec::new();
        for line in [
            "frame=0",
            "out_time_us=N/A",
            "speed=N/A",
            "progress=continue",
            "frame=120",
            "out_time_us=2000000",
            "speed=1.5x",
            "progress=continue",
            "out_time_us=4500000",
            "speed= 2.25x",
            "progress=end",
        ] {
            updates.extend(reader.line(line));
        }
        assert_eq!(updates, [(0.0, None), (2.0, Some(1.5)), (4.5, Some(2.25))]);
    }

    #[test]
    fn describes_the_original_for_media_capabilities() {
        let tiktok = parse_probe(TIKTOK, "mp4").unwrap();
        assert_eq!(
            source_config(&tiktok).unwrap(),
            json!({
                "contentType": "video/mp4; codecs=\"hvc1.1.6.L150.B0\"",
                "width": 1080,
                "height": 1920,
                "bitrate": 548_040,
                "framerate": 30.0,
            })
        );
        // Without a stream bitrate, the file's stands in.
        let youtube = parse_probe(YOUTUBE, "webm").unwrap();
        let config = source_config(&youtube).unwrap();
        assert_eq!(
            config["contentType"],
            "video/webm; codecs=\"av01.0.09M.08\""
        );
        assert_eq!(config["bitrate"], 1_336_098);
        assert_eq!(source_config(&Probe::default()), None);
    }

    #[test]
    fn writes_codec_strings() {
        let with = |codec: &str, profile: &str, level: Option<i64>, pix_fmt: &str| Picture {
            profile: Some(profile.into()),
            level,
            pix_fmt: pix_fmt.into(),
            ..picture(codec, 1920, 1080)
        };
        assert_eq!(
            codecs(&with("h264", "High", Some(40), "yuv420p")),
            "avc1.640028"
        );
        assert_eq!(
            codecs(&with("h264", "Main", Some(31), "yuv420p")),
            "avc1.4D401F"
        );
        assert_eq!(
            codecs(&with("hevc", "Main 10", Some(153), "yuv420p10le")),
            "hvc1.2.4.L153.B0"
        );
        assert_eq!(
            codecs(&with("av1", "Main", Some(12), "yuv420p10le")),
            "av01.0.12M.10"
        );
        assert_eq!(
            codecs(&with("vp9", "Profile 0", None, "yuv420p")),
            "vp09.00.41.08"
        );
        assert_eq!(codecs(&with("vp8", "", None, "yuv420p")), "vp8");
    }
}
