//! The Movie Database (TMDB) API v3: movies, series, seasons and artwork.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::Semaphore;

use super::{RemoteImage, RemoteItem, RemoteSeason};
use crate::config::TmdbConfig;
use crate::media::{Meta, Person, Rating, year_of};
use crate::store::ImageKind;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Concurrent requests; TMDB allows about 50 per second.
const CONCURRENCY: usize = 8;
const CAST_LIMIT: usize = 20;

#[derive(Clone)]
pub struct Tmdb {
    config: TmdbConfig,
    http: reqwest::Client,
    limit: Arc<Semaphore>,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn names(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| s(x, "name")).collect())
        .unwrap_or_default()
}

fn rating(v: &Value) -> Option<f64> {
    let r = v.get("vote_average").and_then(Value::as_f64)?;
    let votes = v.get("vote_count").and_then(Value::as_i64).unwrap_or(1);
    (r > 0.0 && votes > 0).then(|| (r * 10.0).round() / 10.0)
}

fn people(credits: Option<&Value>) -> Vec<Person> {
    let Some(c) = credits else { return Vec::new() };
    let mut out: Vec<Person> = Vec::new();
    if let Some(crew) = c.get("crew").and_then(Value::as_array) {
        for p in crew {
            let role = match p.get("job").and_then(Value::as_str) {
                Some("Director") => "director",
                Some("Screenplay" | "Writer") => "writer",
                Some("Creator") => "creator",
                _ => continue,
            };
            if let Some(name) = s(p, "name")
                && !out.iter().any(|x| x.name == name && x.role == role)
            {
                out.push(Person {
                    name,
                    role: role.into(),
                    character: None,
                });
            }
        }
    }
    if let Some(cast) = c.get("cast").and_then(Value::as_array) {
        for p in cast.iter().take(CAST_LIMIT) {
            if let Some(name) = s(p, "name") {
                out.push(Person {
                    name,
                    role: "actor".into(),
                    character: s(p, "character"),
                });
            }
        }
    }
    out
}

/// Country of a language tag: `en-US` → `US`.
fn country(language: &str) -> &str {
    language.split_once('-').map(|(_, c)| c).unwrap_or("US")
}

fn movie_certification(v: &Value, country: &str) -> Option<String> {
    v.get("release_dates")?
        .get("results")?
        .as_array()?
        .iter()
        .find(|r| r.get("iso_3166_1").and_then(Value::as_str) == Some(country))?
        .get("release_dates")?
        .as_array()?
        .iter()
        .find_map(|d| s(d, "certification"))
}

fn tv_certification(v: &Value, country: &str) -> Option<String> {
    v.get("content_ratings")?
        .get("results")?
        .as_array()?
        .iter()
        .find(|r| r.get("iso_3166_1").and_then(Value::as_str) == Some(country))
        .and_then(|r| s(r, "rating"))
}

/// The search result to trust: with a year, the first one released within a
/// year of it; without, the first one.
fn pick<'a>(results: &'a [Value], year: Option<i32>, date_key: &str) -> Option<&'a Value> {
    match year {
        None => results.first(),
        Some(y) => results
            .iter()
            .find(|r| year_of(r.get(date_key).and_then(Value::as_str)).is_some_and(|ry| (ry - y).abs() <= 1)),
    }
}

impl Tmdb {
    pub fn new(config: TmdbConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            limit: Arc::new(Semaphore::new(CONCURRENCY)),
        }
    }

    fn language<'a>(&'a self, language: Option<&'a str>) -> &'a str {
        language.unwrap_or(&self.config.language)
    }

    /// The configured default language.
    pub fn language_default(&self) -> &str {
        &self.config.language
    }

    /// Image candidates from TMDB paths: posters and stills at w780,
    /// backdrops at w1280 (stored images are at most 1200 px wide).
    fn images(&self, v: &Value, keys: &[(&str, ImageKind)]) -> Vec<RemoteImage> {
        let base = self.config.image_url.trim_end_matches('/');
        keys.iter()
            .filter_map(|(key, kind)| {
                let size = if *kind == ImageKind::Backdrop { "w1280" } else { "w780" };
                s(v, key).map(|path| RemoteImage {
                    kind: *kind,
                    url: format!("{base}/{size}{path}"),
                })
            })
            .collect()
    }

    /// GET a JSON document; `None` on 404.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Option<Value>> {
        let url = format!("{}{path}", self.config.api_url.trim_end_matches('/'));
        for attempt in 0..4 {
            let _permit = self.limit.acquire().await?;
            let mut req = self.http.get(&url).query(query).timeout(TIMEOUT);
            // A v4 read access token is a JWT; anything else is a v3 key.
            req = if self.config.api_key.starts_with("eyJ") {
                req.bearer_auth(&self.config.api_key)
            } else {
                req.query(&[("api_key", &self.config.api_key)])
            };
            let resp = req.send().await.with_context(|| format!("TMDB {path}"))?;
            match resp.status() {
                StatusCode::NOT_FOUND => return Ok(None),
                StatusCode::TOO_MANY_REQUESTS if attempt < 3 => {
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(2)
                        .min(10);
                    drop(_permit);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                st if st.is_success() => return Ok(Some(resp.json().await?)),
                StatusCode::UNAUTHORIZED => bail!("TMDB refused the API key (401)"),
                st => bail!("TMDB {path}: HTTP {st}"),
            }
        }
        bail!("TMDB {path}: rate limited")
    }

    /// Resolve an IMDb or TVDB id to a TMDB id.
    async fn find(&self, id: &str, source: &str, results_key: &str) -> anyhow::Result<Option<String>> {
        let Some(v) = self
            .get(&format!("/find/{id}"), &[("external_source", source.to_owned())])
            .await?
        else {
            return Ok(None);
        };
        Ok(v.get(results_key)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|r| r.get("id"))
            .and_then(Value::as_i64)
            .map(|i| i.to_string()))
    }

    pub async fn movie(&self, hint: &Meta, language: Option<&str>) -> anyhow::Result<Option<RemoteItem>> {
        let lang = self.language(language).to_owned();
        let mut id = hint.tmdb_id.clone();
        if id.is_none()
            && let Some(imdb) = &hint.imdb_id
        {
            id = self.find(imdb, "imdb_id", "movie_results").await?;
        }
        if id.is_none() {
            let mut q = vec![("query", hint.title.clone()), ("language", lang.clone())];
            if let Some(y) = hint.year {
                q.push(("year", y.to_string()));
            }
            let found = self.get("/search/movie", &q).await?;
            let mut results = found
                .as_ref()
                .and_then(|v| v.get("results"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if results.is_empty() && hint.year.is_some() {
                // The year in the name may be off (re-release, festival year).
                let q = [("query", hint.title.clone()), ("language", lang.clone())];
                results = self
                    .get("/search/movie", &q)
                    .await?
                    .and_then(|v| v.get("results").and_then(Value::as_array).cloned())
                    .unwrap_or_default();
            }
            id = pick(&results, hint.year, "release_date")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_i64)
                .map(|i| i.to_string());
        }
        let Some(id) = id else { return Ok(None) };
        let q = [
            ("language", lang.clone()),
            ("append_to_response", "credits,release_dates".to_owned()),
        ];
        let Some(v) = self.get(&format!("/movie/{id}"), &q).await? else {
            return Ok(None);
        };
        let release = s(&v, "release_date");
        Ok(Some(RemoteItem {
            meta: Meta {
                title: s(&v, "title").unwrap_or_else(|| hint.title.clone()),
                original_title: s(&v, "original_title"),
                original_language: s(&v, "original_language").and_then(|l| crate::lang::iso1(&l).map(str::to_owned)),
                sort_title: None,
                year: year_of(release.as_deref()),
                premiere_date: release,
                overview: s(&v, "overview"),
                tagline: s(&v, "tagline"),
                rating: rating(&v),
                ratings: rating(&v).map(|r| Rating::new("tmdb", r, 10.0)).into_iter().collect(),
                official_rating: movie_certification(&v, country(&lang)),
                genres: names(&v, "genres"),
                studios: names(&v, "production_companies"),
                people: people(v.get("credits")),
                tmdb_id: Some(id),
                imdb_id: s(&v, "imdb_id"),
                tvdb_id: None,
                anidb_id: None,
                anilist_id: None,
                provider_title: None,
                runtime_ms: v
                    .get("runtime")
                    .and_then(Value::as_i64)
                    .filter(|r| *r > 0)
                    .map(|r| r * 60_000),
            },
            images: self.images(
                &v,
                &[
                    ("poster_path", ImageKind::Poster),
                    ("backdrop_path", ImageKind::Backdrop),
                ],
            ),
        }))
    }

    pub async fn series(&self, hint: &Meta, language: Option<&str>) -> anyhow::Result<Option<RemoteItem>> {
        let lang = self.language(language).to_owned();
        let mut id = hint.tmdb_id.clone();
        if id.is_none()
            && let Some(tvdb) = &hint.tvdb_id
        {
            id = self.find(tvdb, "tvdb_id", "tv_results").await?;
        }
        if id.is_none()
            && let Some(imdb) = &hint.imdb_id
        {
            id = self.find(imdb, "imdb_id", "tv_results").await?;
        }
        if id.is_none() {
            let mut q = vec![("query", hint.title.clone()), ("language", lang.clone())];
            if let Some(y) = hint.year {
                q.push(("first_air_date_year", y.to_string()));
            }
            let results = self
                .get("/search/tv", &q)
                .await?
                .and_then(|v| v.get("results").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            id = pick(&results, hint.year, "first_air_date")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_i64)
                .map(|i| i.to_string());
        }
        let Some(id) = id else { return Ok(None) };
        let q = [
            ("language", lang.clone()),
            ("append_to_response", "credits,content_ratings,external_ids".to_owned()),
        ];
        let Some(v) = self.get(&format!("/tv/{id}"), &q).await? else {
            return Ok(None);
        };
        let first = s(&v, "first_air_date");
        let mut people = people(v.get("credits"));
        if let Some(creators) = v.get("created_by").and_then(Value::as_array) {
            for c in creators.iter().filter_map(|c| s(c, "name")) {
                people.insert(
                    0,
                    Person {
                        name: c,
                        role: "creator".into(),
                        character: None,
                    },
                );
            }
        }
        let external = v.get("external_ids");
        let mut studios = names(&v, "networks");
        studios.extend(names(&v, "production_companies"));
        studios.dedup();
        Ok(Some(RemoteItem {
            meta: Meta {
                title: s(&v, "name").unwrap_or_else(|| hint.title.clone()),
                original_title: s(&v, "original_name"),
                original_language: s(&v, "original_language").and_then(|l| crate::lang::iso1(&l).map(str::to_owned)),
                sort_title: None,
                year: year_of(first.as_deref()),
                premiere_date: first,
                overview: s(&v, "overview"),
                tagline: s(&v, "tagline"),
                rating: rating(&v),
                ratings: rating(&v).map(|r| Rating::new("tmdb", r, 10.0)).into_iter().collect(),
                official_rating: tv_certification(&v, country(&lang)),
                genres: names(&v, "genres"),
                studios,
                people,
                tmdb_id: Some(id),
                imdb_id: external.and_then(|e| s(e, "imdb_id")),
                tvdb_id: external
                    .and_then(|e| e.get("tvdb_id"))
                    .and_then(Value::as_i64)
                    .map(|i| i.to_string()),
                anidb_id: None,
                anilist_id: None,
                provider_title: None,
                runtime_ms: v
                    .get("episode_run_time")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_i64)
                    .map(|m| m * 60_000),
            },
            images: self.images(
                &v,
                &[
                    ("poster_path", ImageKind::Poster),
                    ("backdrop_path", ImageKind::Backdrop),
                ],
            ),
        }))
    }

    pub async fn season(
        &self,
        tv_id: &str,
        season: i32,
        language: Option<&str>,
    ) -> anyhow::Result<Option<RemoteSeason>> {
        let lang = self.language(language).to_owned();
        let Some(v) = self
            .get(&format!("/tv/{tv_id}/season/{season}"), &[("language", lang)])
            .await?
        else {
            return Ok(None);
        };
        let mut episodes = HashMap::new();
        for e in v.get("episodes").and_then(Value::as_array).into_iter().flatten() {
            let Some(n) = e.get("episode_number").and_then(Value::as_i64) else {
                continue;
            };
            let aired = s(e, "air_date");
            let mut people = people(Some(
                &serde_json::json!({ "crew": e.get("crew"), "cast": e.get("guest_stars") }),
            ));
            people.truncate(CAST_LIMIT);
            episodes.insert(
                n as i32,
                RemoteItem {
                    meta: Meta {
                        title: s(e, "name").unwrap_or_default(),
                        year: year_of(aired.as_deref()),
                        premiere_date: aired,
                        overview: s(e, "overview"),
                        rating: rating(e),
                        runtime_ms: e
                            .get("runtime")
                            .and_then(Value::as_i64)
                            .filter(|r| *r > 0)
                            .map(|r| r * 60_000),
                        tmdb_id: e.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
                        people,
                        ..Default::default()
                    },
                    images: self.images(e, &[("still_path", ImageKind::Thumb)]),
                },
            );
        }
        let aired = s(&v, "air_date");
        Ok(Some(RemoteSeason {
            meta: Meta {
                title: s(&v, "name").unwrap_or_default(),
                year: year_of(aired.as_deref()),
                premiere_date: aired,
                overview: s(&v, "overview"),
                tmdb_id: v.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
                ..Default::default()
            },
            images: self.images(&v, &[("poster_path", ImageKind::Poster)]),
            episodes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_by_year() {
        let r = [
            json!({"id": 1, "release_date": "2019-05-01"}),
            json!({"id": 2, "release_date": "1999-03-31"}),
        ];
        assert_eq!(pick(&r, Some(1999), "release_date").unwrap()["id"], 2);
        assert_eq!(pick(&r, Some(2020), "release_date").unwrap()["id"], 1);
        assert!(pick(&r, Some(1950), "release_date").is_none());
        assert_eq!(pick(&r, None, "release_date").unwrap()["id"], 1);
    }

    #[test]
    fn credits_and_certifications() {
        let v = json!({
            "credits": {
                "cast": [{"name": "Keanu Reeves", "character": "Neo"}],
                "crew": [{"name": "Lana Wachowski", "job": "Director"}, {"name": "X", "job": "Grip"}]
            },
            "release_dates": {"results": [
                {"iso_3166_1": "FR", "release_dates": [{"certification": ""}, {"certification": "12"}]},
                {"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}
            ]},
            "vote_average": 8.24, "vote_count": 100
        });
        let p = people(v.get("credits"));
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].role, "director");
        assert_eq!(p[1].character.as_deref(), Some("Neo"));
        assert_eq!(movie_certification(&v, country("fr-FR")).as_deref(), Some("12"));
        assert_eq!(movie_certification(&v, country("en-US")).as_deref(), Some("R"));
        assert_eq!(rating(&v), Some(8.2));
    }
}
