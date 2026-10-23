//! Helpers for integration tests that need a database or ffmpeg.
//!
//! Set `FERROCINE_TEST_POSTGRES` and/or `FERROCINE_TEST_MARIADB` to
//! connection URLs; tests are skipped for unset variables. Tests that need
//! media files are skipped when `ffmpeg` is not in `PATH`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use ferrocine::config::{DatabaseConfig, Dur};
use ferrocine::store::Store;

pub fn database_urls() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (name, var) in [
        ("postgres", "FERROCINE_TEST_POSTGRES"),
        ("mariadb", "FERROCINE_TEST_MARIADB"),
    ] {
        match std::env::var(var) {
            Ok(url) if !url.is_empty() => out.push((name, url)),
            _ => eprintln!("{var} is not set; skipping {name}"),
        }
    }
    out
}

pub fn db_config(url: &str) -> DatabaseConfig {
    DatabaseConfig {
        url: url.to_owned(),
        max_connections: 8,
        connect_timeout: Dur::secs(10),
    }
}

pub async fn store(url: &str) -> Store {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let store = Store::connect(&db_config(url)).await.expect("connect");
    store.migrate().await.expect("migrate");
    store
}

/// A name unique to this test run, so tests can share a database.
pub fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}-{}-{n:08x}", std::process::id())
}

pub fn have_ffmpeg() -> bool {
    let ok = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("ffmpeg is not available; skipping");
    }
    ok
}

/// Write a short test video with ffmpeg.
pub fn video(path: &Path, seconds: u32, args: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=640x360:rate=25:duration={seconds}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .args(args)
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "cannot create {}", path.display());
}

pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A small PNG.
pub fn image(path: &Path, w: u32, h: u32) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("color=c=blue:size={w}x{h}"))
        .args(["-frames:v", "1"])
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
}

pub struct Media {
    pub root: PathBuf,
    pub movies: PathBuf,
    pub shows: PathBuf,
}

/// A movie library and a show library exercising the scanner's cases.
pub fn media() -> Media {
    let root = std::env::temp_dir().join(unique("fc-media"));
    let movies = root.join("movies");
    let shows = root.join("shows");
    let h264_aac = [
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-shortest",
    ];
    // Direct play: MP4 with H.264 and AAC, a local poster, an .nfo and a sidecar subtitle.
    let matrix = movies.join("The Matrix (1999)");
    video(&matrix.join("The Matrix (1999).mp4"), 12, &h264_aac);
    image(&matrix.join("poster.png"), 400, 600);
    write(
        &matrix.join("movie.nfo"),
        "<movie><title>The Matrix</title><plot>A hacker learns the truth.</plot><genre>Action</genre>\
         <genre>Science Fiction</genre><rating>8.7</rating><uniqueid type=\"imdb\">tt0133093</uniqueid></movie>",
    );
    write(
        &matrix.join("The Matrix (1999).en.srt"),
        "1\n00:00:01,000 --> 00:00:03,000\nWake up, Neo.\n\n2\n00:00:08,000 --> 00:00:10,000\nFollow the white rabbit.\n",
    );
    // A show misplaced in the movie library, hidden by an empty .ignore (Jellyfin's convention).
    let stray = movies.join("Stray Show");
    video(&stray.join("Stray Show - S01E01.mkv"), 1, &h264_aac);
    write(&stray.join(".ignore"), "");
    // Remux: H.264 in Matroska, release name at the root, with an embedded subtitle.
    let alien = movies.join("Alien.1979.1080p.BluRay.x264.mkv");
    let srt = root.join("alien.srt");
    write(
        &srt,
        "1\n00:00:02,000 --> 00:00:04,000\nIn space no one can hear you scream.\n",
    );
    std::fs::create_dir_all(&movies).unwrap();
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=25:duration=12",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=330:duration=12", "-i"])
        .arg(&srt)
        .args([
            "-map",
            "0",
            "-map",
            "1",
            "-map",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-c:a",
            "aac",
            "-c:s",
            "srt",
            "-metadata:s:s:0",
            "language=eng",
            "-t",
            "12",
        ])
        .arg(&alien)
        .status()
        .unwrap();
    assert!(status.success());
    // Transcode: MPEG-4 part 2 and MP3 in AVI.
    video(
        &movies.join("Old Film (1950)/Old Film (1950).avi"),
        12,
        &["-c:v", "mpeg4", "-q:v", "5", "-c:a", "libmp3lame", "-shortest"],
    );
    // Tone mapping: HEVC 10-bit HDR10 (PQ, BT.2020, mastering metadata). libx265
    // writes the colour description from its own parameters.
    video(
        &movies.join("Nebula (2024)/Nebula (2024).mkv"),
        8,
        &[
            "-vf",
            "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:max-cll=1000,400:\
             master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1)",
            "-pix_fmt",
            "yuv420p10le",
            "-c:a",
            "aac",
            "-shortest",
        ],
    );
    // Extras are ignored.
    video(&movies.join("Old Film (1950)/Extras/Making of.mp4"), 2, &h264_aac);

    let series = shows.join("Breaking Test (2008)");
    write(
        &series.join("tvshow.nfo"),
        "<tvshow><title>Breaking Test</title><plot>A chemistry teacher.</plot><genre>Drama</genre></tvshow>",
    );
    image(&series.join("poster.jpg"), 300, 450);
    video(
        &series.join("Season 01/Breaking Test - S01E01 - Pilot.mkv"),
        10,
        &h264_aac,
    );
    video(&series.join("Season 01/Breaking Test - S01E02.mkv"), 10, &h264_aac);
    video(&series.join("Season 02/s02e01.mp4"), 10, &h264_aac);
    video(&series.join("Specials/S00E01 - Behind the scenes.mp4"), 10, &h264_aac);
    Media { root, movies, shows }
}
