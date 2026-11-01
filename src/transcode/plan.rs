//! Playback decision (direct play, remux or transcode) and the ffmpeg
//! command lines that implement it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::hw::{HwKind, ToneMapper};
use crate::media::probe::{AudioStream, MediaInfo, VideoStream};

/// What the client can play, as it reports it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ClientCaps {
    #[serde(default)]
    pub containers: Vec<String>,
    #[serde(default)]
    pub video_codecs: Vec<String>,
    #[serde(default)]
    pub audio_codecs: Vec<String>,
    /// HDR formats the client shows (`hdr10`, `hlg`, `dolby_vision`). When
    /// given, video in another HDR format is tone-mapped; browsers, which
    /// cannot tell, leave it out and get HDR as is.
    #[serde(default)]
    pub hdr: Option<Vec<String>>,
    /// The client switches audio tracks itself (native players): direct play
    /// whatever audio track is chosen.
    #[serde(default)]
    pub selects_tracks: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub caps: ClientCaps,
    pub audio_index: Option<i32>,
    pub max_height: Option<u32>,
    pub max_bitrate: Option<u64>,
    pub force_transcode: bool,
    /// A bitmap subtitle stream (PGS, DVD) to burn into the picture.
    pub burn_subtitle: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Direct,
    Remux,
    Transcode,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Direct => "direct",
            Method::Remux => "remux",
            Method::Transcode => "transcode",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum VideoPlan {
    None,
    Copy {
        index: i32,
        hevc: bool,
    },
    Encode {
        index: i32,
        width: u32,
        height: u32,
        bitrate: u64,
        /// HDR source: tone-map to SDR (BT.709).
        tone_map: bool,
        /// A bitmap subtitle stream overlaid on the picture.
        burn: Option<i32>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioPlan {
    None,
    Copy {
        index: i32,
    },
    /// Converted to AAC stereo.
    Aac {
        index: i32,
    },
    /// Converted to AC-3 with `channels` (at most 5.1), for clients that
    /// decode AC-3: surround kept where AAC would make it stereo.
    Ac3 {
        index: i32,
        channels: u32,
    },
}

impl AudioPlan {
    /// The bitrate of converted audio, for the video's share of a limit.
    fn bitrate(&self) -> u64 {
        match self {
            AudioPlan::Ac3 { .. } => AC3_BITRATE,
            _ => AAC_BITRATE,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub method: Method,
    pub video: VideoPlan,
    pub audio: AudioPlan,
    pub reasons: Vec<String>,
}

impl Plan {
    pub fn tone_mapped(&self) -> bool {
        matches!(self.video, VideoPlan::Encode { tone_map: true, .. })
    }

    /// The subtitle stream burned into the picture.
    pub fn burned(&self) -> Option<i32> {
        match self.video {
            VideoPlan::Encode { burn, .. } => burn,
            _ => None,
        }
    }
}

/// Server-side limits of a decision.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Transcoded video is never taller than this.
    pub max_height: u32,
    /// Tone-map HDR sources when transcoding them.
    pub tone_mapping: bool,
}

/// HDR to SDR (BT.709), ending in pixel format `format`: the BT.2390 curve
/// with `tonemapx`, Hable with upstream ffmpeg (which lacks BT.2390).
/// Jellyfin's `tonemapx` also applies Dolby Vision metadata; the upstream
/// chain linearises with zimg, maps in floating point and converts back.
pub(crate) fn tonemap(mapper: ToneMapper, format: &str) -> String {
    match mapper {
        ToneMapper::Tonemapx => format!("tonemapx=tonemap=bt2390:desat=0:t=bt709:m=bt709:p=bt709:r=tv:format={format}"),
        ToneMapper::Zscale => format!(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,\
             zscale=t=bt709:m=bt709:r=tv,format={format}"
        ),
    }
}

/// Codecs that HLS with fMP4 segments can carry without re-encoding.
const HLS_VIDEO: [&str; 4] = ["h264", "hevc", "av1", "vp9"];
const HLS_AUDIO: [&str; 6] = ["aac", "mp3", "ac3", "eac3", "opus", "flac"];
const AAC_BITRATE: u64 = 192_000;
/// AC-3's highest rate, which 5.1 needs to sound right.
const AC3_BITRATE: u64 = 640_000;

fn has(list: &[String], codec: &str) -> bool {
    list.iter().any(|c| c.eq_ignore_ascii_case(codec))
}

/// Video the client can decode: its codec, and not 10-bit H.264 (High 10),
/// which no browser decodes.
fn video_ok(v: &VideoStream, caps: &ClientCaps) -> bool {
    has(&caps.video_codecs, &v.codec) && !(v.codec == "h264" && v.bit_depth.is_some_and(|b| b > 8))
}

/// An HDR format the client said it cannot show.
fn hdr_unsupported<'a>(v: &'a VideoStream, caps: &ClientCaps) -> Option<&'a str> {
    let format = v.hdr.as_deref()?;
    caps.hdr.as_ref().filter(|list| !has(list, format)).map(|_| format)
}

/// Target bitrate for a height.
fn bitrate_for(height: u32) -> u64 {
    match height {
        0..=480 => 1_500_000,
        481..=720 => 3_500_000,
        721..=1080 => 8_000_000,
        1081..=1440 => 12_000_000,
        _ => 20_000_000,
    }
}

fn even(x: u64) -> u32 {
    (x.max(2) & !1) as u32
}

/// Decide how to play `media` for this client.
pub fn decide(media: &MediaInfo, req: &Request, limits: Limits) -> Plan {
    let max_height = limits.max_height;
    let mut reasons = Vec::new();
    let audio: Option<&AudioStream> = media.audio_stream(req.audio_index);
    let first_audio = media.audio.first().map(|a| a.index);
    let limit_height = req.max_height.unwrap_or(u32::MAX).min(max_height);
    let too_tall = media.video.as_ref().is_some_and(|v| v.height as u32 > limit_height);
    let too_fast = match (req.max_bitrate, media.bitrate) {
        (Some(max), Some(b)) => b as u64 > max,
        _ => false,
    };
    if req.force_transcode {
        reasons.push("transcoding was requested".into());
    }
    // Bitmap subtitles cannot be sent as text: they are drawn on the picture.
    let burn = req
        .burn_subtitle
        .and_then(|i| media.subtitle(i))
        .filter(|s| !s.text && !s.external)
        .map(|s| s.index);
    if burn.is_some() {
        reasons.push("bitmap subtitles are burned into the picture".into());
    }
    if too_tall {
        reasons.push(format!("the video is taller than {limit_height} pixels"));
    }
    if too_fast {
        reasons.push("the bitrate exceeds the limit".into());
    }
    let codec_ok = media.video.as_ref().is_none_or(|v| video_ok(v, &req.caps));
    if let Some(v) = &media.video
        && !codec_ok
    {
        reasons.push(format!(
            "video codec {} is not supported by the client",
            describe_video(v)
        ));
    }
    let hdr_off = media.video.as_ref().and_then(|v| hdr_unsupported(v, &req.caps));
    if let Some(f) = hdr_off {
        reasons.push(format!("HDR format {f} is not supported by the client"));
    }
    let video_playable = codec_ok && hdr_off.is_none();
    let audio_playable = audio.is_none_or(|a| has(&req.caps.audio_codecs, &a.codec));
    if let Some(a) = audio
        && !audio_playable
    {
        reasons.push(format!("audio codec {} is not supported by the client", a.codec));
    }

    // Direct play: the client plays the file as is, with its first audio
    // track unless it switches tracks itself.
    let container_ok = has(&req.caps.containers, &media.container);
    let audio_is_first = req.caps.selects_tracks || audio.is_none_or(|a| Some(a.index) == first_audio);
    if !req.force_transcode
        && burn.is_none()
        && !too_tall
        && !too_fast
        && video_playable
        && audio_playable
        && container_ok
        && audio_is_first
    {
        return Plan {
            method: Method::Direct,
            video: VideoPlan::None,
            audio: AudioPlan::None,
            reasons,
        };
    }
    if !container_ok {
        reasons.push(format!("container {} is not supported by the client", media.container));
    }
    if !audio_is_first {
        reasons.push("another audio track was chosen".into());
    }

    let audio_plan = match audio {
        None => AudioPlan::None,
        Some(a) if audio_playable && HLS_AUDIO.contains(&a.codec.as_str()) => AudioPlan::Copy { index: a.index },
        // Surround the client cannot play: AC-3 5.1 when it plays AC-3 (TVs,
        // receivers), else AAC stereo.
        Some(a) if a.channels.unwrap_or(2) > 2 && has(&req.caps.audio_codecs, "ac3") => AudioPlan::Ac3 {
            index: a.index,
            channels: a.channels.unwrap_or(2).clamp(3, 6) as u32,
        },
        Some(a) => AudioPlan::Aac { index: a.index },
    };
    let video_plan = match &media.video {
        None => VideoPlan::None,
        Some(v)
            if !req.force_transcode
                && burn.is_none()
                && !too_tall
                && !too_fast
                && video_playable
                && HLS_VIDEO.contains(&v.codec.as_str()) =>
        {
            VideoPlan::Copy {
                index: v.index,
                hevc: v.codec == "hevc",
            }
        }
        Some(v) => {
            let src_h = (v.height.max(2)) as u32;
            let height = src_h.min(limit_height);
            let width = if height == src_h {
                even(v.width.max(2) as u64)
            } else {
                even(u64::from(v.width.max(2) as u32) * u64::from(height) / u64::from(src_h))
            };
            let height = even(u64::from(height));
            let mut bitrate = bitrate_for(height);
            if let Some(b) = media.bitrate {
                // No point in spending more than the source does.
                bitrate = bitrate.min((b as u64).max(1_000_000));
            }
            if let Some(max) = req.max_bitrate {
                bitrate = bitrate.min(max.saturating_sub(audio_plan.bitrate()).max(300_000));
            }
            VideoPlan::Encode {
                index: v.index,
                width,
                height,
                bitrate,
                tone_map: limits.tone_mapping && v.hdr.is_some(),
                burn,
            }
        }
    };
    let method = if matches!(video_plan, VideoPlan::Encode { .. }) {
        Method::Transcode
    } else {
        Method::Remux
    };
    Plan {
        method,
        video: video_plan,
        audio: audio_plan,
        reasons,
    }
}

fn describe_video(v: &VideoStream) -> String {
    match v.bit_depth {
        Some(b) if b > 8 && v.codec == "h264" => format!("h264 ({b}-bit)"),
        _ => v.codec.clone(),
    }
}

/// How the video is decoded, scaled and encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipeline {
    /// Hardware decode, scale and encode.
    Hardware(HwKind),
    /// Software decode and scale, hardware encode (the decoder lacks the codec).
    HybridEncode(HwKind),
    Software,
}

impl Pipeline {
    pub fn accel(self) -> Option<HwKind> {
        match self {
            Pipeline::Hardware(k) | Pipeline::HybridEncode(k) => Some(k),
            Pipeline::Software => None,
        }
    }
}

/// Pipelines to try in order: hardware first, software last.
///
/// Tone mapping runs on the CPU (`tonemapx`), on frames already scaled to
/// the output size. With VA-API, the GPU decodes and scales and the frames
/// are downloaded for it; Quick Sync and NVENC sessions that tone-map start
/// at the hybrid pipeline (software decoding), since their GPU tone mappers
/// could not be tested. Burning subtitles in overlays them on software
/// frames: those sessions start at the hybrid pipeline on every GPU.
pub fn pipelines(accel: Option<HwKind>, tone_map: bool, burn: bool) -> Vec<Pipeline> {
    match accel {
        Some(k) if burn => vec![Pipeline::HybridEncode(k), Pipeline::Software],
        Some(k @ (HwKind::Qsv | HwKind::Nvenc)) if tone_map => vec![Pipeline::HybridEncode(k), Pipeline::Software],
        Some(k) => vec![Pipeline::Hardware(k), Pipeline::HybridEncode(k), Pipeline::Software],
        None => vec![Pipeline::Software],
    }
}

pub struct CommandSpec<'a> {
    pub input: &'a Path,
    pub start_ms: i64,
    pub out_dir: &'a Path,
    pub segment_seconds: u32,
    pub device: &'a Path,
    pub x264_preset: &'a str,
    /// Used when the plan tone-maps.
    pub tone_mapper: ToneMapper,
    /// Colours of DVD subtitles to burn in (`rrggbb,…`), from the disc.
    pub subtitle_palette: Option<String>,
    /// Stream index → stream id: streams of a DVD are mapped by id.
    pub stream_ids: &'a std::collections::HashMap<i32, String>,
}

/// ffmpeg arguments writing an fMP4 HLS EVENT playlist in `out_dir`.
pub fn hls_args(plan: &Plan, pipeline: Pipeline, s: &CommandSpec<'_>) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "warning", "-nostdin", "-y"]
        .map(String::from)
        .to_vec();
    let device = s.device.to_string_lossy().into_owned();
    let encoding = matches!(plan.video, VideoPlan::Encode { .. });
    if encoding {
        match pipeline {
            Pipeline::Hardware(HwKind::Vaapi) if plan.tone_mapped() => a.extend(
                // A named device, for decoding and for uploading the tone-mapped frames.
                [
                    "-init_hw_device",
                    &format!("vaapi=va:{device}"),
                    "-filter_hw_device",
                    "va",
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    "va",
                    "-hwaccel_output_format",
                    "vaapi",
                ]
                .map(String::from),
            ),
            Pipeline::Hardware(HwKind::Vaapi) => a.extend(
                [
                    "-hwaccel",
                    "vaapi",
                    "-hwaccel_device",
                    &device,
                    "-hwaccel_output_format",
                    "vaapi",
                ]
                .map(String::from),
            ),
            Pipeline::Hardware(HwKind::Qsv) => a.extend(
                [
                    "-hwaccel",
                    "qsv",
                    "-qsv_device",
                    &device,
                    "-hwaccel_output_format",
                    "qsv",
                ]
                .map(String::from),
            ),
            Pipeline::Hardware(HwKind::Nvenc) => {
                a.extend(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"].map(String::from))
            }
            Pipeline::HybridEncode(HwKind::Vaapi) => a.extend(
                [
                    "-init_hw_device",
                    &format!("vaapi=va:{device}"),
                    "-filter_hw_device",
                    "va",
                ]
                .map(String::from),
            ),
            Pipeline::HybridEncode(HwKind::Qsv) => a.extend(
                [
                    "-init_hw_device",
                    &format!("vaapi=va:{device}"),
                    "-init_hw_device",
                    "qsv=qs@va",
                    "-filter_hw_device",
                    "qs",
                ]
                .map(String::from),
            ),
            Pipeline::HybridEncode(HwKind::Nvenc) | Pipeline::Software => {}
        }
    }
    if let (Some(_), Some(palette)) = (plan.burned(), &s.subtitle_palette) {
        a.extend(["-palette:s".into(), palette.clone()]);
    }
    // A DVD: subtitle streams appear when first shown, so a burn-in reads
    // further ahead before binding them.
    if !s.stream_ids.is_empty() {
        let (size, duration) = if plan.burned().is_some() {
            ("200M", "600M")
        } else {
            ("32M", "30M")
        };
        a.extend(["-probesize", size, "-analyzeduration", duration].map(String::from));
    }
    let sel = |index: i32| match s.stream_ids.get(&index) {
        Some(id) => format!("0:i:{id}"),
        None => format!("0:{index}"),
    };
    if matches!(plan.video, VideoPlan::Copy { .. }) {
        // Copied video starts at the keyframe before the seek point: start
        // the audio there too, not exactly at it, so both begin together.
        a.push("-noaccurate_seek".into());
    }
    a.extend(crate::media::disc::input_args(s.input, s.start_ms));
    match &plan.video {
        VideoPlan::None => {}
        VideoPlan::Copy { index, hevc } => {
            a.extend(["-map".into(), sel(*index), "-c:v".into(), "copy".into()]);
            if *hevc {
                // Browsers want the hvc1 sample entry for HEVC in MP4.
                a.extend(["-tag:v", "hvc1"].map(String::from));
            }
        }
        VideoPlan::Encode {
            index,
            width,
            height,
            bitrate,
            tone_map,
            burn,
        } => {
            let (w, h) = (*width, *height);
            let (filter, encoder): (String, &str) = match pipeline {
                Pipeline::Hardware(HwKind::Vaapi) if *tone_map => (
                    format!(
                        "scale_vaapi=w={w}:h={h}:format=p010,hwdownload,format=p010,{},hwupload",
                        tonemap(s.tone_mapper, "nv12")
                    ),
                    "h264_vaapi",
                ),
                Pipeline::HybridEncode(HwKind::Vaapi) if *tone_map => (
                    format!("scale={w}:{h},{},hwupload", tonemap(s.tone_mapper, "nv12")),
                    "h264_vaapi",
                ),
                Pipeline::HybridEncode(HwKind::Qsv) if *tone_map => (
                    format!(
                        "scale={w}:{h},{},hwupload=extra_hw_frames=64",
                        tonemap(s.tone_mapper, "nv12")
                    ),
                    "h264_qsv",
                ),
                Pipeline::HybridEncode(HwKind::Nvenc) if *tone_map => (
                    format!("scale={w}:{h},{}", tonemap(s.tone_mapper, "yuv420p")),
                    "h264_nvenc",
                ),
                Pipeline::Software if *tone_map => (
                    format!("scale={w}:{h},{}", tonemap(s.tone_mapper, "yuv420p")),
                    "libx264",
                ),
                Pipeline::Hardware(HwKind::Vaapi) => (format!("scale_vaapi=w={w}:h={h}:format=nv12"), "h264_vaapi"),
                Pipeline::Hardware(HwKind::Qsv) => (format!("scale_qsv=w={w}:h={h}:format=nv12"), "h264_qsv"),
                Pipeline::Hardware(HwKind::Nvenc) => (format!("scale_cuda=w={w}:h={h}:format=yuv420p"), "h264_nvenc"),
                Pipeline::HybridEncode(HwKind::Vaapi) => (format!("scale={w}:{h},format=nv12,hwupload"), "h264_vaapi"),
                Pipeline::HybridEncode(HwKind::Qsv) => (
                    format!("scale={w}:{h},format=nv12,hwupload=extra_hw_frames=64"),
                    "h264_qsv",
                ),
                Pipeline::HybridEncode(HwKind::Nvenc) => (format!("scale={w}:{h},format=yuv420p"), "h264_nvenc"),
                Pipeline::Software => (format!("scale={w}:{h},format=yuv420p"), "libx264"),
            };
            match burn {
                // Overlaid at the source size, before scaling (software frames:
                // a burn-in starts from the software-decoding pipelines).
                Some(sub) => a.extend([
                    "-filter_complex".into(),
                    format!(
                        "[{}][{}]overlay=eof_action=pass:repeatlast=0,{filter}[v]",
                        sel(*index),
                        sel(*sub)
                    ),
                    "-map".into(),
                    "[v]".into(),
                ]),
                None => a.extend(["-map".into(), sel(*index), "-vf".into(), filter]),
            }
            a.extend(["-c:v".into(), encoder.into()]);
            match encoder {
                "libx264" => a.extend(["-preset", s.x264_preset, "-profile:v", "high"].map(String::from)),
                "h264_nvenc" => a.extend(["-preset", "p4", "-profile:v", "high"].map(String::from)),
                "h264_qsv" => a.extend(["-profile:v", "high", "-look_ahead", "0"].map(String::from)),
                _ => a.extend(["-profile:v", "high"].map(String::from)),
            }
            a.extend([
                "-b:v".into(),
                bitrate.to_string(),
                "-maxrate".into(),
                bitrate.to_string(),
                "-bufsize".into(),
                (bitrate * 2).to_string(),
                // Keyframes on segment boundaries.
                "-force_key_frames".into(),
                format!("expr:gte(t,n_forced*{})", s.segment_seconds),
            ]);
        }
    }
    match &plan.audio {
        AudioPlan::None => a.push("-an".into()),
        AudioPlan::Copy { index } => a.extend(["-map".into(), sel(*index), "-c:a".into(), "copy".into()]),
        AudioPlan::Aac { index } => a.extend([
            "-map".into(),
            sel(*index),
            "-c:a".into(),
            "aac".into(),
            "-ac".into(),
            "2".into(),
            "-b:a".into(),
            AAC_BITRATE.to_string(),
        ]),
        AudioPlan::Ac3 { index, channels } => a.extend([
            "-map".into(),
            sel(*index),
            "-c:a".into(),
            "ac3".into(),
            "-ac".into(),
            channels.to_string(),
            "-b:a".into(),
            AC3_BITRATE.to_string(),
        ]),
    }
    let dir = s.out_dir.to_string_lossy();
    a.extend(
        [
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-max_muxing_queue_size",
            "2048",
            "-f",
            "hls",
            "-hls_time",
            &s.segment_seconds.to_string(),
            "-hls_playlist_type",
            "event",
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "init.mp4",
            "-hls_segment_filename",
            &format!("{dir}/seg%05d.m4s"),
            "-hls_flags",
            "independent_segments+temp_file",
            "-start_number",
            "0",
            &format!("{dir}/index.m3u8"),
        ]
        .map(String::from),
    );
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> ClientCaps {
        ClientCaps {
            containers: vec!["mp4".into(), "webm".into()],
            video_codecs: vec!["h264".into(), "vp9".into()],
            audio_codecs: vec!["aac".into(), "mp3".into(), "opus".into()],
            ..Default::default()
        }
    }

    fn media(container: &str, video: &str, audio: &[&str]) -> MediaInfo {
        MediaInfo {
            container: container.into(),
            duration_ms: Some(3_600_000),
            bitrate: Some(6_000_000),
            video: Some(VideoStream {
                index: 0,
                codec: video.into(),
                width: 1920,
                height: 1080,
                bit_depth: Some(8),
                ..Default::default()
            }),
            audio: audio
                .iter()
                .enumerate()
                .map(|(i, c)| AudioStream {
                    index: i as i32 + 1,
                    codec: (*c).into(),
                    default: i == 0,
                    ..Default::default()
                })
                .collect(),
            subtitles: vec![],
            chapters: vec![],
        }
    }

    fn decide_(m: &MediaInfo, r: &Request, max_height: u32) -> Plan {
        decide(
            m,
            r,
            Limits {
                max_height,
                tone_mapping: true,
            },
        )
    }

    fn req() -> Request {
        Request {
            caps: caps(),
            ..Default::default()
        }
    }

    #[test]
    fn direct_play_when_everything_fits() {
        let p = decide_(&media("mp4", "h264", &["aac"]), &req(), 2160);
        assert_eq!(p.method, Method::Direct);
    }

    #[test]
    fn remux_for_containers_and_audio() {
        let p = decide_(&media("mkv", "h264", &["aac"]), &req(), 2160);
        assert_eq!(p.method, Method::Remux);
        assert_eq!(p.audio, AudioPlan::Copy { index: 1 });
        let p = decide_(&media("mkv", "h264", &["dts"]), &req(), 2160);
        assert_eq!(p.method, Method::Remux);
        assert_eq!(p.audio, AudioPlan::Aac { index: 1 });
        // Surround a client cannot play: AC-3 5.1 when it plays AC-3, else AAC stereo.
        let mut dts = media("mkv", "h264", &["dts"]);
        dts.audio[0].channels = Some(8);
        assert_eq!(decide_(&dts, &req(), 2160).audio, AudioPlan::Aac { index: 1 });
        let mut tv = req();
        tv.caps.audio_codecs.push("ac3".into());
        let p = decide_(&dts, &tv, 2160);
        assert_eq!(p.audio, AudioPlan::Ac3 { index: 1, channels: 6 });
        let spec = CommandSpec {
            input: Path::new("/m/a.mkv"),
            start_ms: 0,
            out_dir: Path::new("/tmp/s"),
            segment_seconds: 4,
            device: Path::new("/dev/dri/renderD128"),
            x264_preset: "veryfast",
            tone_mapper: ToneMapper::Tonemapx,
            subtitle_palette: None,
            stream_ids: &Default::default(),
        };
        let args = hls_args(&p, Pipeline::Software, &spec).join(" ");
        assert!(args.contains("-c:a ac3 -ac 6 -b:a 640000"), "{args}");
        dts.audio[0].channels = Some(2);
        assert_eq!(decide_(&dts, &tv, 2160).audio, AudioPlan::Aac { index: 1 });
        // Second audio track of an MP4: remux to pick it.
        let mut r = req();
        r.audio_index = Some(2);
        let p = decide_(&media("mp4", "h264", &["aac", "aac"]), &r, 2160);
        assert_eq!((p.method, p.audio), (Method::Remux, AudioPlan::Copy { index: 2 }));
        // …unless the client switches tracks itself.
        r.caps.selects_tracks = true;
        let p = decide_(&media("mp4", "h264", &["aac", "aac"]), &r, 2160);
        assert_eq!(p.method, Method::Direct);
    }

    #[test]
    fn copied_video_starts_audio_at_the_same_seek_point() {
        let p = decide_(&media("mkv", "h264", &["aac"]), &req(), 2160);
        assert_eq!(p.method, Method::Remux);
        let spec = CommandSpec {
            input: Path::new("/m/a.mkv"),
            start_ms: 100_000,
            out_dir: Path::new("/tmp/s"),
            segment_seconds: 4,
            device: Path::new("/dev/dri/renderD128"),
            x264_preset: "veryfast",
            tone_mapper: ToneMapper::Tonemapx,
            subtitle_palette: None,
            stream_ids: &Default::default(),
        };
        let args = hls_args(&p, Pipeline::Software, &spec).join(" ");
        assert!(args.contains("-noaccurate_seek -ss 100.000 -i /m/a.mkv"), "{args}");
        // Encoded video starts exactly where asked.
        let t = decide_(&media("mkv", "mpeg4", &["aac"]), &req(), 2160);
        let args = hls_args(&t, Pipeline::Software, &spec).join(" ");
        assert!(!args.contains("-noaccurate_seek"), "{args}");
    }

    #[test]
    fn transcode_for_codecs_and_limits() {
        let p = decide_(&media("mkv", "hevc", &["aac"]), &req(), 2160);
        assert_eq!(p.method, Method::Transcode);
        assert!(p.reasons[0].contains("hevc"));
        assert!(p.reasons[1].contains("container mkv"));
        let mut m = media("mp4", "h264", &["aac"]);
        m.video.as_mut().unwrap().bit_depth = Some(10);
        assert_eq!(decide_(&m, &req(), 2160).method, Method::Transcode);
        let mut r = req();
        r.max_height = Some(720);
        let p = decide_(&media("mp4", "h264", &["aac"]), &r, 2160);
        assert_eq!(
            p.video,
            VideoPlan::Encode {
                index: 0,
                width: 1280,
                height: 720,
                bitrate: 3_500_000,
                tone_map: false,
                burn: None
            }
        );
        let mut r = req();
        r.max_bitrate = Some(2_000_000);
        let p = decide_(&media("mp4", "h264", &["aac"]), &r, 2160);
        assert!(matches!(
            p.video,
            VideoPlan::Encode {
                bitrate: 1_808_000,
                height: 1080,
                ..
            }
        ));
    }

    #[test]
    fn hdr_is_tone_mapped_when_transcoded() {
        let mut m = media("mkv", "hevc", &["aac"]);
        m.video.as_mut().unwrap().hdr = Some("hdr10".into());
        m.video.as_mut().unwrap().bit_depth = Some(10);
        let p = decide_(&m, &req(), 2160);
        assert!(p.tone_mapped());
        let off = decide(
            &m,
            &req(),
            Limits {
                max_height: 2160,
                tone_mapping: false,
            },
        );
        assert!(!off.tone_mapped());
        // A remux keeps HDR for clients that play HEVC.
        let mut hevc = req();
        hevc.caps.video_codecs.push("hevc".into());
        let r = decide_(&m, &hevc, 2160);
        assert_eq!(r.method, Method::Remux);
        assert!(!r.tone_mapped());
        // A client listing its HDR formats: HDR10 kept, Dolby Vision tone-mapped.
        hevc.caps.containers.push("mkv".into());
        hevc.caps.hdr = Some(vec!["hdr10".into(), "hlg".into()]);
        assert_eq!(decide_(&m, &hevc, 2160).method, Method::Direct);
        m.video.as_mut().unwrap().hdr = Some("dolby_vision".into());
        let p = decide_(&m, &hevc, 2160);
        assert!(p.tone_mapped(), "{p:?}");
        assert!(p.reasons.iter().any(|r| r.contains("dolby_vision")));
        // An SDR TV: every HDR format is tone-mapped.
        hevc.caps.hdr = Some(vec![]);
        m.video.as_mut().unwrap().hdr = Some("hdr10".into());
        assert!(decide_(&m, &hevc, 2160).tone_mapped());

        let spec = CommandSpec {
            input: Path::new("/m/hdr.mkv"),
            start_ms: 0,
            out_dir: Path::new("/tmp/s"),
            segment_seconds: 4,
            device: Path::new("/dev/dri/renderD128"),
            x264_preset: "veryfast",
            tone_mapper: ToneMapper::Tonemapx,
            subtitle_palette: None,
            stream_ids: &Default::default(),
        };
        let tm = "tonemapx=tonemap=bt2390:desat=0:t=bt709:m=bt709:p=bt709:r=tv";
        let hw = hls_args(&p, Pipeline::Hardware(HwKind::Vaapi), &spec).join(" ");
        assert!(hw.starts_with("-hide_banner -loglevel warning -nostdin -y -init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va -hwaccel vaapi -hwaccel_device va -hwaccel_output_format vaapi -i"), "{hw}");
        assert!(hw.contains(&format!("-vf scale_vaapi=w=1920:h=1080:format=p010,hwdownload,format=p010,{tm}:format=nv12,hwupload -c:v h264_vaapi")), "{hw}");
        let sw = hls_args(&p, Pipeline::Software, &spec).join(" ");
        assert!(
            sw.contains(&format!("-vf scale=1920:1080,{tm}:format=yuv420p -c:v libx264")),
            "{sw}"
        );
        let nv = hls_args(&p, Pipeline::HybridEncode(HwKind::Nvenc), &spec).join(" ");
        assert!(nv.contains(&format!("{tm}:format=yuv420p -c:v h264_nvenc")), "{nv}");
        // Quick Sync and NVENC tone-map from the hybrid pipeline on.
        assert_eq!(
            pipelines(Some(HwKind::Nvenc), true, false),
            [Pipeline::HybridEncode(HwKind::Nvenc), Pipeline::Software]
        );
        assert_eq!(
            pipelines(Some(HwKind::Vaapi), true, false)[0],
            Pipeline::Hardware(HwKind::Vaapi)
        );
    }

    #[test]
    fn burns_bitmap_subtitles() {
        let mut m = media("mp4", "h264", &["aac"]);
        let sub = |index: i32, codec: &str, text: bool| crate::media::probe::SubtitleStream {
            index,
            codec: codec.into(),
            text,
            ..Default::default()
        };
        m.subtitles = vec![sub(3, "dvd_subtitle", false), sub(4, "subrip", true)];
        let burn = |i: i32| Request {
            burn_subtitle: Some(i),
            ..req()
        };
        // Direct play otherwise; a bitmap subtitle forces an encode.
        assert_eq!(decide_(&m, &req(), 2160).method, Method::Direct);
        let plan = decide_(&m, &burn(3), 2160);
        assert_eq!((plan.method, plan.burned()), (Method::Transcode, Some(3)));
        assert!(plan.reasons.iter().any(|r| r.contains("burned")));
        // Text subtitles are sent as WebVTT, never burned.
        assert_eq!(decide_(&m, &burn(4), 2160).burned(), None);
        assert_eq!(
            pipelines(Some(HwKind::Vaapi), false, true),
            [Pipeline::HybridEncode(HwKind::Vaapi), Pipeline::Software]
        );
        let spec = CommandSpec {
            input: Path::new("/m/a.mkv"),
            start_ms: 0,
            out_dir: Path::new("/tmp/s1"),
            segment_seconds: 4,
            device: Path::new("/dev/dri/renderD128"),
            x264_preset: "veryfast",
            tone_mapper: ToneMapper::Tonemapx,
            subtitle_palette: Some("000000,ffffff".into()),
            stream_ids: &Default::default(),
        };
        let a = hls_args(&plan, Pipeline::Software, &spec).join(" ");
        assert!(!a.contains("probesize"), "{a}");
        // A DVD: by stream id, reading further ahead for the subtitle.
        let ids = [(0, "0x1e0".to_owned()), (1, "0x80".to_owned()), (3, "0x20".to_owned())].into();
        let dvd = CommandSpec {
            stream_ids: &ids,
            subtitle_palette: spec.subtitle_palette.clone(),
            ..spec
        };
        let d = hls_args(&plan, Pipeline::Software, &dvd).join(" ");
        assert!(d.contains("-probesize 200M -analyzeduration 600M -i"), "{d}");
        assert!(
            d.contains("[0:i:0x1e0][0:i:0x20]overlay") && d.contains("-map 0:i:0x80"),
            "{d}"
        );
        let a = hls_args(&plan, Pipeline::HybridEncode(HwKind::Vaapi), &spec).join(" ");
        assert!(a.contains("-palette:s 000000,ffffff -i /m/a.mkv"), "{a}");
        assert!(
            a.contains(
                "-filter_complex [0:0][0:3]overlay=eof_action=pass:repeatlast=0,scale=1920:1080,format=nv12,hwupload[v] -map [v] -c:v h264_vaapi"
            ),
            "{a}"
        );
    }

    #[test]
    fn command_lines() {
        let plan = decide_(&media("mkv", "mpeg4", &["ac3"]), &req(), 2160);
        let spec = CommandSpec {
            input: Path::new("/m/a.mkv"),
            start_ms: 90_500,
            out_dir: Path::new("/tmp/s1"),
            segment_seconds: 4,
            device: Path::new("/dev/dri/renderD128"),
            x264_preset: "veryfast",
            tone_mapper: ToneMapper::Tonemapx,
            subtitle_palette: None,
            stream_ids: &Default::default(),
        };
        let hw = hls_args(&plan, Pipeline::Hardware(HwKind::Vaapi), &spec).join(" ");
        assert!(hw.contains(
            "-hwaccel vaapi -hwaccel_device /dev/dri/renderD128 -hwaccel_output_format vaapi -ss 90.500 -i /m/a.mkv"
        ));
        assert!(hw.contains("-vf scale_vaapi=w=1920:h=1080:format=nv12 -c:v h264_vaapi"));
        assert!(hw.contains("-c:a aac -ac 2"));
        assert!(hw.ends_with("-start_number 0 /tmp/s1/index.m3u8"));
        let hybrid = hls_args(&plan, Pipeline::HybridEncode(HwKind::Vaapi), &spec).join(" ");
        assert!(hybrid.contains("-init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va"));
        assert!(hybrid.contains("scale=1920:1080,format=nv12,hwupload"));
        let sw = hls_args(&plan, Pipeline::Software, &spec).join(" ");
        assert!(sw.contains("-c:v libx264 -preset veryfast") && !sw.contains("hwaccel"));
        // NVENC: CUDA decode and scaling, or software decode feeding the encoder.
        let nv = hls_args(&plan, Pipeline::Hardware(HwKind::Nvenc), &spec).join(" ");
        assert!(
            nv.contains("-hwaccel cuda -hwaccel_output_format cuda -ss 90.500 -i /m/a.mkv"),
            "{nv}"
        );
        assert!(
            nv.contains("-vf scale_cuda=w=1920:h=1080:format=yuv420p -c:v h264_nvenc -preset p4"),
            "{nv}"
        );
        let nv = hls_args(&plan, Pipeline::HybridEncode(HwKind::Nvenc), &spec).join(" ");
        assert!(
            !nv.contains("hwaccel") && nv.contains("-vf scale=1920:1080,format=yuv420p -c:v h264_nvenc"),
            "{nv}"
        );
        // Quick Sync: through VA-API's device, as Intel's drivers expect.
        let qsv = hls_args(&plan, Pipeline::Hardware(HwKind::Qsv), &spec).join(" ");
        assert!(
            qsv.contains("-hwaccel qsv -qsv_device /dev/dri/renderD128 -hwaccel_output_format qsv"),
            "{qsv}"
        );
        assert!(
            qsv.contains("-vf scale_qsv=w=1920:h=1080:format=nv12 -c:v h264_qsv"),
            "{qsv}"
        );
        let qsv = hls_args(&plan, Pipeline::HybridEncode(HwKind::Qsv), &spec).join(" ");
        assert!(
            qsv.contains("-init_hw_device vaapi=va:/dev/dri/renderD128 -init_hw_device qsv=qs@va -filter_hw_device qs")
        );
        assert!(
            qsv.contains("scale=1920:1080,format=nv12,hwupload=extra_hw_frames=64 -c:v h264_qsv"),
            "{qsv}"
        );
        // Every pipeline keeps keyframes on segment boundaries.
        for p in pipelines(Some(HwKind::Nvenc), false, false) {
            assert!(
                hls_args(&plan, p, &spec)
                    .join(" ")
                    .contains("-force_key_frames expr:gte(t,n_forced*4)")
            );
        }
        let remux = decide_(&media("mkv", "h264", &["aac"]), &req(), 2160);
        let r = hls_args(&remux, Pipeline::Hardware(HwKind::Vaapi), &spec).join(" ");
        assert!(r.contains("-map 0:0 -c:v copy -map 0:1 -c:a copy") && !r.contains("hwaccel"));
    }
}
