//! Seek bar previews ("trickplay"): a frame every `interval`, tiled 10 × 10
//! into JPEG sheets in the metadata directory, made in the background after
//! scans, one item at a time.
//!
//! Only keyframes are decoded (`-skip_frame nokey`), so a pass over a film
//! takes seconds to a few minutes even for 4K; a frame is the last keyframe
//! before its time. A set is named by a tag of the file's identity (path,
//! size, modification time) and the settings, so an unchanged file is never
//! read twice and a replaced one gets new previews.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio_util::sync::CancellationToken;

use crate::assets::Assets;
use crate::config::{Config, TrickplayConfig};
use crate::store::{ItemRow, Store, Trickplay};
use crate::transcode::hw::ToneMapper;

pub const COLUMNS: u32 = 10;
pub const ROWS: u32 = 10;
/// Longest a single item may take.
const TIMEOUT: Duration = Duration::from_secs(3600);

/// The preview set an item should have now.
pub fn wanted_tag(row: &ItemRow, cfg: &TrickplayConfig) -> String {
    crate::assets::tag_of(
        format!(
            "{}|{:?}|{:?}|{}|{}|{COLUMNS}x{ROWS}",
            row.path_key,
            row.file_size,
            row.file_mtime,
            cfg.interval.0.as_millis(),
            cfg.width
        )
        .as_bytes(),
    )
}

/// Whether an item gets previews: something playable with a picture.
fn wanted(row: &ItemRow) -> bool {
    row.kind.playable() && row.path.is_some() && row.media.as_ref().is_some_and(|m| m.video.is_some())
}

/// The ffmpeg arguments making the sheets of `input` in `out`.
pub fn ffmpeg_args(input: &Path, cfg: &TrickplayConfig, tone_map: Option<ToneMapper>, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-skip_frame",
        "nokey",
        "-threads",
        "2",
    ]
    .map(String::from)
    .to_vec();
    a.extend(crate::media::disc::input_args(input, 0));
    let interval = cfg.interval.0.as_secs_f64();
    // Square pixels (anamorphic DVDs), then the preview width.
    let mut vf = format!("fps=1/{interval},scale=iw*sar:ih,setsar=1,scale={}:-2", cfg.width);
    if let Some(mapper) = tone_map {
        vf.push(',');
        vf.push_str(&crate::transcode::plan::tonemap(mapper, "yuv420p"));
    }
    vf.push_str(&format!(",tile={COLUMNS}x{ROWS}"));
    a.extend(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"].map(String::from));
    a.push(vf);
    a.extend(["-q:v", "5", "-start_number", "1", "-f", "image2"].map(String::from));
    a.push(out.join("%d.jpg").to_string_lossy().into_owned());
    a
}

/// Make the previews of an item.
pub async fn generate(
    ffmpeg: &Path,
    assets: &Assets,
    row: &ItemRow,
    cfg: &TrickplayConfig,
    tone_mapper: Option<ToneMapper>,
) -> anyhow::Result<Trickplay> {
    let Some(path) = row.path.as_deref().map(Path::new) else {
        bail!("no file");
    };
    let media = row.media.as_ref().context("not probed")?;
    let hdr = media.video.as_ref().is_some_and(|v| v.hdr.is_some());
    let work = assets.trickplay_workdir().await?;
    let args = ffmpeg_args(path, cfg, tone_mapper.filter(|_| hdr), &work);
    let run = tokio::process::Command::new(ffmpeg)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(TIMEOUT, run).await {
        Ok(r) => r.with_context(|| format!("cannot run {}", ffmpeg.display()))?,
        Err(_) => {
            let _ = tokio::fs::remove_dir_all(&work).await;
            bail!("ffmpeg took over an hour");
        }
    };
    let sheets = std::fs::read_dir(&work)?.flatten().count() as u32;
    if !out.status.success() || sheets == 0 {
        let _ = tokio::fs::remove_dir_all(&work).await;
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg failed: {}", err.trim().lines().last().unwrap_or("no frame"));
    }
    let (sheet_w, sheet_h) = image::image_dimensions(work.join("1.jpg")).context("unreadable sheet")?;
    let interval_ms = cfg.interval.0.as_millis() as i64;
    let per_sheet = (COLUMNS * ROWS) as i64;
    let thumbnails = match media.duration_ms {
        Some(d) if d > 0 => ((d + interval_ms - 1) / interval_ms).min(i64::from(sheets) * per_sheet),
        _ => i64::from(sheets) * per_sheet,
    };
    let tag = wanted_tag(row, cfg);
    assets.adopt_trickplay(&work, &tag).await?;
    Ok(Trickplay {
        tag,
        interval_ms,
        width: (sheet_w / COLUMNS) as i32,
        height: (sheet_h / ROWS) as i32,
        columns: COLUMNS as i32,
        rows: ROWS as i32,
        thumbnails: thumbnails as i32,
    })
}

/// Make the previews items lack, library by library; returns how many
/// were made. `failed` remembers what failed, not to try it again.
pub async fn pass(
    store: &Store,
    assets: &Arc<Assets>,
    config: &Config,
    tone_mapper: Option<ToneMapper>,
    failed: &mut HashSet<String>,
    cancel: &CancellationToken,
) -> anyhow::Result<u32> {
    let cfg = &config.metadata.trickplay;
    let mut made = 0;
    for lib in &config.libraries {
        for row in store.library_items(&lib.id).await?.into_iter().filter(wanted) {
            if cancel.is_cancelled() {
                return Ok(made);
            }
            let tag = wanted_tag(&row, cfg);
            if failed.contains(&tag) || store.trickplay(row.id).await?.is_some_and(|t| t.tag == tag) {
                continue;
            }
            let started = std::time::Instant::now();
            match generate(&config.transcoding.ffmpeg, assets, &row, cfg, tone_mapper).await {
                Ok(t) => {
                    store.put_trickplay(row.id, &t).await?;
                    made += 1;
                    tracing::info!(item = row.id, title = %row.meta.title, thumbnails = t.thumbnails,
                        seconds = started.elapsed().as_secs(), "seek bar previews made");
                }
                Err(e) => {
                    tracing::warn!(item = row.id, title = %row.meta.title, error = %format!("{e:#}"),
                        "cannot make seek bar previews; not tried again before a restart");
                    failed.insert(tag);
                }
            }
        }
    }
    Ok(made)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Dur;

    #[test]
    fn keyframes_tiled_at_the_preview_width() {
        let cfg = TrickplayConfig {
            enabled: true,
            interval: Dur::secs(10),
            width: 240,
        };
        let a = ffmpeg_args(Path::new("/m/a.mkv"), &cfg, None, Path::new("/meta/tmp/t1")).join(" ");
        assert!(
            a.starts_with("-hide_banner -loglevel error -nostdin -y -skip_frame nokey -threads 2 -i /m/a.mkv"),
            "{a}"
        );
        assert!(
            a.contains("-vf fps=1/10,scale=iw*sar:ih,setsar=1,scale=240:-2,tile=10x10 -q:v 5"),
            "{a}"
        );
        assert!(a.ends_with("-f image2 /meta/tmp/t1/%d.jpg"), "{a}");
        let hdr = ffmpeg_args(Path::new("/m/a.mkv"), &cfg, Some(ToneMapper::Tonemapx), Path::new("/t")).join(" ");
        assert!(hdr.contains("scale=240:-2,tonemapx="), "{hdr}");
    }
}
