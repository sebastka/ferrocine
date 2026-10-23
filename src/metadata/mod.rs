//! Remote metadata providers and how they are combined.
//!
//! - Movies: TMDB (AniList first in anime libraries), then OMDb ratings and
//!   fanart.tv artwork.
//! - Series: AniList first in anime libraries; TheTVDB first when a library
//!   uses another episode order than broadcast; then TMDB; then TheTVDB to
//!   fill gaps; then OMDb and fanart.tv.
//! - Seasons and episodes: AniDB (anime), else TheTVDB in the library's
//!   order, else TMDB, else TheTVDB in broadcast order.
//!
//! Each provider only fills what is still missing (`Meta::fill_from`), so the
//! first one that knows a field wins; `.nfo` files win over all of them.

pub mod anidb;
pub mod anilist;
pub mod fanart;
pub mod omdb;
pub mod tmdb;
pub mod tvdb;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::config::{Config, EpisodeOrder, LibraryConfig};
use crate::media::Meta;
use crate::store::ImageKind;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Largest image downloaded from a provider.
pub const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

/// An image offered by a provider; earlier entries are preferred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteImage {
    pub kind: ImageKind,
    pub url: String,
}

/// Remote metadata of a movie, series or episode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteItem {
    pub meta: Meta,
    pub images: Vec<RemoteImage>,
}

impl RemoteItem {
    /// Merge `other` after `self`: missing fields and further image candidates.
    pub fn merge(&mut self, other: RemoteItem) {
        self.meta.fill_from(&other.meta, true);
        self.images.extend(other.images);
    }

    /// Put `images` before the current candidates of their kinds.
    pub fn prefer_images(&mut self, images: Vec<RemoteImage>) {
        let mut merged = images;
        merged.append(&mut self.images);
        self.images = merged;
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteSeason {
    pub meta: Meta,
    pub images: Vec<RemoteImage>,
    pub episodes: HashMap<i32, RemoteItem>,
}

/// State kept across scans: AniDB's title dump and request pacing, and the
/// TheTVDB token.
#[derive(Default)]
pub struct ProviderState {
    pub anidb: anidb::State,
    pub tvdb: tvdb::State,
    pub opensubtitles: crate::subtitles::opensubtitles::State,
}

/// GET a JSON document; `None` on 404. Retries on 429 (honouring
/// `Retry-After`, at most 10 s) and on 5xx, three times.
pub async fn get_json(req: reqwest::RequestBuilder, what: &str) -> anyhow::Result<Option<Value>> {
    let mut attempt = 0;
    loop {
        let Some(r) = req.try_clone() else {
            bail!("{what}: request cannot be retried")
        };
        let resp = r.timeout(TIMEOUT).send().await.with_context(|| what.to_owned())?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return Ok(Some(
                resp.json().await.with_context(|| format!("{what}: invalid JSON"))?,
            ));
        }
        if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) && attempt < 3 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(2)
                .min(10);
            attempt += 1;
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        bail!("{what}: HTTP {status}");
    }
}

/// Download an image (no credentials), capped at [`MAX_IMAGE_BYTES`].
pub async fn download(http: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = http.get(url).timeout(TIMEOUT).send().await?;
    if !resp.status().is_success() {
        bail!("{url}: HTTP {}", resp.status());
    }
    if resp.content_length().is_some_and(|l| l > MAX_IMAGE_BYTES as u64) {
        bail!("{url}: image too large");
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > MAX_IMAGE_BYTES {
            bail!("{url}: image too large");
        }
    }
    Ok(body)
}

/// Strip HTML tags and entities that some providers leave in descriptions.
pub fn plain_text(s: &str) -> String {
    // Line breaks first, so that removing tags keeps paragraphs apart.
    let s = s.replace("<br />", "\n").replace("<br/>", "\n").replace("<br>", "\n");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    // Collapse the blank lines left by removed <br>s.
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    let mut joined = lines.join("\n");
    while joined.contains("\n\n\n") {
        joined = joined.replace("\n\n\n", "\n\n");
    }
    joined.trim().to_owned()
}

/// The providers configured for one scan.
pub struct Providers {
    pub tmdb: Option<tmdb::Tmdb>,
    pub tvdb: Option<tvdb::Tvdb>,
    pub omdb: Option<omdb::Omdb>,
    pub fanart: Option<fanart::Fanart>,
    pub anilist: Option<anilist::Anilist>,
    pub anidb: Option<anidb::Anidb>,
    pub http: reqwest::Client,
    limit: Arc<Semaphore>,
}

/// Ids of a series at the providers, from its stored metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeriesIds {
    pub tmdb: Option<String>,
    pub tvdb: Option<String>,
    pub anidb: Option<String>,
}

impl SeriesIds {
    pub fn of(meta: &Meta) -> Self {
        Self {
            tmdb: meta.tmdb_id.clone(),
            tvdb: meta.tvdb_id.clone(),
            anidb: meta.anidb_id.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.tvdb.is_none() && self.anidb.is_none()
    }
}

impl Providers {
    /// The providers of `cfg`; `None` when none is configured.
    pub fn new(cfg: &Config, http: &reqwest::Client, state: &Arc<ProviderState>) -> Option<Self> {
        let m = &cfg.metadata;
        let anime_libraries = cfg.libraries.iter().any(|l| l.anime);
        let p = Self {
            tmdb: m.tmdb.clone().map(|c| tmdb::Tmdb::new(c, http.clone())),
            tvdb: m.tvdb.clone().map(|c| tvdb::Tvdb::new(c, http.clone(), state.clone())),
            omdb: m.omdb.clone().map(|c| omdb::Omdb::new(c, http.clone())),
            fanart: m.fanart.clone().map(|c| fanart::Fanart::new(c, http.clone())),
            anilist: (m.anilist.enabled && anime_libraries)
                .then(|| anilist::Anilist::new(m.anilist.clone(), http.clone())),
            anidb: m
                .anidb
                .clone()
                .map(|c| anidb::Anidb::new(c, http.clone(), state.clone())),
            http: http.clone(),
            limit: Arc::new(Semaphore::new(8)),
        };
        let any = p.tmdb.is_some()
            || p.tvdb.is_some()
            || p.omdb.is_some()
            || p.fanart.is_some()
            || p.anilist.is_some()
            || p.anidb.is_some();
        any.then_some(p)
    }

    fn language<'a>(&'a self, lib: &'a LibraryConfig) -> Option<&'a str> {
        lib.language
            .as_deref()
            .or_else(|| self.tmdb.as_ref().map(|t| t.language_default()))
    }

    /// Whether a provider of series and episodes exists for this library.
    pub fn has_series_source(&self, lib: &LibraryConfig) -> bool {
        self.tmdb.is_some() || self.tvdb.is_some() || (lib.anime && (self.anilist.is_some() || self.anidb.is_some()))
    }

    /// Log a failed lookup and go on: one provider being down must not stop the others.
    fn ok<T>(r: anyhow::Result<Option<T>>, provider: &str, title: &str) -> Option<T> {
        match r {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(provider, title, error = %format!("{e:#}"), "metadata lookup failed");
                None
            }
        }
    }

    /// Like [`Self::ok`], remembering the failure: "no match" and "provider
    /// down" must not be confused (an unmatched item is only retried a week later).
    fn tried<T>(r: anyhow::Result<Option<T>>, provider: &str, title: &str, failed: &mut bool) -> Option<T> {
        if r.is_err() {
            *failed = true;
        }
        Self::ok(r, provider, title)
    }

    fn merge_into(found: &mut Option<RemoteItem>, r: Option<RemoteItem>) {
        if let Some(r) = r {
            match found {
                Some(f) => f.merge(r),
                None => *found = Some(r),
            }
        }
    }

    fn with_found(hint: &Meta, found: &Option<RemoteItem>) -> Meta {
        let mut h = hint.clone();
        if let Some(f) = found {
            h.fill_from(&f.meta, true);
        }
        h
    }

    /// Remote metadata of a movie; an error when nothing was found because a
    /// provider failed.
    pub async fn movie(&self, hint: &Meta, lib: &LibraryConfig) -> anyhow::Result<Option<RemoteItem>> {
        let _permit = self.limit.acquire().await?;
        let lang = self.language(lib);
        let mut failed = false;
        let mut found: Option<RemoteItem> = None;
        if lib.anime
            && let Some(a) = &self.anilist
        {
            found = Self::tried(a.search(hint, true).await, "anilist", &hint.title, &mut failed);
        }
        if let Some(t) = &self.tmdb {
            let h = Self::with_found(hint, &found);
            let r = Self::tried(t.movie(&h, lang).await, "tmdb", &hint.title, &mut failed);
            Self::merge_into(&mut found, r);
        }
        let Some(mut item) = found else {
            if failed {
                bail!("no provider answered");
            }
            return Ok(None);
        };
        self.enrich(&mut item, hint, lib, false).await;
        Ok(Some(item))
    }

    /// Remote metadata of a series; an error when nothing was found because
    /// a provider failed.
    pub async fn series(&self, hint: &Meta, lib: &LibraryConfig) -> anyhow::Result<Option<RemoteItem>> {
        let _permit = self.limit.acquire().await?;
        let lang = self.language(lib);
        let mut failed = false;
        let mut found: Option<RemoteItem> = None;
        if lib.anime {
            if let Some(a) = &self.anilist {
                let r = Self::tried(a.search(hint, false).await, "anilist", &hint.title, &mut failed);
                Self::merge_into(&mut found, r);
            }
            if let Some(a) = &self.anidb {
                let h = Self::with_found(hint, &found);
                let r = Self::tried(a.series(&h).await, "anidb", &hint.title, &mut failed);
                Self::merge_into(&mut found, r);
            }
        }
        let tvdb_first = lib.episode_order != EpisodeOrder::Aired;
        if tvdb_first && let Some(t) = &self.tvdb {
            let h = Self::with_found(hint, &found);
            let r = Self::tried(t.series(&h, lang).await, "tvdb", &hint.title, &mut failed);
            Self::merge_into(&mut found, r);
        }
        if let Some(t) = &self.tmdb {
            let h = Self::with_found(hint, &found);
            let r = Self::tried(t.series(&h, lang).await, "tmdb", &hint.title, &mut failed);
            Self::merge_into(&mut found, r);
        }
        if !tvdb_first && let Some(t) = &self.tvdb {
            let h = Self::with_found(hint, &found);
            let r = Self::tried(t.series(&h, lang).await, "tvdb", &hint.title, &mut failed);
            Self::merge_into(&mut found, r);
        }
        let Some(mut item) = found else {
            if failed {
                bail!("no provider answered");
            }
            return Ok(None);
        };
        self.enrich(&mut item, hint, lib, true).await;
        Ok(Some(item))
    }

    /// OMDb ratings and fanart.tv artwork, once the ids are known.
    async fn enrich(&self, item: &mut RemoteItem, hint: &Meta, lib: &LibraryConfig, series: bool) {
        let imdb = item.meta.imdb_id.clone().or_else(|| hint.imdb_id.clone());
        if let (Some(o), Some(imdb)) = (&self.omdb, imdb)
            && let Some(r) = Self::ok(o.lookup(&imdb).await, "omdb", &hint.title)
        {
            item.merge(r);
        }
        if let Some(f) = &self.fanart {
            let lang = self.language(lib);
            let images = if series {
                match item.meta.tvdb_id.clone().or_else(|| hint.tvdb_id.clone()) {
                    Some(tvdb) => Self::ok(f.series(&tvdb, lang).await, "fanart", &hint.title),
                    None => None,
                }
            } else {
                let id = item
                    .meta
                    .tmdb_id
                    .clone()
                    .or_else(|| item.meta.imdb_id.clone())
                    .or_else(|| hint.tmdb_id.clone());
                match id {
                    Some(id) => Self::ok(f.movie(&id, lang).await, "fanart", &hint.title),
                    None => None,
                }
            };
            if let Some(images) = images {
                // fanart.tv's backdrops and logos first; its posters after the others.
                let (first, later): (Vec<_>, Vec<_>) = images.into_iter().partition(|i| i.kind != ImageKind::Poster);
                item.prefer_images(first);
                item.images.extend(later);
            }
        }
    }

    /// A season's metadata and its episodes, from the best source for the library.
    pub async fn season(&self, ids: &SeriesIds, number: i32, lib: &LibraryConfig) -> Option<RemoteSeason> {
        let _permit = self.limit.acquire().await.ok()?;
        let lang = self.language(lib);
        let name = format!("season {number}");
        if lib.anime
            && let (Some(a), Some(aid)) = (&self.anidb, &ids.anidb)
            && let Some(s) = Self::ok(a.season(aid, number).await, "anidb", &name)
        {
            return Some(s);
        }
        if lib.episode_order != EpisodeOrder::Aired
            && let (Some(t), Some(id)) = (&self.tvdb, &ids.tvdb)
            && let Some(s) = Self::ok(t.season(id, number, lib.episode_order, lang).await, "tvdb", &name)
        {
            return Some(s);
        }
        if let (Some(t), Some(id)) = (&self.tmdb, &ids.tmdb)
            && let Some(s) = Self::ok(t.season(id, number, lang).await, "tmdb", &name)
        {
            return Some(s);
        }
        if let (Some(t), Some(id)) = (&self.tvdb, &ids.tvdb) {
            return Self::ok(t.season(id, number, EpisodeOrder::Aired, lang).await, "tvdb", &name);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        assert_eq!(
            plain_text("A <i>hero</i>.<br>\n<br>\n<br>\n(Source: X) &amp; more &#039;quoted&#039;"),
            "A hero.\n\n(Source: X) & more 'quoted'"
        );
    }

    #[test]
    fn preferred_images_come_first() {
        let img = |k, u: &str| RemoteImage { kind: k, url: u.into() };
        let mut r = RemoteItem {
            images: vec![img(ImageKind::Backdrop, "tmdb")],
            ..Default::default()
        };
        r.prefer_images(vec![img(ImageKind::Backdrop, "fanart")]);
        assert_eq!(r.images[0].url, "fanart");
    }
}
