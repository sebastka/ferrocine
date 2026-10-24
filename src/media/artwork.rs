//! Artwork: found next to the media, downloaded, or extracted from the video;
//! always resized and re-encoded as JPEG before it is stored.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

use super::naming::IMAGE_EXTENSIONS;
use crate::store::ImageKind;

/// Largest source image accepted (bytes).
pub const MAX_SOURCE_BYTES: u64 = 25 * 1024 * 1024;
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Processed {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Width limit for a kind of image, from the configured poster width.
pub fn max_width(kind: ImageKind, poster_width: u32) -> u32 {
    match kind {
        ImageKind::Poster | ImageKind::Thumb => poster_width,
        ImageKind::Backdrop => (poster_width * 2).min(1920),
        ImageKind::Logo => (poster_width * 4 / 3).min(800),
    }
}

/// Decode, shrink to `max_width` (never enlarge) and encode as JPEG.
/// CPU-bound: call from a blocking thread.
pub fn process(bytes: &[u8], max_width: u32, quality: u8) -> anyhow::Result<Processed> {
    let img = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("cannot decode image")?;
    let img = if img.width() > max_width {
        let height = (u64::from(img.height()) * u64::from(max_width) / u64::from(img.width())).max(1) as u32;
        img.resize_exact(max_width, height, FilterType::CatmullRom)
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let mut out = Vec::with_capacity(64 * 1024);
    JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb)?;
    Ok(Processed {
        width: rgb.width(),
        height: rgb.height(),
        data: out,
    })
}

/// Process an image for `kind`: logos keep their transparency as PNG,
/// everything else becomes JPEG.
pub fn process_for(kind: ImageKind, bytes: &[u8], max_width: u32, quality: u8) -> anyhow::Result<Processed> {
    if kind != ImageKind::Logo {
        return process(bytes, max_width, quality);
    }
    let img = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("cannot decode image")?;
    let img = if img.width() > max_width {
        let height = (u64::from(img.height()) * u64::from(max_width) / u64::from(img.width())).max(1) as u32;
        img.resize_exact(max_width, height, FilterType::CatmullRom)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let mut out = Cursor::new(Vec::with_capacity(64 * 1024));
    image::DynamicImage::ImageRgba8(rgba.clone()).write_to(&mut out, image::ImageFormat::Png)?;
    Ok(Processed {
        width: rgba.width(),
        height: rgba.height(),
        data: out.into_inner(),
    })
}

/// Read and process an image file next to the media.
pub async fn load_file(path: &Path, kind: ImageKind, max_width: u32, quality: u8) -> anyhow::Result<Processed> {
    let meta = tokio::fs::metadata(path).await?;
    if meta.len() > MAX_SOURCE_BYTES {
        bail!("image too large ({} bytes)", meta.len());
    }
    let bytes = tokio::fs::read(path).await?;
    tokio::task::spawn_blocking(move || process_for(kind, &bytes, max_width, quality)).await?
}

/// The first image in `names` (files of `dir`) whose stem is one of `bases`
/// (case-insensitive), in the order of `bases`.
pub fn find_image(dir: &Path, names: &[String], bases: &[String]) -> Option<PathBuf> {
    for base in bases {
        for name in names {
            let p = Path::new(name);
            let stem_matches = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case(base));
            let ext_matches = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
            if stem_matches && ext_matches {
                return Some(dir.join(name));
            }
        }
    }
    None
}

/// Extract one frame at `at_ms` as JPEG bytes.
pub async fn extract_frame(ffmpeg: &Path, file: &Path, at_ms: i64, width: u32) -> anyhow::Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(super::disc::input_args(file, at_ms))
        .args(["-map", "0:v:0", "-frames:v", "1", "-an", "-sn", "-vf"])
        // Square pixels and the target width; tone mapping is out of scope.
        .arg(format!("scale='min({width},iw)':-2,format=yuvj420p"))
        .args(["-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "3", "pipe:1"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(EXTRACT_TIMEOUT, cmd.output())
        .await
        .context("frame extraction timed out")??;
    if !out.status.success() || out.stdout.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ffmpeg cannot extract a frame: {}",
            err.trim().lines().last().unwrap_or("no output")
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_and_encodes() {
        let img = image::RgbImage::from_fn(800, 400, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let p = process(&png, 300, 80).unwrap();
        assert_eq!((p.width, p.height), (300, 150));
        assert_eq!(&p.data[..2], &[0xFF, 0xD8]);
        // Never enlarged.
        let p = process(&png, 2000, 80).unwrap();
        assert_eq!(p.width, 800);
        assert!(process(b"not an image", 300, 80).is_err());
    }

    #[test]
    fn finds_images_in_priority_order() {
        let names: Vec<String> = ["Folder.JPG", "poster.png", "movie.mkv", "fanart.txt"]
            .map(String::from)
            .to_vec();
        let dir = Path::new("/m");
        let bases = ["poster".to_owned(), "folder".to_owned()];
        assert_eq!(find_image(dir, &names, &bases), Some(PathBuf::from("/m/poster.png")));
        assert_eq!(find_image(dir, &names, &["fanart".to_owned()]), None);
    }
}
