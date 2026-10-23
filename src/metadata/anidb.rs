//! AniDB HTTP API: anime and their episodes in AniDB's numbering (each
//! season is an anime of its own there).
//!
//! AniDB bans clients that ask too much, so this client:
//! - finds anime by name in the daily title dump, downloaded at most once a
//!   day and kept in memory (the API itself cannot search);
//! - waits 2.5 s between API requests, across the whole process;
//! - caches every anime it fetched for a day.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use quick_xml::Reader;
use quick_xml::events::Event;
use tokio::sync::Mutex;

use super::{ProviderState, RemoteImage, RemoteItem, RemoteSeason, plain_text};
use crate::config::AnidbConfig;
use crate::media::{Meta, Person, Rating, year_of};
use crate::store::ImageKind;

const PACE: Duration = Duration::from_millis(2500);
const DAY: Duration = Duration::from_secs(86_400);
const TIMEOUT: Duration = Duration::from_secs(30);

/// A title of the dump: (aid, normalised title).
type Titles = Arc<Vec<(u32, String)>>;

#[derive(Default)]
pub struct State {
    titles: Mutex<Option<(Instant, Titles)>>,
    last_request: Mutex<Option<Instant>>,
    cache: Mutex<HashMap<u32, (Instant, Arc<Anime>)>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Episode {
    /// 1: regular, 2: special (`S1`), others ignored.
    pub kind: u8,
    pub number: i32,
    pub titles: Vec<(String, String)>,
    pub airdate: Option<String>,
    pub length_minutes: Option<i64>,
    pub rating: Option<f64>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Anime {
    pub aid: u32,
    pub kind: Option<String>,
    /// (language, type, title).
    pub titles: Vec<(String, String, String)>,
    pub startdate: Option<String>,
    pub description: Option<String>,
    pub rating: Option<f64>,
    pub picture: Option<String>,
    /// (weight, name).
    pub tags: Vec<(i64, String)>,
    /// (role, name).
    pub creators: Vec<(String, String)>,
    pub episodes: Vec<Episode>,
}

/// Lower case, letters and digits only, single spaces.
pub fn normalise(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut space = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if space && !out.is_empty() {
                out.push(' ');
            }
            out.push(c);
            space = false;
        } else {
            space = true;
        }
    }
    out
}

fn gunzip_if_needed(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(256 * 1024 * 1024)
            .read_to_end(&mut out)
            .context("invalid gzip")?;
        Ok(out)
    } else {
        Ok(bytes)
    }
}

/// `<animetitles><anime aid="1"><title …>Name</title>…` → (aid, normalised).
pub fn parse_titles(xml: &str) -> Vec<(u32, String)> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut aid = 0u32;
    let mut in_title = false;
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                "anime" => {
                    aid = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == "aid")
                        .and_then(|a| a.value.parse().ok())
                        .unwrap_or(0);
                }
                "title" => {
                    in_title = true;
                    buf.clear();
                }
                _ => {}
            },
            Ok(Event::Text(t)) if in_title => buf.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) if in_title => buf.push_str(entity(&r)),
            Ok(Event::End(e)) if e.local_name().as_ref() == "title" => {
                in_title = false;
                if aid > 0 {
                    out.push((aid, normalise(&buf)));
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn entity(r: &quick_xml::events::BytesRef<'_>) -> &'static str {
    match &**r {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    }
}

/// AniDB descriptions link characters as `http://anidb.net/ch123 [Name]`.
fn strip_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("http") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        match (tail.find(" ["), tail.find(']')) {
            (Some(a), Some(b)) if a < b && !tail[..a].contains(char::is_whitespace) => {
                out.push_str(&tail[a + 2..b]);
                rest = &tail[b + 1..];
            }
            _ => {
                out.push_str("http");
                rest = &tail[4..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse a `request=anime` answer.
pub fn parse_anime(xml: &str) -> anyhow::Result<Anime> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut a = Anime::default();
    let mut stack: Vec<String> = Vec::new();
    let mut attrs: Vec<HashMap<String, String>> = Vec::new();
    let mut text = String::new();
    let mut episode: Option<Episode> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_owned();
                let map: HashMap<String, String> = e
                    .attributes()
                    .flatten()
                    .map(|x| (x.key.as_ref().to_owned(), x.value.into_owned()))
                    .collect();
                if name == "anime" {
                    a.aid = map.get("id").and_then(|i| i.parse().ok()).unwrap_or(0);
                }
                if name == "episode" {
                    episode = Some(Episode::default());
                }
                if name == "error" {
                    stack.push(name);
                    attrs.push(map);
                    text.clear();
                    continue;
                }
                stack.push(name);
                attrs.push(map);
                text.clear();
            }
            Ok(Event::Text(t)) => text.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) => text.push_str(entity(&r)),
            Ok(Event::End(_)) => {
                let Some(name) = stack.pop() else { continue };
                let map = attrs.pop().unwrap_or_default();
                let parent = stack.last().map(String::as_str).unwrap_or_default();
                let value = text.trim().to_owned();
                text.clear();
                let lang = map.get("xml:lang").cloned().unwrap_or_default();
                match (parent, name.as_str()) {
                    (_, "error") => bail!("AniDB: {value}"),
                    ("anime", "type") => a.kind = Some(value),
                    ("anime", "startdate") => a.startdate = Some(value),
                    ("anime", "description") => a.description = Some(value),
                    ("anime", "picture") => a.picture = Some(value),
                    ("titles", "title") => {
                        a.titles
                            .push((lang, map.get("type").cloned().unwrap_or_default(), value));
                    }
                    ("ratings", "permanent") => a.rating = value.parse().ok(),
                    ("tag", "name") => {
                        let weight = attrs
                            .last()
                            .and_then(|m| m.get("weight"))
                            .and_then(|w| w.parse().ok())
                            .unwrap_or(0);
                        a.tags.push((weight, value));
                    }
                    ("creators", "name") => {
                        a.creators.push((map.get("type").cloned().unwrap_or_default(), value));
                    }
                    ("episode", "epno") => {
                        if let Some(e) = episode.as_mut() {
                            e.kind = map.get("type").and_then(|t| t.parse().ok()).unwrap_or(0);
                            e.number = value
                                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                                .parse()
                                .unwrap_or(0);
                        }
                    }
                    ("episode", "title") => {
                        if let Some(e) = episode.as_mut() {
                            e.titles.push((lang, value));
                        }
                    }
                    ("episode", "airdate") => {
                        if let Some(e) = episode.as_mut() {
                            e.airdate = Some(value);
                        }
                    }
                    ("episode", "length") => {
                        if let Some(e) = episode.as_mut() {
                            e.length_minutes = value.parse().ok();
                        }
                    }
                    ("episode", "rating") => {
                        if let Some(e) = episode.as_mut() {
                            e.rating = value.parse().ok();
                        }
                    }
                    ("episode", "summary") => {
                        if let Some(e) = episode.as_mut() {
                            e.summary = Some(value);
                        }
                    }
                    (_, "episode") => {
                        if let Some(e) = episode.take() {
                            a.episodes.push(e);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("AniDB: invalid XML: {e}"),
            _ => {}
        }
    }
    if a.aid == 0 {
        bail!("AniDB: no anime in the answer");
    }
    Ok(a)
}

/// English official title, else the main (romanised) one.
fn title_of(a: &Anime) -> Option<String> {
    let find = |lang: &str, kind: &str| {
        a.titles
            .iter()
            .find(|(l, k, _)| l == lang && k == kind)
            .map(|(_, _, t)| t.clone())
    };
    find("en", "official").or_else(|| a.titles.iter().find(|(_, k, _)| k == "main").map(|(_, _, t)| t.clone()))
}

fn episode_title(e: &Episode) -> Option<String> {
    ["en", "x-jat"]
        .iter()
        .find_map(|l| e.titles.iter().find(|(lang, _)| lang == l).map(|(_, t)| t.clone()))
}

pub fn to_remote(a: &Anime, image_url: &str) -> RemoteItem {
    let mut tags = a.tags.clone();
    tags.sort_by_key(|t| std::cmp::Reverse(t.0));
    let genres = tags
        .iter()
        .filter(|(w, _)| *w >= 400)
        .take(5)
        .map(|(_, n)| {
            let mut c = n.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect();
    let people = a
        .creators
        .iter()
        .filter(|(role, _)| {
            matches!(
                role.as_str(),
                "Direction" | "Original Work" | "Music" | "Animation Work"
            )
        })
        .map(|(role, name)| Person {
            name: name.clone(),
            role: match role.as_str() {
                "Direction" => "director",
                "Original Work" => "writer",
                "Music" => "composer",
                _ => "studio",
            }
            .into(),
            character: None,
        })
        .collect();
    RemoteItem {
        meta: Meta {
            title: title_of(a).unwrap_or_default(),
            original_title: a
                .titles
                .iter()
                .find(|(l, k, _)| l == "ja" && k == "official")
                .map(|(_, _, t)| t.clone()),
            original_language: a
                .titles
                .iter()
                .any(|(l, k, _)| l == "ja" && k == "official")
                .then(|| "ja".to_owned()),
            year: year_of(a.startdate.as_deref()),
            premiere_date: a.startdate.clone(),
            overview: a.description.as_deref().map(|d| plain_text(&strip_links(d))),
            rating: a.rating,
            ratings: a.rating.map(|r| Rating::new("anidb", r, 10.0)).into_iter().collect(),
            genres,
            people,
            anidb_id: Some(a.aid.to_string()),
            ..Default::default()
        },
        images: a
            .picture
            .as_ref()
            .map(|p| RemoteImage {
                kind: ImageKind::Poster,
                url: format!("{}/{p}", image_url.trim_end_matches('/')),
            })
            .into_iter()
            .collect(),
    }
}

/// Season 1 holds the regular episodes, season 0 the specials.
pub fn season_of(a: &Anime, number: i32) -> Option<RemoteSeason> {
    let kind = match number {
        1 => 1,
        0 => 2,
        _ => return None,
    };
    let episodes: HashMap<i32, RemoteItem> = a
        .episodes
        .iter()
        .filter(|e| e.kind == kind && e.number > 0)
        .map(|e| {
            (
                e.number,
                RemoteItem {
                    meta: Meta {
                        title: episode_title(e).unwrap_or_default(),
                        year: year_of(e.airdate.as_deref()),
                        premiere_date: e.airdate.clone(),
                        overview: e.summary.as_deref().map(|s| plain_text(&strip_links(s))),
                        rating: e.rating,
                        runtime_ms: e.length_minutes.map(|m| m * 60_000),
                        ..Default::default()
                    },
                    images: Vec::new(),
                },
            )
        })
        .collect();
    (!episodes.is_empty()).then_some(RemoteSeason {
        episodes,
        ..Default::default()
    })
}

pub struct Anidb {
    config: AnidbConfig,
    http: reqwest::Client,
    state: Arc<ProviderState>,
}

impl Anidb {
    pub fn new(config: AnidbConfig, http: reqwest::Client, state: Arc<ProviderState>) -> Self {
        Self { config, http, state }
    }

    async fn titles(&self) -> anyhow::Result<Titles> {
        let mut t = self.state.anidb.titles.lock().await;
        if let Some((at, titles)) = t.as_ref()
            && at.elapsed() < DAY
        {
            return Ok(titles.clone());
        }
        let resp = self.http.get(&self.config.titles_url).timeout(TIMEOUT).send().await?;
        if !resp.status().is_success() {
            bail!("AniDB title dump: HTTP {}", resp.status());
        }
        let bytes = gunzip_if_needed(resp.bytes().await?.to_vec())?;
        let xml = String::from_utf8_lossy(&bytes).into_owned();
        let titles = Arc::new(tokio::task::spawn_blocking(move || parse_titles(&xml)).await?);
        tracing::info!(titles = titles.len(), "AniDB title dump loaded");
        *t = Some((Instant::now(), titles.clone()));
        Ok(titles)
    }

    async fn anime(&self, aid: u32) -> anyhow::Result<Arc<Anime>> {
        if let Some((at, a)) = self.state.anidb.cache.lock().await.get(&aid)
            && at.elapsed() < DAY
        {
            return Ok(a.clone());
        }
        // One request at a time, 2.5 s apart, across the process.
        let mut last = self.state.anidb.last_request.lock().await;
        if let Some(t) = *last
            && t.elapsed() < PACE
        {
            tokio::time::sleep(PACE - t.elapsed()).await;
        }
        let resp = self
            .http
            .get(&self.config.api_url)
            .query(&[
                ("request", "anime".to_owned()),
                ("client", self.config.client.clone()),
                ("clientver", self.config.client_version.to_string()),
                ("protover", "1".to_owned()),
                ("aid", aid.to_string()),
            ])
            .timeout(TIMEOUT)
            .send()
            .await;
        *last = Some(Instant::now());
        drop(last);
        let resp = resp.context("AniDB")?;
        if !resp.status().is_success() {
            bail!("AniDB: HTTP {}", resp.status());
        }
        let bytes = gunzip_if_needed(resp.bytes().await?.to_vec())?;
        let anime = Arc::new(parse_anime(&String::from_utf8_lossy(&bytes))?);
        self.state
            .anidb
            .cache
            .lock()
            .await
            .insert(aid, (Instant::now(), anime.clone()));
        Ok(anime)
    }

    /// Find a series by AniDB id, else by exact title (several matches:
    /// the one that started within a year of the hint's year).
    pub async fn series(&self, hint: &Meta) -> anyhow::Result<Option<RemoteItem>> {
        let aid = match hint.anidb_id.as_ref().and_then(|a| a.parse::<u32>().ok()) {
            Some(aid) => Some(aid),
            None => {
                let titles = self.titles().await?;
                let wanted: Vec<String> = [Some(&hint.title), hint.original_title.as_ref()]
                    .into_iter()
                    .flatten()
                    .map(|t| normalise(t))
                    .collect();
                let mut aids: Vec<u32> = titles
                    .iter()
                    .filter(|(_, t)| wanted.contains(t))
                    .map(|(a, _)| *a)
                    .collect();
                aids.sort_unstable();
                aids.dedup();
                let mut chosen = None;
                for aid in aids.into_iter().take(3) {
                    let a = self.anime(aid).await?;
                    let year = year_of(a.startdate.as_deref());
                    if hint.year.is_none() || year.zip(hint.year).is_some_and(|(y, h)| (y - h).abs() <= 1) {
                        chosen = Some(aid);
                        break;
                    }
                }
                chosen
            }
        };
        let Some(aid) = aid else { return Ok(None) };
        let a = self.anime(aid).await?;
        Ok(Some(to_remote(&a, &self.config.image_url)))
    }

    pub async fn season(&self, aid: &str, number: i32) -> anyhow::Result<Option<RemoteSeason>> {
        let Ok(aid) = aid.parse::<u32>() else { return Ok(None) };
        Ok(season_of(&*self.anime(aid).await?, number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANIME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<anime id="17617" restricted="false">
  <type>TV Series</type>
  <episodecount>28</episodecount>
  <startdate>2023-09-29</startdate>
  <titles>
    <title xml:lang="x-jat" type="main">Sousou no Frieren</title>
    <title xml:lang="en" type="official">Frieren: Beyond Journey&apos;s End</title>
    <title xml:lang="ja" type="official">葬送のフリーレン</title>
  </titles>
  <creators><name id="1" type="Direction">Saitou Keiichirou</name><name id="2" type="Translator">X</name></creators>
  <description>The elf mage http://anidb.net/ch123 [Frieren] outlives her party.</description>
  <ratings><permanent count="5000">8.95</permanent></ratings>
  <picture>285468.jpg</picture>
  <tags><tag id="1" weight="600"><name>fantasy</name></tag><tag id="2" weight="0"><name>noise</name></tag></tags>
  <episodes>
    <episode id="1"><epno type="1">1</epno><length>25</length><airdate>2023-09-29</airdate>
      <rating votes="10">8.1</rating><title xml:lang="ja">冒険の終わり</title><title xml:lang="en">The Journey&apos;s End</title></episode>
    <episode id="2"><epno type="2">S1</epno><length>5</length><title xml:lang="en">Mini Special</title></episode>
  </episodes>
</anime>"#;

    #[test]
    fn parses_an_anime() {
        let a = parse_anime(ANIME).unwrap();
        assert_eq!((a.aid, a.episodes.len()), (17617, 2));
        let r = to_remote(&a, "https://cdn.example/images/main/");
        assert_eq!(r.meta.title, "Frieren: Beyond Journey's End");
        assert_eq!(r.meta.original_title.as_deref(), Some("葬送のフリーレン"));
        assert_eq!(
            r.meta.overview.as_deref(),
            Some("The elf mage Frieren outlives her party.")
        );
        assert_eq!(r.meta.genres, ["Fantasy"]);
        assert_eq!(r.meta.people.len(), 1);
        assert_eq!(r.images[0].url, "https://cdn.example/images/main/285468.jpg");
        let s1 = season_of(&a, 1).unwrap();
        assert_eq!(s1.episodes[&1].meta.title, "The Journey's End");
        assert_eq!(s1.episodes[&1].meta.runtime_ms, Some(25 * 60_000));
        assert_eq!(season_of(&a, 0).unwrap().episodes[&1].meta.title, "Mini Special");
        assert!(season_of(&a, 2).is_none());
        assert!(
            parse_anime("<error code=\"500\">Banned</error>")
                .unwrap_err()
                .to_string()
                .contains("Banned")
        );
    }

    #[test]
    fn titles_and_normalising() {
        let xml = r#"<animetitles><anime aid="17617"><title type="main" xml:lang="x-jat">Sousou no Frieren</title>
            <title type="official" xml:lang="en">Frieren: Beyond Journey&apos;s End</title></anime>
            <anime aid="1"><title type="main">Crest of the Stars</title></anime></animetitles>"#;
        let t = parse_titles(xml);
        assert_eq!(t.len(), 3);
        assert!(t.contains(&(17617, "frieren beyond journey s end".into())));
        assert_eq!(normalise("  Re:ZERO -Starting Life-  "), "re zero starting life");
    }
}
