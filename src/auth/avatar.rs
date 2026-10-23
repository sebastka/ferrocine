//! The user's picture, from the OpenID Connect `picture` claim: fetched at
//! sign-in, cropped to a square, resized and stored as JPEG, so that the UI
//! serves it from its own origin (its Content Security Policy allows no
//! other) and the provider is not contacted on every page view.
//!
//! The URL comes from the provider's signed token, but users may be able to
//! set it there: only https is fetched (http on localhost, as for the
//! issuer), with a short timeout, a size cap and few redirects, and only a
//! decoded and re-encoded image is ever kept, so nothing else read from that
//! URL can come back to the user.

use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Side of the stored square, enough for high-density screens.
const SIDE: u32 = 192;

/// Whether a picture URL may be fetched.
pub fn allowed(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) if u.scheme() == "https" => u.host_str().is_some(),
        Ok(u) if u.scheme() == "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

/// Crop to the centred square, resize and encode as JPEG. CPU-bound.
pub fn square(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let img = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()
        .context("not an image")?;
    let side = img.width().min(img.height());
    let img = img
        .crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side)
        .resize_exact(SIDE.min(side), SIDE.min(side), FilterType::CatmullRom);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img.to_rgb8())?;
    Ok(out)
}

/// Download and process the picture at `url`.
pub async fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    if !allowed(url) {
        bail!("picture URL is not https");
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .timeout(TIMEOUT)
        .user_agent(concat!("Ferrocine/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    if resp.content_length().is_some_and(|l| l > MAX_BYTES as u64) {
        bail!("picture too large");
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > MAX_BYTES {
            bail!("picture too large");
        }
    }
    tokio::task::spawn_blocking(move || square(&body)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_local_http() {
        assert!(allowed("https://id.example.com/avatar/1.png"));
        assert!(allowed("http://127.0.0.1:8080/a.png"));
        assert!(!allowed("http://id.example.com/a.png"));
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("not a url"));
    }

    #[test]
    fn crops_to_a_square() {
        let img = image::RgbImage::from_pixel(400, 300, image::Rgb([10, 200, 30]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let jpeg = square(&png).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (SIDE, SIDE));
        assert!(square(b"<html>").is_err());
    }
}
