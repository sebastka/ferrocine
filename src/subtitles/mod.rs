//! Subtitles from OpenSubtitles.com: searched on demand from the player, or
//! fetched automatically during scans for the configured languages. They are
//! stored in the database as WebVTT; libraries stay read-only.

pub mod opensubtitles;

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, LibraryConfig};
use crate::media::ItemKind;
use crate::metadata::ProviderState;
use crate::store::{ItemRow, NewSubtitle, Store, now_ms};
pub use opensubtitles::{Candidate, OpenSubtitles, SearchQuery};

/// Subtitle stream indices of stored subtitles start here (embedded streams
/// use their index in the file, sidecar files 1000 and up).
pub const STORED_BASE: i32 = 1_000_000;

/// The OpenSubtitles hash: the file size plus the 64-bit little-endian
/// words of its first and last 64 KiB, wrapping; 16 hex digits.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    const CHUNK: u64 = 65_536;
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let mut sum = size;
    let mut add = |buf: &[u8]| {
        for word in buf.chunks(8) {
            let mut w = [0u8; 8];
            w[..word.len()].copy_from_slice(word);
            sum = sum.wrapping_add(u64::from_le_bytes(w));
        }
    };
    let mut buf = vec![0u8; CHUNK.min(size) as usize];
    f.read_exact(&mut buf)?;
    add(&buf);
    f.seek(SeekFrom::Start(size.saturating_sub(CHUNK)))?;
    let mut buf = vec![0u8; CHUNK.min(size) as usize];
    f.read_exact(&mut buf)?;
    add(&buf);
    Ok(format!("{sum:016x}"))
}

/// SRT to WebVTT: a header, `.` decimal separators, and no formatting
/// browsers do not support (`<font>`, ASS override tags `{\an8}`).
pub fn srt_to_vtt(srt: &str) -> String {
    let text = srt
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut out = String::with_capacity(text.len() + 16);
    out.push_str("WEBVTT\n\n");
    for line in text.lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            let mut cleaned = String::with_capacity(line.len());
            let mut rest = line;
            while let Some(i) = rest.find(['<', '{']) {
                cleaned.push_str(&rest[..i]);
                let tail = &rest[i..];
                let (open, close) = if tail.starts_with('{') { ('{', '}') } else { ('<', '>') };
                let tag = tail.find(close).map(|e| &tail[..=e]);
                match tag {
                    Some(t) if open == '{' && t.starts_with("{\\") => rest = &tail[t.len()..],
                    Some(t) if t.to_lowercase().starts_with("<font") || t.to_lowercase() == "</font>" => {
                        rest = &tail[t.len()..]
                    }
                    _ => {
                        cleaned.push(open);
                        rest = &tail[1..];
                    }
                }
            }
            cleaned.push_str(rest);
            out.push_str(&cleaned);
        }
        out.push('\n');
    }
    out
}

/// The candidate to download automatically: matched by hash first, then
/// human translations, the hearing-impaired preference, trusted uploaders,
/// and downloads.
pub fn best(candidates: &[Candidate], hearing_impaired: bool) -> Option<&Candidate> {
    candidates.iter().min_by_key(|c| {
        (
            !c.hash_match,
            c.machine_translated || c.ai_translated,
            c.hearing_impaired != hearing_impaired,
            !c.from_trusted,
            -(c.downloads as i64),
        )
    })
}

/// Whether an item already has subtitles in `language` that the web player
/// can show (text tracks, embedded or sidecar, or stored).
fn has_language(row: &ItemRow, stored: &[String], language: &str) -> bool {
    let in_file = row.media.as_ref().is_some_and(|m| {
        m.subtitles
            .iter()
            .any(|s| s.text && s.language.as_deref().is_some_and(|l| crate::lang::same(l, language)))
    });
    in_file || stored.iter().any(|l| crate::lang::same(l, language))
}

fn audio_in(row: &ItemRow, language: &str) -> bool {
    row.media
        .as_ref()
        .and_then(|m| m.audio_stream(None))
        .and_then(|a| a.language.as_deref())
        .is_some_and(|l| crate::lang::same(l, language))
}

/// What to search for an item: its hash and ids (the series' ids and the
/// episode's numbers for an episode).
pub fn query_for(row: &ItemRow, series: Option<&ItemRow>, hash: Option<String>, languages: &[String]) -> SearchQuery {
    let mut q = SearchQuery {
        moviehash: hash,
        languages: languages.to_vec(),
        ..Default::default()
    };
    match (row.kind, series) {
        (ItemKind::Episode, Some(s)) => {
            q.parent_imdb_id = s.meta.imdb_id.clone();
            q.parent_tmdb_id = s.meta.tmdb_id.clone();
            q.season_number = row.season_number;
            q.episode_number = row.episode_number;
            if q.parent_imdb_id.is_none() && q.parent_tmdb_id.is_none() {
                q.query = Some(s.meta.title.clone());
            }
        }
        _ => {
            q.imdb_id = row.meta.imdb_id.clone();
            q.tmdb_id = row.meta.tmdb_id.clone();
            if q.imdb_id.is_none() && q.tmdb_id.is_none() {
                q.query = Some(row.meta.title.clone());
                q.year = row.meta.year;
            }
        }
    }
    q
}

/// Fetch subtitles for a library's movies and episodes that lack one of the
/// configured languages. Returns the number downloaded.
pub async fn automatic(
    store: &Store,
    cfg: &Config,
    lib: &LibraryConfig,
    http: &reqwest::Client,
    state: &Arc<ProviderState>,
) -> anyhow::Result<u32> {
    let s = &cfg.subtitles;
    let (Some(os_cfg), false) = (&s.opensubtitles, s.languages.is_empty()) else {
        return Ok(0);
    };
    let os = OpenSubtitles::new(os_cfg.clone(), http.clone(), state.clone());
    let rows = store.library_items(&lib.id).await?;
    let series: HashMap<i64, &ItemRow> = rows
        .iter()
        .filter(|r| r.kind == ItemKind::Series)
        .map(|r| (r.id, r))
        .collect();
    let retry = cfg.scanner.retry_unmatched.as_millis_i64();
    let mut downloads = 0u32;
    for row in rows.iter().filter(|r| r.kind.playable() && r.media.is_some()) {
        let Some(path) = row.path.as_deref() else { continue };
        let stored: Vec<String> = store
            .subtitles_of(row.id)
            .await?
            .into_iter()
            .map(|s| s.language)
            .collect();
        let mut hash: Option<Option<String>> = None;
        for language in &s.languages {
            if has_language(row, &stored, language) || (s.skip_when_audio_matches && audio_in(row, language)) {
                continue;
            }
            let now = now_ms();
            match store.subtitle_search(row.id, language).await? {
                Some((_, true)) => continue,
                Some((at, false)) if now - at < retry => continue,
                _ => {}
            }
            if downloads >= s.max_downloads_per_scan {
                tracing::info!(library = %lib.id, downloads, "subtitle downloads per scan reached; the rest waits for the next scan");
                return Ok(downloads);
            }
            if hash.is_none() && crate::media::disc::is_dvd(Path::new(path)) {
                // A DVD part's hash is not the movie's.
                hash = Some(None);
            }
            if hash.is_none() {
                let p = path.to_owned();
                hash = Some(tokio::task::spawn_blocking(move || hash_file(Path::new(&p)).ok()).await?);
            }
            let q = query_for(
                row,
                row.series_id.and_then(|id| series.get(&id).copied()),
                hash.clone().flatten(),
                std::slice::from_ref(language),
            );
            let candidates = match os.search(&q).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "OpenSubtitles search failed; automatic subtitles stop for this scan");
                    return Ok(downloads);
                }
            };
            let Some(choice) = best(&candidates, s.hearing_impaired) else {
                store.record_subtitle_search(row.id, language, false, now).await?;
                continue;
            };
            match os.download(choice.file_id).await {
                Ok(d) => {
                    store
                        .add_subtitle(
                            &NewSubtitle {
                                item_id: row.id,
                                language,
                                source: "opensubtitles",
                                source_id: &choice.file_id.to_string(),
                                release_name: choice.release.as_deref(),
                                hearing_impaired: choice.hearing_impaired,
                                forced: choice.foreign_parts_only,
                                hash_match: choice.hash_match,
                                content: &d.vtt,
                                created_by: "auto",
                            },
                            now,
                        )
                        .await?;
                    store.record_subtitle_search(row.id, language, true, now).await?;
                    downloads += 1;
                    tracing::info!(item = row.id, title = %row.meta.title, language = %language, remaining = ?d.remaining, "subtitle downloaded");
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "OpenSubtitles download failed; automatic subtitles stop for this scan");
                    return Ok(downloads);
                }
            }
            // OpenSubtitles allows a few requests per second.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    Ok(downloads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_opensubtitles() {
        let dir = std::env::temp_dir().join(format!("fc-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        // Independent computation of the same definition.
        let words = |b: &[u8]| {
            b.chunks(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                .fold(0u64, u64::wrapping_add)
        };
        let expected = (data.len() as u64)
            .wrapping_add(words(&data[..65_536]))
            .wrapping_add(words(&data[data.len() - 65_536..]));
        assert_eq!(hash_file(&path).unwrap(), format!("{expected:016x}"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn converts_srt() {
        let srt = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<font color=\"#fff\">{\\an8}Hello</font> <i>there</i>\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nA {curly} <3\r\n";
        assert_eq!(
            srt_to_vtt(srt),
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello <i>there</i>\n\n2\n00:00:03.000 --> 00:00:04.000\nA {curly} <3\n"
        );
    }

    #[test]
    fn chooses_hash_matches_and_human_translations() {
        let c = |id: i64, hash: bool, machine: bool, downloads: u64| Candidate {
            file_id: id,
            hash_match: hash,
            machine_translated: machine,
            downloads,
            ..Default::default()
        };
        let list = [c(1, false, false, 9000), c(2, true, true, 50), c(3, true, false, 10)];
        assert_eq!(best(&list, false).unwrap().file_id, 3);
        assert!(best(&[], false).is_none());
    }
}
