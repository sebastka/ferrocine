//! ffmpeg and hardware acceleration detection: each candidate is proven by
//! encoding a few test frames, since an encoder listed by ffmpeg may still
//! lack a driver or a device.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::{HwAccel, TranscodingConfig};

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HwKind {
    Vaapi,
    Qsv,
    Nvenc,
}

impl HwKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HwKind::Vaapi => "vaapi",
            HwKind::Qsv => "qsv",
            HwKind::Nvenc => "nvenc",
        }
    }

    fn encoder(self) -> &'static str {
        match self {
            HwKind::Vaapi => "h264_vaapi",
            HwKind::Qsv => "h264_qsv",
            HwKind::Nvenc => "h264_nvenc",
        }
    }
}

/// How HDR is tone-mapped, depending on the filters ffmpeg has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToneMapper {
    /// Jellyfin's SIMD tone mapper (the image's ffmpeg).
    Tonemapx,
    /// Upstream ffmpeg: `zscale` (zimg) and `tonemap`, slower.
    Zscale,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FfmpegInfo {
    pub version: Option<String>,
    pub hw_accel: Option<HwKind>,
    pub device: Option<String>,
    /// `None`: HDR sources are transcoded without tone mapping.
    pub tone_mapper: Option<ToneMapper>,
    /// H.264/HEVC/AV1 encoders this ffmpeg was built with.
    pub encoders: Vec<String>,
}

async fn run(ffmpeg: &Path, args: &[&str]) -> Option<(bool, String, String)> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(TEST_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// `ffmpeg version 8.1.2 Copyright…` → `8.1.2`.
fn parse_version(out: &str) -> Option<String> {
    out.lines()
        .next()?
        .strip_prefix("ffmpeg version ")?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

fn parse_tone_mapper(filters: &str) -> Option<ToneMapper> {
    let has = |name: &str| filters.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
    if has("tonemapx") {
        Some(ToneMapper::Tonemapx)
    } else if has("zscale") && has("tonemap") {
        Some(ToneMapper::Zscale)
    } else {
        None
    }
}

fn parse_encoders(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            (flags.starts_with('V') && flags.len() == 6).then_some(name)
        })
        .filter(|n| n.contains("264") || n.contains("hevc") || n.contains("265") || n.contains("av1"))
        .map(str::to_owned)
        .collect()
}

/// Encode a few frames of a test pattern with `kind`.
async fn works(ffmpeg: &Path, kind: HwKind, device: &Path) -> Result<(), String> {
    let dev = device.to_string_lossy();
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
        .map(String::from)
        .to_vec();
    match kind {
        HwKind::Vaapi => {
            args.extend(["-init_hw_device", &format!("vaapi=hw:{dev}"), "-filter_hw_device", "hw"].map(String::from))
        }
        HwKind::Qsv => args.extend(
            [
                "-init_hw_device",
                &format!("vaapi=va:{dev}"),
                "-init_hw_device",
                "qsv=hw@va",
                "-filter_hw_device",
                "hw",
            ]
            .map(String::from),
        ),
        HwKind::Nvenc => {}
    }
    args.extend(
        [
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=25",
            "-frames:v",
            "10",
            "-vf",
        ]
        .map(String::from),
    );
    args.push(
        match kind {
            HwKind::Vaapi => "format=nv12,hwupload,scale_vaapi=w=640:h=360",
            HwKind::Qsv => "format=nv12,hwupload=extra_hw_frames=16,scale_qsv=w=640:h=360",
            HwKind::Nvenc => "scale=640:360,format=yuv420p",
        }
        .into(),
    );
    args.extend(["-c:v", kind.encoder(), "-f", "null", "-"].map(String::from));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(ffmpeg, &refs).await {
        Some((true, _, _)) => Ok(()),
        Some((false, _, err)) => Err(err.trim().lines().last().unwrap_or("failed").to_owned()),
        None => Err("ffmpeg did not finish".into()),
    }
}

/// Find ffmpeg's version, its encoders and a working acceleration.
pub async fn detect(cfg: &TranscodingConfig) -> FfmpegInfo {
    let mut info = FfmpegInfo::default();
    match run(&cfg.ffmpeg, &["-hide_banner", "-version"]).await {
        Some((true, out, _)) | Some((_, out, _)) if out.starts_with("ffmpeg version") => {
            info.version = parse_version(&out);
        }
        _ => {
            tracing::error!(ffmpeg = %cfg.ffmpeg.display(), "ffmpeg cannot be run: remux, transcoding and thumbnails are unavailable");
            return info;
        }
    }
    if let Some((_, out, _)) = run(&cfg.ffmpeg, &["-hide_banner", "-encoders"]).await {
        info.encoders = parse_encoders(&out);
    }
    if let Some((_, out, _)) = run(&cfg.ffmpeg, &["-hide_banner", "-filters"]).await {
        info.tone_mapper = parse_tone_mapper(&out);
    }
    if cfg.tone_mapping && info.tone_mapper.is_none() {
        tracing::warn!("ffmpeg has neither tonemapx nor zscale: HDR sources are transcoded without tone mapping");
    }
    let candidates: Vec<HwKind> = match cfg.hw_accel {
        HwAccel::None => vec![],
        HwAccel::Vaapi => vec![HwKind::Vaapi],
        HwAccel::Qsv => vec![HwKind::Qsv],
        HwAccel::Nvenc => vec![HwKind::Nvenc],
        HwAccel::Auto => vec![HwKind::Vaapi, HwKind::Qsv, HwKind::Nvenc],
    };
    for kind in candidates {
        if !info.encoders.iter().any(|e| e == kind.encoder()) {
            tracing::debug!(accel = kind.as_str(), "encoder not built into ffmpeg");
            continue;
        }
        if kind != HwKind::Nvenc && !cfg.device.exists() {
            tracing::debug!(accel = kind.as_str(), device = %cfg.device.display(), "no render device");
            continue;
        }
        match works(&cfg.ffmpeg, kind, &cfg.device).await {
            Ok(()) => {
                info.hw_accel = Some(kind);
                if kind != HwKind::Nvenc {
                    info.device = Some(cfg.device.to_string_lossy().into_owned());
                }
                break;
            }
            Err(e) => {
                let explicit = cfg.hw_accel != HwAccel::Auto;
                if explicit {
                    tracing::warn!(accel = kind.as_str(), error = %e, "hardware acceleration does not work; using software");
                } else {
                    tracing::debug!(accel = kind.as_str(), error = %e, "hardware acceleration test failed");
                }
            }
        }
    }
    tracing::info!(
        version = info.version.as_deref().unwrap_or("?"),
        accel = info.hw_accel.map_or("software", HwKind::as_str),
        tone_mapper = ?info.tone_mapper,
        "ffmpeg ready"
    );
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_output() {
        assert_eq!(
            parse_version("ffmpeg version 8.1.2 Copyright (c) 2000-2026\nbuilt with gcc").as_deref(),
            Some("8.1.2")
        );
        let enc = parse_encoders(
            "Encoders:\n V..... = Video\n ------\n V....D libx264              libx264 H.264\n V....D h264_vaapi           H.264/AVC (VAAPI)\n A....D aac                  AAC\n V....D mjpeg  MJPEG",
        );
        assert_eq!(enc, ["libx264", "h264_vaapi"]);
        let filters = " .S tonemap           V->V       Conversion\n .S zscale            V->V       Apply\n";
        assert_eq!(parse_tone_mapper(filters), Some(ToneMapper::Zscale));
        let jf = format!("{filters} .S tonemapx          V->V       SIMD optimized\n");
        assert_eq!(parse_tone_mapper(&jf), Some(ToneMapper::Tonemapx));
        assert_eq!(parse_tone_mapper(" .S tonemap  V->V  x\n"), None);
    }
}
