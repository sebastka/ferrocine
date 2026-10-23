//! OMDb: IMDb, Rotten Tomatoes and Metacritic ratings, by IMDb id.

use serde_json::Value;

use super::{RemoteItem, get_json};
use crate::config::OmdbConfig;
use crate::media::{Meta, Rating};

pub struct Omdb {
    config: OmdbConfig,
    http: reqwest::Client,
}

/// `"8.7/10"`, `"83%"`, `"73/100"` → (value, max).
fn parse_value(s: &str) -> Option<(f64, f64)> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        return Some((p.trim().parse().ok()?, 100.0));
    }
    let (v, max) = s.split_once('/')?;
    Some((v.trim().parse().ok()?, max.trim().parse().ok()?))
}

fn known(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "N/A")
        .map(str::to_owned)
}

pub fn parse(v: &Value) -> Option<RemoteItem> {
    if v.get("Response").and_then(Value::as_str) != Some("True") {
        return None;
    }
    let mut ratings = Vec::new();
    for r in v.get("Ratings").and_then(Value::as_array).into_iter().flatten() {
        let source = match r.get("Source").and_then(Value::as_str) {
            Some("Internet Movie Database") => "imdb",
            Some("Rotten Tomatoes") => "rotten_tomatoes",
            Some("Metacritic") => "metacritic",
            _ => continue,
        };
        if let Some((value, max)) = r.get("Value").and_then(Value::as_str).and_then(parse_value) {
            ratings.push(Rating::new(source, value, max));
        }
    }
    if !ratings.iter().any(|r| r.source == "imdb")
        && let Some(r) = known(v, "imdbRating").and_then(|r| r.parse::<f64>().ok())
    {
        ratings.push(Rating::new("imdb", r, 10.0));
    }
    Some(RemoteItem {
        meta: Meta {
            official_rating: known(v, "Rated"),
            imdb_id: known(v, "imdbID"),
            ratings,
            ..Default::default()
        },
        images: Vec::new(),
    })
}

impl Omdb {
    pub fn new(config: OmdbConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    pub async fn lookup(&self, imdb_id: &str) -> anyhow::Result<Option<RemoteItem>> {
        let req = self
            .http
            .get(format!("{}/", self.config.api_url.trim_end_matches('/')))
            .query(&[("i", imdb_id), ("apikey", &self.config.api_key)]);
        Ok(get_json(req, "OMDb").await?.as_ref().and_then(parse))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_ratings() {
        let v = json!({"Response": "True", "Rated": "R", "imdbID": "tt0133093", "imdbRating": "8.7",
            "Ratings": [{"Source": "Internet Movie Database", "Value": "8.7/10"},
                        {"Source": "Rotten Tomatoes", "Value": "83%"},
                        {"Source": "Metacritic", "Value": "73/100"}]});
        let r = parse(&v).unwrap();
        assert_eq!(r.meta.official_rating.as_deref(), Some("R"));
        let got: Vec<(String, f64, f64)> = r
            .meta
            .ratings
            .iter()
            .map(|x| (x.source.clone(), x.value, x.max))
            .collect();
        assert_eq!(
            got,
            [
                ("imdb".into(), 8.7, 10.0),
                ("rotten_tomatoes".into(), 83.0, 100.0),
                ("metacritic".into(), 73.0, 100.0)
            ]
        );
        assert!(parse(&json!({"Response": "False", "Error": "Movie not found!"})).is_none());
        let only = parse(&json!({"Response": "True", "Rated": "N/A", "imdbRating": "7.1"})).unwrap();
        assert_eq!((only.meta.official_rating, only.meta.ratings.len()), (None, 1));
    }
}
