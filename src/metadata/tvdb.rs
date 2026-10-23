//! TheTVDB API v4: series, their episodes in a chosen order (broadcast, DVD,
//! absolute), translations and artwork.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{ProviderState, RemoteImage, RemoteItem, RemoteSeason};
use crate::config::{EpisodeOrder, TvdbConfig};
use crate::media::{Meta, year_of};
use crate::store::ImageKind;

/// Tokens last a month; renewed well before.
const TOKEN_LIFETIME: Duration = Duration::from_secs(7 * 86_400);
/// Artwork types of TheTVDB.
const ART_POSTER: i64 = 2;
const ART_BACKGROUND: i64 = 3;
const ART_CLEARLOGO: i64 = 23;

#[derive(Default)]
pub struct State {
    token: Mutex<Option<(String, String, Instant)>>,
}

pub struct Tvdb {
    config: TvdbConfig,
    http: reqwest::Client,
    state: Arc<ProviderState>,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn id_string(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn season_type(order: EpisodeOrder) -> &'static str {
    match order {
        EpisodeOrder::Aired => "default",
        EpisodeOrder::Dvd => "dvd",
        EpisodeOrder::Absolute => "absolute",
    }
}

/// A translation of `field` (`name` or `overview`) in `lang` from
/// `translations.<field>Translations`.
fn translation(v: &Value, field: &str, lang: &str) -> Option<String> {
    v.get("translations")?
        .get(format!("{field}Translations"))?
        .as_array()?
        .iter()
        .find(|t| t.get("language").and_then(Value::as_str) == Some(lang))
        .and_then(|t| s(t, field))
}

/// Artwork of one type, best score first, in `lang`, then English, then without language.
fn artwork(v: &Value, kind: i64, lang: &str) -> Vec<String> {
    let Some(list) = v.get("artworks").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut arts: Vec<&Value> = list
        .iter()
        .filter(|a| a.get("type").and_then(Value::as_i64) == Some(kind))
        .collect();
    let rank = |a: &Value| match a.get("language").and_then(Value::as_str) {
        Some(l) if l == lang => 0,
        Some("eng") => 1,
        None | Some("") => 2,
        _ => 3,
    };
    arts.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            let score = |x: &Value| x.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            score(b).total_cmp(&score(a))
        })
    });
    arts.into_iter().filter_map(|a| s(a, "image")).collect()
}

impl Tvdb {
    pub fn new(config: TvdbConfig, http: reqwest::Client, state: Arc<ProviderState>) -> Self {
        Self { config, http, state }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.api_url.trim_end_matches('/'))
    }

    async fn token(&self, renew: bool) -> anyhow::Result<String> {
        let mut t = self.state.tvdb.token.lock().await;
        if let Some((key, token, at)) = t.as_ref()
            && !renew
            && *key == self.config.api_key
            && at.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token.clone());
        }
        let mut body = json!({ "apikey": self.config.api_key });
        if let Some(pin) = &self.config.pin {
            body["pin"] = json!(pin);
        }
        let resp = self
            .http
            .post(self.url("/login"))
            .json(&body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("TheTVDB login")?;
        if !resp.status().is_success() {
            bail!("TheTVDB refused the API key: HTTP {}", resp.status());
        }
        let v: Value = resp.json().await?;
        let token = v
            .pointer("/data/token")
            .and_then(Value::as_str)
            .context("TheTVDB login answered no token")?
            .to_owned();
        *t = Some((self.config.api_key.clone(), token.clone(), Instant::now()));
        Ok(token)
    }

    /// GET `data` of a v4 endpoint; `None` on 404. Renews the token once on 401.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Option<Value>> {
        for renew in [false, true] {
            let token = self.token(renew).await?;
            let resp = self
                .http
                .get(self.url(path))
                .query(query)
                .bearer_auth(&token)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .with_context(|| format!("TheTVDB {path}"))?;
            match resp.status() {
                reqwest::StatusCode::UNAUTHORIZED if !renew => continue,
                reqwest::StatusCode::NOT_FOUND => return Ok(None),
                st if st.is_success() => {
                    let mut v: Value = resp
                        .json()
                        .await
                        .with_context(|| format!("TheTVDB {path}: invalid JSON"))?;
                    return Ok(Some(v["data"].take()));
                }
                st => bail!("TheTVDB {path}: HTTP {st}"),
            }
        }
        bail!("TheTVDB refused the token")
    }

    async fn find(&self, hint: &Meta) -> anyhow::Result<Option<String>> {
        if let Some(id) = &hint.tvdb_id {
            return Ok(Some(id.clone()));
        }
        if let Some(imdb) = &hint.imdb_id
            && let Some(found) = self.get(&format!("/search/remoteid/{imdb}"), &[]).await?
            && let Some(id) = found
                .as_array()
                .and_then(|a| a.iter().find_map(|r| r.get("series").and_then(|s| id_string(s, "id"))))
        {
            return Ok(Some(id));
        }
        let mut q = vec![("query", hint.title.clone()), ("type", "series".to_owned())];
        if let Some(y) = hint.year {
            q.push(("year", y.to_string()));
        }
        let results = self
            .get("/search", &q)
            .await?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let pick = match hint.year {
            None => results.first(),
            Some(y) => results.iter().find(|r| {
                s(r, "year")
                    .and_then(|ry| ry.parse::<i32>().ok())
                    .is_some_and(|ry| (ry - y).abs() <= 1)
            }),
        };
        Ok(pick.and_then(|r| id_string(r, "tvdb_id")))
    }

    pub async fn series(&self, hint: &Meta, language: Option<&str>) -> anyhow::Result<Option<RemoteItem>> {
        let Some(id) = self.find(hint).await? else {
            return Ok(None);
        };
        let Some(v) = self
            .get(
                &format!("/series/{id}/extended"),
                &[("meta", "translations".to_owned())],
            )
            .await?
        else {
            return Ok(None);
        };
        let lang = language.and_then(crate::lang::iso2).unwrap_or("eng");
        let first = s(&v, "firstAired");
        let remote = |source: &str| {
            v.get("remoteIds")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|r| r.get("sourceName").and_then(Value::as_str) == Some(source))
                })
                .and_then(|r| id_string(r, "id"))
        };
        let mut images = Vec::new();
        for (kind, art) in [
            (ImageKind::Poster, ART_POSTER),
            (ImageKind::Backdrop, ART_BACKGROUND),
            (ImageKind::Logo, ART_CLEARLOGO),
        ] {
            images.extend(
                artwork(&v, art, lang)
                    .into_iter()
                    .take(2)
                    .map(|url| RemoteImage { kind, url }),
            );
        }
        Ok(Some(RemoteItem {
            meta: Meta {
                title: translation(&v, "name", lang)
                    .or_else(|| s(&v, "name"))
                    .unwrap_or_else(|| hint.title.clone()),
                // TheTVDB's own name is in the original language.
                original_title: s(&v, "name"),
                original_language: s(&v, "originalLanguage").and_then(|l| crate::lang::iso1(&l).map(str::to_owned)),
                year: year_of(first.as_deref()),
                premiere_date: first,
                overview: translation(&v, "overview", lang).or_else(|| s(&v, "overview")),
                genres: v
                    .get("genres")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|g| s(g, "name")).collect())
                    .unwrap_or_default(),
                studios: v
                    .get("originalNetwork")
                    .and_then(|n| s(n, "name"))
                    .into_iter()
                    .collect(),
                tvdb_id: Some(id),
                imdb_id: remote("IMDB"),
                tmdb_id: remote("TheMovieDB.com"),
                runtime_ms: v
                    .get("averageRuntime")
                    .and_then(Value::as_i64)
                    .filter(|r| *r > 0)
                    .map(|r| r * 60_000),
                ..Default::default()
            },
            images,
        }))
    }

    /// Episodes of season `number` in `order`. With absolute order, every
    /// episode is in season 1, numbered from the first episode.
    pub async fn season(
        &self,
        series: &str,
        number: i32,
        order: EpisodeOrder,
        language: Option<&str>,
    ) -> anyhow::Result<Option<RemoteSeason>> {
        let lang = language.and_then(crate::lang::iso2).unwrap_or("eng");
        let path = format!("/series/{series}/episodes/{}/{lang}", season_type(order));
        let mut episodes = HashMap::new();
        for page in 0..50 {
            let Some(v) = self.get(&path, &[("page", page.to_string())]).await? else {
                break;
            };
            let list = v.get("episodes").and_then(Value::as_array).cloned().unwrap_or_default();
            if list.is_empty() {
                break;
            }
            for e in &list {
                let season = e.get("seasonNumber").and_then(Value::as_i64).unwrap_or(1) as i32;
                let in_season = if order == EpisodeOrder::Absolute {
                    number == 1
                } else {
                    season == number
                };
                let Some(n) = e.get("number").and_then(Value::as_i64) else {
                    continue;
                };
                if !in_season {
                    continue;
                }
                let aired = s(e, "aired");
                episodes.insert(
                    n as i32,
                    RemoteItem {
                        meta: Meta {
                            title: s(e, "name").unwrap_or_default(),
                            year: year_of(aired.as_deref()),
                            premiere_date: aired,
                            overview: s(e, "overview"),
                            runtime_ms: e
                                .get("runtime")
                                .and_then(Value::as_i64)
                                .filter(|r| *r > 0)
                                .map(|r| r * 60_000),
                            tvdb_id: id_string(e, "id"),
                            ..Default::default()
                        },
                        images: s(e, "image")
                            .map(|url| RemoteImage {
                                kind: ImageKind::Thumb,
                                url,
                            })
                            .into_iter()
                            .collect(),
                    },
                );
            }
            if list.len() < 500 {
                break;
            }
        }
        if episodes.is_empty() {
            return Ok(None);
        }
        Ok(Some(RemoteSeason {
            episodes,
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_prefers_language_then_score() {
        let v = json!({"artworks": [
            {"type": 2, "image": "fr-low", "language": "fra", "score": 1},
            {"type": 2, "image": "en-high", "language": "eng", "score": 100},
            {"type": 2, "image": "fr-high", "language": "fra", "score": 50},
            {"type": 3, "image": "bg", "language": null, "score": 5}
        ]});
        assert_eq!(artwork(&v, ART_POSTER, "fra"), ["fr-high", "fr-low", "en-high"]);
        assert_eq!(artwork(&v, ART_BACKGROUND, "fra"), ["bg"]);
        let t = json!({"translations": {"nameTranslations": [{"language": "fra", "name": "Le Nom"}]}});
        assert_eq!(translation(&t, "name", "fra").as_deref(), Some("Le Nom"));
        assert_eq!(season_type(EpisodeOrder::Dvd), "dvd");
    }
}
