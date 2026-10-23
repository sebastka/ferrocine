//! OpenSubtitles.com REST API: search by file hash and ids, download as SRT.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::OpenSubtitlesConfig;
use crate::metadata::ProviderState;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Login tokens are valid for a day.
const TOKEN_LIFETIME: Duration = Duration::from_secs(20 * 3600);
const MAX_SUBTITLE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Default)]
pub struct State {
    token: Mutex<Option<(String, String, Instant)>>,
    /// After a quota error, no download is attempted before this.
    exhausted_until: Mutex<Option<Instant>>,
}

/// A subtitle offered by the search.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Candidate {
    /// The file to download.
    pub file_id: i64,
    /// ISO 639-1 when known, else the provider's code.
    pub language: String,
    pub release: Option<String>,
    pub file_name: Option<String>,
    pub hearing_impaired: bool,
    /// Only the parts in a foreign language ("forced").
    pub foreign_parts_only: bool,
    pub ai_translated: bool,
    pub machine_translated: bool,
    pub from_trusted: bool,
    pub downloads: u64,
    pub rating: Option<f64>,
    /// Found by the hash of this very file.
    pub hash_match: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchQuery {
    pub moviehash: Option<String>,
    pub imdb_id: Option<String>,
    pub tmdb_id: Option<String>,
    pub parent_imdb_id: Option<String>,
    pub parent_tmdb_id: Option<String>,
    pub season_number: Option<i32>,
    pub episode_number: Option<i32>,
    pub query: Option<String>,
    pub year: Option<i32>,
    /// ISO 639-1.
    pub languages: Vec<String>,
}

pub struct Downloaded {
    pub vtt: String,
    /// Downloads left today, as the provider counts them.
    pub remaining: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
#[error("the OpenSubtitles download quota is used up{}", .reset.as_deref().map(|r| format!(" until {r}")).unwrap_or_default())]
pub struct QuotaExceeded {
    pub reset: Option<String>,
}

/// `tt0133093` → `133093` (the API wants the number without leading zeros).
fn imdb_number(id: &str) -> Option<String> {
    let digits = id.trim().trim_start_matches("tt").trim_start_matches('0');
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.to_owned())
}

/// Query parameters, sorted by name as the API asks (it redirects otherwise).
pub fn params(q: &SearchQuery) -> Vec<(&'static str, String)> {
    let mut p: Vec<(&'static str, String)> = Vec::new();
    if let Some(e) = q.episode_number {
        p.push(("episode_number", e.to_string()));
    }
    if let Some(i) = q.imdb_id.as_deref().and_then(imdb_number) {
        p.push(("imdb_id", i));
    }
    if !q.languages.is_empty() {
        let mut langs: Vec<String> = q.languages.iter().map(|l| l.to_lowercase()).collect();
        langs.sort();
        langs.dedup();
        p.push(("languages", langs.join(",")));
    }
    if let Some(h) = &q.moviehash {
        p.push(("moviehash", h.clone()));
    }
    if let Some(i) = q.parent_imdb_id.as_deref().and_then(imdb_number) {
        p.push(("parent_imdb_id", i));
    }
    if let Some(t) = &q.parent_tmdb_id {
        p.push(("parent_tmdb_id", t.clone()));
    }
    if let Some(text) = &q.query {
        p.push(("query", text.to_lowercase()));
    }
    if let Some(s) = q.season_number {
        p.push(("season_number", s.to_string()));
    }
    if let Some(t) = &q.tmdb_id {
        p.push(("tmdb_id", t.clone()));
    }
    if let Some(y) = q.year {
        p.push(("year", y.to_string()));
    }
    p.sort_by_key(|(k, _)| *k);
    p
}

/// Parse a search answer: one candidate per file.
pub fn parse_search(v: &Value) -> Vec<Candidate> {
    let mut out = Vec::new();
    for d in v.get("data").and_then(Value::as_array).into_iter().flatten() {
        let a = &d["attributes"];
        let b = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
        let lang = a.get("language").and_then(Value::as_str).unwrap_or_default();
        for f in a.get("files").and_then(Value::as_array).into_iter().flatten() {
            let Some(file_id) = f.get("file_id").and_then(Value::as_i64) else {
                continue;
            };
            out.push(Candidate {
                file_id,
                language: crate::lang::iso1(lang).map_or_else(|| lang.to_owned(), str::to_owned),
                release: a.get("release").and_then(Value::as_str).map(str::to_owned),
                file_name: f.get("file_name").and_then(Value::as_str).map(str::to_owned),
                hearing_impaired: b("hearing_impaired"),
                foreign_parts_only: b("foreign_parts_only"),
                ai_translated: b("ai_translated"),
                machine_translated: b("machine_translated"),
                from_trusted: b("from_trusted"),
                downloads: a.get("download_count").and_then(Value::as_u64).unwrap_or(0),
                rating: a.get("ratings").and_then(Value::as_f64).filter(|r| *r > 0.0),
                hash_match: b("moviehash_match"),
            });
        }
    }
    out
}

pub struct OpenSubtitles {
    config: OpenSubtitlesConfig,
    http: reqwest::Client,
    state: Arc<ProviderState>,
}

impl OpenSubtitles {
    pub fn new(config: OpenSubtitlesConfig, http: reqwest::Client, state: Arc<ProviderState>) -> Self {
        Self { config, http, state }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.api_url.trim_end_matches('/'))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header("Api-Key", &self.config.api_key)
            // The API wants "<application> v<version>".
            .header(reqwest::header::USER_AGENT, format!("Ferrocine v{}", crate::VERSION))
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(TIMEOUT)
    }

    /// The account's login token: downloads need one.
    async fn token(&self) -> anyhow::Result<String> {
        let (user, password) = (&self.config.username, &self.config.password);
        let mut t = self.state.opensubtitles.token.lock().await;
        if let Some((u, token, at)) = t.as_ref()
            && u == user
            && at.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token.clone());
        }
        let resp = self
            .request(reqwest::Method::POST, "/login")
            .json(&json!({ "username": user, "password": password }))
            .send()
            .await
            .context("OpenSubtitles login")?;
        if !resp.status().is_success() {
            bail!("OpenSubtitles refused the login: HTTP {}", resp.status());
        }
        let v: Value = resp.json().await?;
        let token = v
            .get("token")
            .and_then(Value::as_str)
            .context("OpenSubtitles login answered no token")?
            .to_owned();
        *t = Some((user.clone(), token.clone(), Instant::now()));
        Ok(token)
    }

    pub async fn search(&self, q: &SearchQuery) -> anyhow::Result<Vec<Candidate>> {
        let resp = self
            .request(reqwest::Method::GET, "/subtitles")
            .query(&params(q))
            .send()
            .await
            .context("OpenSubtitles search")?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("OpenSubtitles refused the API key: HTTP {status}");
        }
        if !status.is_success() {
            bail!("OpenSubtitles search: HTTP {status}");
        }
        Ok(parse_search(
            &resp.json().await.context("OpenSubtitles search: invalid JSON")?,
        ))
    }

    /// Download a file as SRT and convert it to WebVTT.
    pub async fn download(&self, file_id: i64) -> anyhow::Result<Downloaded> {
        {
            let mut until = self.state.opensubtitles.exhausted_until.lock().await;
            match *until {
                Some(t) if Instant::now() < t => return Err(QuotaExceeded { reset: None }.into()),
                Some(_) => *until = None,
                None => {}
            }
        }
        let mut retried = false;
        let resp = loop {
            let resp = self
                .request(reqwest::Method::POST, "/download")
                .bearer_auth(self.token().await?)
                .json(&json!({ "file_id": file_id, "sub_format": "srt" }))
                .send()
                .await
                .context("OpenSubtitles download")?;
            // A token that expired early: sign in again, once.
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                *self.state.opensubtitles.token.lock().await = None;
                continue;
            }
            break resp;
        };
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::NOT_ACCEPTABLE || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let reset = v.get("reset_time_utc").and_then(Value::as_str).map(str::to_owned);
            // Wait an hour (or until the announced reset) before trying again.
            let wait = reset
                .as_deref()
                .and_then(|r| r.parse::<jiff::Timestamp>().ok())
                .map(|t| (t.as_second() - jiff::Timestamp::now().as_second()).clamp(60, 86_400) as u64)
                .unwrap_or(3600);
            *self.state.opensubtitles.exhausted_until.lock().await = Some(Instant::now() + Duration::from_secs(wait));
            return Err(QuotaExceeded { reset }.into());
        }
        if !status.is_success() {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or_default();
            bail!("OpenSubtitles download: HTTP {status} {msg}");
        }
        let link = v
            .get("link")
            .and_then(Value::as_str)
            .context("OpenSubtitles download answered no link")?;
        let file = self.http.get(link).timeout(TIMEOUT).send().await?;
        if !file.status().is_success() {
            bail!("OpenSubtitles file: HTTP {}", file.status());
        }
        let bytes = file.bytes().await?;
        if bytes.len() > MAX_SUBTITLE_BYTES {
            bail!("OpenSubtitles file too large");
        }
        Ok(Downloaded {
            vtt: super::srt_to_vtt(&String::from_utf8_lossy(&bytes)),
            remaining: v.get("remaining").and_then(Value::as_i64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_parameters() {
        let q = SearchQuery {
            moviehash: Some("8e245d9679d31e12".into()),
            parent_imdb_id: Some("tt0903747".into()),
            season_number: Some(1),
            episode_number: Some(2),
            languages: vec!["fr".into(), "EN".into()],
            ..Default::default()
        };
        let p: Vec<String> = params(&q).into_iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(
            p,
            [
                "episode_number=2",
                "languages=en,fr",
                "moviehash=8e245d9679d31e12",
                "parent_imdb_id=903747",
                "season_number=1"
            ]
        );
    }

    #[test]
    fn parses_search_results() {
        let v = json!({"data": [{"attributes": {
            "language": "en", "download_count": 1200, "hearing_impaired": true, "moviehash_match": true,
            "release": "Sintel.2010.1080p", "from_trusted": true, "ratings": 8.5,
            "files": [{"file_id": 42, "file_name": "sintel.en.srt"}]}}]});
        let c = parse_search(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0].file_id, c[0].downloads, c[0].hash_match, c[0].hearing_impaired),
            (42, 1200, true, true)
        );
        assert_eq!(c[0].language, "en");
    }
}
