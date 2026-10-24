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

/// How a search was made and how to read its results.
struct Search<'a> {
    /// The name from the file (or `.nfo`).
    title: &'a str,
    year: Option<i32>,
    /// `release_date` or `first_air_date`.
    date_key: &'a str,
    /// `title`/`original_title` or `name`/`original_name`.
    title_keys: [&'a str; 2],
    /// TMDB already kept results of that year: a movie search's `year` keeps
    /// films with any release that year, whatever their main date (a 2020
    /// premiere re-released in 2025 is dated 2025).
    year_filtered: bool,
}

/// The search results worth trying, best first: exact title matches
/// (ignoring case, accents and punctuation), then results dated within a
/// year of the name's, then TMDB's order. Results dated otherwise are only
/// dropped when TMDB did not filter by year itself.
fn candidates<'a>(results: &'a [Value], s: &Search<'_>) -> Vec<&'a Value> {
    let dated = |r: &Value| {
        s.year
            .is_none_or(|y| year_of(r.get(s.date_key).and_then(Value::as_str)).is_some_and(|ry| (ry - y).abs() <= 1))
    };
    let wanted = crate::search::fold(s.title);
    let exact = |r: &&Value| {
        s.title_keys.iter().any(|k| {
            r.get(*k)
                .and_then(Value::as_str)
                .is_some_and(|t| crate::search::fold(t) == wanted)
        })
    };
    let mut kept: Vec<&Value> = results.iter().filter(|r| s.year_filtered || dated(r)).collect();
    // Stable: TMDB's order within each rank.
    kept.sort_by_key(|r| (!exact(r), !dated(r)));
    kept
}

/// Whether TMDB's runtime (minutes) fits a file's duration: within 30 % or
/// 20 minutes (extended cuts, credits, PAL speed-up). Unknown fits.
fn runtime_fits(runtime_min: Option<i64>, file_ms: Option<i64>) -> bool {
    let (Some(r), Some(f)) = (runtime_min.filter(|r| *r > 0), file_ms.filter(|f| *f > 0)) else {
        return true;
    };
    let diff = (r * 60_000 - f).abs();
    diff <= (r * 60_000 * 3 / 10).max(20 * 60_000)
}

/// Candidates whose runtime is checked against the file's.
const RUNTIME_CHECKS: usize = 3;

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

    /// A movie: by its ids, else searched by name. `hint.runtime_ms` is the
    /// file's duration, which discards results of another length.
    pub async fn movie(&self, hint: &Meta, language: Option<&str>) -> anyhow::Result<Option<RemoteItem>> {
        let lang = self.language(language).to_owned();
        let mut ids: Vec<String> = Vec::new();
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
            let mut year_filtered = hint.year.is_some();
            if results.is_empty() && hint.year.is_some() {
                // The year in the name may be off (re-release, festival year).
                let q = [("query", hint.title.clone()), ("language", lang.clone())];
                results = self
                    .get("/search/movie", &q)
                    .await?
                    .and_then(|v| v.get("results").and_then(Value::as_array).cloned())
                    .unwrap_or_default();
                year_filtered = false;
            }
            let search = Search {
                title: &hint.title,
                year: hint.year,
                date_key: "release_date",
                title_keys: ["title", "original_title"],
                year_filtered,
            };
            ids = candidates(&results, &search)
                .iter()
                .filter_map(|r| r.get("id").and_then(Value::as_i64))
                .map(|i| i.to_string())
                .collect();
        }
        let ids: Vec<String> = id.into_iter().chain(ids).collect();
        let q = [
            ("language", lang.clone()),
            ("append_to_response", "credits,release_dates".to_owned()),
        ];
        // The first candidate whose runtime fits the file's, else the first.
        let mut chosen = None;
        for id in ids.iter().take(RUNTIME_CHECKS) {
            let Some(v) = self.get(&format!("/movie/{id}"), &q).await? else {
                continue;
            };
            let fits = runtime_fits(v.get("runtime").and_then(Value::as_i64), hint.runtime_ms);
            if !fits {
                tracing::debug!(title = %hint.title, candidate = %id, "runtime differs from the file's; trying the next result");
            }
            if fits {
                chosen = Some(v);
                break;
            }
            chosen.get_or_insert(v);
        }
        let Some(v) = chosen else { return Ok(None) };
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
                tmdb_id: v.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
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
            let search = Search {
                title: &hint.title,
                year: hint.year,
                date_key: "first_air_date",
                title_keys: ["name", "original_name"],
                // `first_air_date_year` filters on the date the results carry.
                year_filtered: false,
            };
            id = candidates(&results, &search)
                .first()
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

    fn search<'a>(title: &'a str, year: Option<i32>, year_filtered: bool) -> Search<'a> {
        Search {
            title,
            year,
            date_key: "release_date",
            title_keys: ["title", "original_title"],
            year_filtered,
        }
    }

    fn ids(c: Vec<&Value>) -> Vec<i64> {
        c.iter().map(|r| r["id"].as_i64().unwrap()).collect()
    }

    #[test]
    fn candidates_by_year_without_tmdb_filter() {
        let r = [
            json!({"id": 1, "title": "A", "release_date": "2019-05-01"}),
            json!({"id": 2, "title": "B", "release_date": "1999-03-31"}),
        ];
        assert_eq!(ids(candidates(&r, &search("X", Some(1999), false))), [2]);
        assert_eq!(ids(candidates(&r, &search("X", Some(2020), false))), [1]);
        assert!(candidates(&r, &search("X", Some(1950), false)).is_empty());
        assert_eq!(ids(candidates(&r, &search("X", None, false))), [1, 2]);
    }

    #[test]
    fn hamilton_is_hamilton() {
        // TMDB's answer to "Hamilton", year 2020 (October 2026): the filmed
        // musical premiered in 2020 but is dated by its 2025 re-release.
        let r = [
            json!({"id": 556574, "title": "Hamilton", "original_title": "Hamilton", "release_date": "2025-09-05"}),
            json!({"id": 510026, "title": "Bethany Hamilton: Unstoppable", "original_title": "Bethany Hamilton: Unstoppable", "release_date": "2019-07-12"}),
            json!({"id": 723212, "title": "Hamilton: History Has Its Eyes On You", "release_date": "2020-07-10"}),
        ];
        assert_eq!(ids(candidates(&r, &search("Hamilton", Some(2020), true)))[0], 556574);
        // An exact title comes first whatever TMDB's order.
        let reordered = [r[1].clone(), r[2].clone(), r[0].clone()];
        assert_eq!(
            ids(candidates(&reordered, &search("hamilton", Some(2020), true))),
            [556574, 510026, 723212]
        );
        // By the original title too, accents and punctuation aside.
        let amelie = [
            json!({"id": 1, "title": "Amelie From Montmartre", "original_title": "x"}),
            json!({"id": 194, "title": "Amélie", "original_title": "Le Fabuleux Destin d'Amélie Poulain"}),
        ];
        assert_eq!(
            ids(candidates(
                &amelie,
                &search("Le fabuleux destin d amelie poulain", None, false)
            ))[0],
            194
        );
    }

    #[test]
    fn runtimes_fit_within_reason() {
        let minutes = |m: i64| Some(m * 60_000);
        // Hamilton's file (2 h 40) against Hamilton and Bethany Hamilton.
        assert!(runtime_fits(Some(160), Some(9_615_000)));
        assert!(!runtime_fits(Some(99), Some(9_615_000)));
        // An extended cut, a short film's credits, PAL speed-up.
        assert!(runtime_fits(Some(178), minutes(228)));
        assert!(runtime_fits(Some(8), minutes(25)));
        assert!(runtime_fits(Some(100), minutes(96)));
        assert!(!runtime_fits(Some(30), minutes(120)));
        // Unknown on either side fits.
        assert!(runtime_fits(None, minutes(100)) && runtime_fits(Some(0), minutes(100)));
        assert!(runtime_fits(Some(100), None));
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
