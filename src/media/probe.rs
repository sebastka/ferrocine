//! Technical description of a media file, from `ffprobe -show_format -show_streams`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Short container name: `mp4`, `mkv`, `webm`, `avi`, `ts`, …
    pub container: String,
    pub duration_ms: Option<i64>,
    pub bitrate: Option<i64>,
    pub video: Option<VideoStream>,
    #[serde(default)]
    pub audio: Vec<AudioStream>,
    #[serde(default)]
    pub subtitles: Vec<SubtitleStream>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoStream {
    pub index: i32,
    pub codec: String,
    pub profile: Option<String>,
    pub level: Option<i32>,
    pub width: i32,
    pub height: i32,
    pub bit_depth: Option<i32>,
    pub pix_fmt: Option<String>,
    /// `hdr10`, `hlg` or `dolby_vision`.
    pub hdr: Option<String>,
    pub frame_rate: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioStream {
    pub index: i32,
    pub codec: String,
    pub profile: Option<String>,
    pub channels: Option<i32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleStream {
    /// Stream index in the file; external files are numbered from 1000.
    pub index: i32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub external: bool,
    /// Text-based (convertible to WebVTT), as opposed to bitmaps (PGS, VobSub).
    pub text: bool,
    /// External file; never sent to clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Downloaded from a provider (`opensubtitles`) and stored in the database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Index of the first external subtitle.
pub const EXTERNAL_SUBTITLE_BASE: i32 = 1000;

const TEXT_SUBTITLES: [&str; 7] = ["subrip", "ass", "ssa", "webvtt", "mov_text", "text", "srt"];

pub fn is_text_subtitle(codec: &str) -> bool {
    TEXT_SUBTITLES.contains(&codec)
}

impl MediaInfo {
    pub fn audio_stream(&self, index: Option<i32>) -> Option<&AudioStream> {
        match index {
            Some(i) => self.audio.iter().find(|a| a.index == i),
            None => self.audio.iter().find(|a| a.default).or(self.audio.first()),
        }
    }

    pub fn subtitle(&self, index: i32) -> Option<&SubtitleStream> {
        self.subtitles.iter().find(|s| s.index == index)
    }
}

/// Run ffprobe on `path`.
pub async fn probe(ffprobe: &Path, path: &Path) -> anyhow::Result<MediaInfo> {
    let mut cmd = tokio::process::Command::new(ffprobe);
    cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(super::disc::ffmpeg_input(path))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .context("ffprobe timed out")?
        .with_context(|| format!("cannot run {}", ffprobe.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ffprobe failed: {}",
            err.trim().lines().last().unwrap_or("unknown error")
        );
    }
    let json: Value = serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let mut info = parse(&json)?;
    // No client plays a DVD's file as is (it is one part of the movie), and
    // its timestamps restart: the IFO knows the duration.
    if super::disc::is_dvd(path) {
        info.container = "dvd".into();
        let p = path.to_path_buf();
        if let Ok(Some(title)) = tokio::task::spawn_blocking(move || super::disc::title(&p)).await {
            let size: Option<i64> = json
                .pointer("/format/size")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());
            info.bitrate = size.map(|s| s * 8000 / title.duration_ms.max(1)).or(info.bitrate);
            info.duration_ms = Some(title.duration_ms);
        }
    }
    Ok(info)
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(str::to_owned)
}

fn int_of(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn tag(v: &Value, key: &str) -> Option<String> {
    let tags = v.get("tags")?.as_object()?;
    // Tag names vary in case between containers.
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn disposition(v: &Value, key: &str) -> bool {
    v.get("disposition")
        .and_then(|d| d.get(key))
        .and_then(Value::as_i64)
        .is_some_and(|x| x == 1)
}

fn frame_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| (n / d * 1000.0).round() / 1000.0)
}

fn bit_depth(stream: &Value) -> Option<i32> {
    if let Some(b) = int_of(stream, "bits_per_raw_sample") {
        return i32::try_from(b).ok();
    }
    let pix = stream.get("pix_fmt").and_then(Value::as_str)?;
    Some(if pix.contains("10") {
        10
    } else if pix.contains("12") {
        12
    } else {
        8
    })
}

fn hdr(stream: &Value) -> Option<String> {
    let dovi = stream.get("side_data_list").and_then(Value::as_array).is_some_and(|l| {
        l.iter().any(|d| {
            d.get("side_data_type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("DOVI"))
        })
    });
    if dovi {
        return Some("dolby_vision".into());
    }
    match stream.get("color_transfer").and_then(Value::as_str) {
        Some("smpte2084") => Some("hdr10".into()),
        Some("arib-std-b67") => Some("hlg".into()),
        _ => None,
    }
}

/// Short container name from ffprobe's `format_name` and the codecs.
fn container(format_name: &str, video: Option<&VideoStream>, audio: &[AudioStream]) -> String {
    let first = format_name.split(',').next().unwrap_or_default();
    if format_name.contains("matroska") {
        let webm_video = video.is_none_or(|v| matches!(v.codec.as_str(), "vp8" | "vp9" | "av1"));
        let webm_audio = audio.iter().all(|a| matches!(a.codec.as_str(), "opus" | "vorbis"));
        return if webm_video && webm_audio { "webm" } else { "mkv" }.into();
    }
    if format_name.contains("mp4") || format_name.contains("mov") {
        return "mp4".into();
    }
    match first {
        "mpegts" => "ts".into(),
        "mpeg" => "mpeg".into(),
        "asf" => "wmv".into(),
        other => other.to_owned(),
    }
}

pub fn parse(json: &Value) -> anyhow::Result<MediaInfo> {
    let format = json.get("format").context("ffprobe output has no format")?;
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut video = None;
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    for s in &streams {
        let index = int_of(s, "index").and_then(|i| i32::try_from(i).ok()).unwrap_or(-1);
        let codec = str_of(s, "codec_name").unwrap_or_else(|| "unknown".into());
        match s.get("codec_type").and_then(Value::as_str) {
            // Cover art is a "video" stream too; skip attached pictures.
            Some("video") if video.is_none() && !disposition(s, "attached_pic") => {
                video = Some(VideoStream {
                    index,
                    codec,
                    profile: str_of(s, "profile"),
                    level: int_of(s, "level")
                        .and_then(|l| i32::try_from(l).ok())
                        .filter(|l| *l > 0),
                    width: int_of(s, "width").unwrap_or(0) as i32,
                    height: int_of(s, "height").unwrap_or(0) as i32,
                    bit_depth: bit_depth(s),
                    pix_fmt: str_of(s, "pix_fmt"),
                    hdr: hdr(s),
                    frame_rate: s
                        .get("avg_frame_rate")
                        .and_then(Value::as_str)
                        .and_then(frame_rate)
                        .or_else(|| s.get("r_frame_rate").and_then(Value::as_str).and_then(frame_rate)),
                });
            }
            Some("audio") => audio.push(AudioStream {
                index,
                codec,
                profile: str_of(s, "profile"),
                channels: int_of(s, "channels").and_then(|c| i32::try_from(c).ok()),
                language: tag(s, "language").filter(|l| l != "und"),
                title: tag(s, "title"),
                default: disposition(s, "default"),
            }),
            Some("subtitle") => subtitles.push(SubtitleStream {
                index,
                text: is_text_subtitle(&codec),
                codec,
                language: tag(s, "language").filter(|l| l != "und"),
                title: tag(s, "title"),
                default: disposition(s, "default"),
                forced: disposition(s, "forced"),
                external: false,
                path: None,
                source: None,
            }),
            _ => {}
        }
    }
    let duration_ms = format
        .get("duration")
        .and_then(Value::as_str)
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0) as i64);
    let format_name = str_of(format, "format_name").unwrap_or_default();
    Ok(MediaInfo {
        container: container(&format_name, video.as_ref(), &audio),
        duration_ms,
        bitrate: int_of(format, "bit_rate"),
        video,
        audio,
        subtitles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "hevc", "profile": "Main 10", "level": 150,
                 "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le", "color_transfer": "smpte2084",
                 "avg_frame_rate": "24000/1001", "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "codec_name": "eac3", "channels": 6,
                 "tags": {"language": "eng", "title": "Surround"}, "disposition": {"default": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "aac", "channels": 2,
                 "tags": {"LANGUAGE": "fre"}, "disposition": {"default": 0}},
                {"index": 3, "codec_type": "subtitle", "codec_name": "subrip",
                 "tags": {"language": "eng"}, "disposition": {"forced": 1}},
                {"index": 4, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle"},
                {"index": 5, "codec_type": "video", "codec_name": "mjpeg", "disposition": {"attached_pic": 1}}
            ],
            "format": {"format_name": "matroska,webm", "duration": "5400.250000", "bit_rate": "25000000"}
        })
    }

    #[test]
    fn parses_streams() {
        let m = parse(&sample()).unwrap();
        assert_eq!(m.container, "mkv");
        assert_eq!(m.duration_ms, Some(5_400_250));
        assert_eq!(m.bitrate, Some(25_000_000));
        let v = m.video.as_ref().unwrap();
        assert_eq!((v.codec.as_str(), v.width, v.height), ("hevc", 3840, 2160));
        assert_eq!(v.bit_depth, Some(10));
        assert_eq!(v.hdr.as_deref(), Some("hdr10"));
        assert_eq!(v.frame_rate, Some(23.976));
        assert_eq!(m.audio.len(), 2);
        assert_eq!(m.audio[1].language.as_deref(), Some("fre"));
        assert_eq!(m.audio_stream(None).unwrap().index, 1);
        assert_eq!(m.audio_stream(Some(2)).unwrap().codec, "aac");
        assert!(m.subtitles[0].text && m.subtitles[0].forced);
        assert!(!m.subtitles[1].text);
    }

    #[test]
    fn container_names() {
        let a = |c: &str| AudioStream {
            codec: c.into(),
            ..Default::default()
        };
        let v = |c: &str| VideoStream {
            codec: c.into(),
            ..Default::default()
        };
        assert_eq!(container("matroska,webm", Some(&v("vp9")), &[a("opus")]), "webm");
        assert_eq!(container("matroska,webm", Some(&v("h264")), &[a("aac")]), "mkv");
        assert_eq!(container("mov,mp4,m4a,3gp,3g2,mj2", None, &[]), "mp4");
        assert_eq!(container("mpegts", None, &[]), "ts");
        assert_eq!(container("avi", None, &[]), "avi");
    }
}
