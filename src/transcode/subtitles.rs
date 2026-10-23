//! Text subtitles (embedded or sidecar files) converted to WebVTT by ffmpeg.
//!
//! A track is converted once from the start and cached; requests for a
//! stream that starts later get the cues shifted in Rust, which is far
//! cheaper than asking ffmpeg to read the file again.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};

const TIMEOUT: Duration = Duration::from_secs(120);
const CACHE_ENTRIES: usize = 32;

/// (item, stream index, file mtime).
pub type TrackKey = (i64, i32, i64);

pub struct Subtitles {
    ffmpeg: std::path::PathBuf,
    /// Converted tracks, least recently used first.
    cache: Mutex<VecDeque<(TrackKey, Arc<String>)>>,
}

impl Subtitles {
    pub fn new(ffmpeg: std::path::PathBuf) -> Self {
        Self {
            ffmpeg,
            cache: Mutex::new(VecDeque::new()),
        }
    }

    /// WebVTT of a track: stream `index` of `file`, or the whole `external` file.
    pub async fn webvtt(
        &self,
        key: TrackKey,
        file: &Path,
        index: i32,
        external: Option<&Path>,
    ) -> anyhow::Result<Arc<String>> {
        if let Ok(mut c) = self.cache.lock()
            && let Some(pos) = c.iter().position(|(k, _)| *k == key)
            && let Some(entry) = c.remove(pos)
        {
            let vtt = entry.1.clone();
            c.push_back(entry);
            return Ok(vtt);
        }
        let mut cmd = tokio::process::Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]);
        match external {
            Some(sub) => {
                cmd.arg(sub).args(["-map", "0:s:0"]);
            }
            None => {
                cmd.arg(file).args(["-map", &format!("0:{index}")]);
            }
        }
        cmd.args(["-c:s", "webvtt", "-f", "webvtt", "pipe:1"])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(TIMEOUT, cmd.output())
            .await
            .context("subtitle conversion timed out")??;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "ffmpeg cannot convert the subtitles: {}",
                err.trim().lines().last().unwrap_or("unknown error")
            );
        }
        let vtt = Arc::new(String::from_utf8_lossy(&out.stdout).into_owned());
        if let Ok(mut c) = self.cache.lock() {
            c.push_back((key, vtt.clone()));
            while c.len() > CACHE_ENTRIES {
                c.pop_front();
            }
        }
        Ok(vtt)
    }
}

/// `HH:MM:SS.mmm` or `MM:SS.mmm` → milliseconds.
fn parse_time(s: &str) -> Option<i64> {
    let (hms, ms) = s.trim().split_once('.')?;
    let ms: i64 = ms.get(..3)?.parse().ok()?;
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    Some(secs * 1000 + ms)
}

fn format_time(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// Shift every cue by `-offset_ms`, dropping cues that end before zero.
pub fn shift(vtt: &str, offset_ms: i64) -> String {
    if offset_ms <= 0 {
        return vtt.to_owned();
    }
    let mut out = String::with_capacity(vtt.len());
    // Blocks are separated by blank lines; the first is the header.
    let normalized = vtt.replace("\r\n", "\n");
    for (i, block) in normalized.split("\n\n").enumerate() {
        if i == 0 {
            out.push_str(block);
            out.push_str("\n\n");
            continue;
        }
        let mut lines = block.lines();
        let mut kept = Vec::new();
        let mut timing_found = false;
        let mut drop = false;
        for line in lines.by_ref() {
            if !timing_found && let Some((a, rest)) = line.split_once("-->") {
                timing_found = true;
                let rest = rest.trim_start();
                let (b, settings) = rest.split_once(' ').unwrap_or((rest, ""));
                match (parse_time(a), parse_time(b)) {
                    (Some(start), Some(end)) if end - offset_ms > 0 => {
                        let mut t = format!(
                            "{} --> {}",
                            format_time(start - offset_ms),
                            format_time(end - offset_ms)
                        );
                        if !settings.is_empty() {
                            t.push(' ');
                            t.push_str(settings);
                        }
                        kept.push(t);
                    }
                    _ => drop = true,
                }
            } else {
                kept.push(line.to_owned());
            }
        }
        if !drop && !kept.is_empty() {
            out.push_str(&kept.join("\n"));
            out.push_str("\n\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_and_drops_cues() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nGone\n\n1\n00:01:00.000 --> 00:01:02.000 line:90%\nKept <i>here</i>\n\n";
        let s = shift(vtt, 30_000);
        assert_eq!(
            s,
            "WEBVTT\n\n1\n00:00:30.000 --> 00:00:32.000 line:90%\nKept <i>here</i>\n\n"
        );
        assert_eq!(shift(vtt, 0), vtt);
        assert_eq!(parse_time("01:02.003"), Some(62_003));
        assert_eq!(format_time(3_723_004), "01:02:03.004");
        // A cue straddling the start is clamped to zero.
        let s = shift("WEBVTT\n\n00:00:09.000 --> 00:00:12.000\nX\n", 10_000);
        assert!(s.contains("00:00:00.000 --> 00:00:02.000"));
    }
}
