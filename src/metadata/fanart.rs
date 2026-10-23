//! fanart.tv: clear logos, backdrops and posters, by TMDB/IMDb id (movies)
//! or TheTVDB id (series).

use serde_json::Value;

use super::{RemoteImage, get_json};
use crate::config::FanartConfig;
use crate::store::ImageKind;

pub struct Fanart {
    config: FanartConfig,
    http: reqwest::Client,
}

/// Images of `keys` (in order), best first: in `lang`, then English, then
/// without text (`00`), each by likes. Backdrops prefer images without text.
pub fn pick(v: &Value, keys: &[&str], lang: &str, textless_first: bool) -> Vec<String> {
    let mut all: Vec<(usize, i64, String)> = Vec::new();
    for (key_rank, key) in keys.iter().enumerate() {
        for img in v.get(*key).and_then(Value::as_array).into_iter().flatten() {
            let Some(url) = img.get("url").and_then(Value::as_str) else {
                continue;
            };
            let l = img.get("lang").and_then(Value::as_str).unwrap_or("");
            let lang_rank = match (l, textless_first) {
                ("" | "00", true) => 0,
                (l, true) if l == lang => 1,
                ("en", true) => 2,
                (l, false) if l == lang => 0,
                ("en", false) => 1,
                ("" | "00", false) => 2,
                _ => 3,
            };
            let likes = img
                .get("likes")
                .and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or_else(|| x.as_i64()))
                .unwrap_or(0);
            all.push((key_rank * 10 + lang_rank, -likes, url.to_owned()));
        }
    }
    all.sort();
    all.into_iter().map(|(_, _, url)| url).collect()
}

fn images(v: &Value, lang: &str, sets: &[(ImageKind, &[&str], bool)]) -> Vec<RemoteImage> {
    let mut out = Vec::new();
    for (kind, keys, textless) in sets {
        out.extend(
            pick(v, keys, lang, *textless)
                .into_iter()
                .take(2)
                .map(|url| RemoteImage { kind: *kind, url }),
        );
    }
    out
}

impl Fanart {
    pub fn new(config: FanartConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    async fn get(&self, path: &str) -> anyhow::Result<Option<Value>> {
        let mut q = vec![("api_key", self.config.api_key.clone())];
        if let Some(c) = &self.config.client_key {
            q.push(("client_key", c.clone()));
        }
        let req = self
            .http
            .get(format!("{}{path}", self.config.api_url.trim_end_matches('/')))
            .query(&q);
        get_json(req, "fanart.tv").await
    }

    fn lang(language: Option<&str>) -> &'static str {
        language.and_then(crate::lang::iso1).unwrap_or("en")
    }

    pub async fn movie(&self, id: &str, language: Option<&str>) -> anyhow::Result<Option<Vec<RemoteImage>>> {
        let Some(v) = self.get(&format!("/movies/{id}")).await? else {
            return Ok(None);
        };
        Ok(Some(images(
            &v,
            Self::lang(language),
            &[
                (ImageKind::Logo, &["hdmovielogo", "movielogo"], false),
                (ImageKind::Backdrop, &["moviebackground"], true),
                (ImageKind::Poster, &["movieposter"], false),
                (ImageKind::Thumb, &["moviethumb"], false),
            ],
        )))
    }

    pub async fn series(&self, tvdb_id: &str, language: Option<&str>) -> anyhow::Result<Option<Vec<RemoteImage>>> {
        let Some(v) = self.get(&format!("/tv/{tvdb_id}")).await? else {
            return Ok(None);
        };
        Ok(Some(images(
            &v,
            Self::lang(language),
            &[
                (ImageKind::Logo, &["hdtvlogo", "clearlogo"], false),
                (ImageKind::Backdrop, &["showbackground"], true),
                (ImageKind::Poster, &["tvposter"], false),
            ],
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_language_likes_and_textless_backdrops() {
        let v = json!({
            "hdmovielogo": [
                {"url": "logo-de", "lang": "de", "likes": "9"},
                {"url": "logo-fr", "lang": "fr", "likes": "1"},
                {"url": "logo-en", "lang": "en", "likes": "5"}
            ],
            "movielogo": [{"url": "old-fr", "lang": "fr", "likes": "50"}],
            "moviebackground": [
                {"url": "bg-en", "lang": "en", "likes": "10"},
                {"url": "bg-none", "lang": "00", "likes": "2"}
            ]
        });
        assert_eq!(
            pick(&v, &["hdmovielogo", "movielogo"], "fr", false),
            ["logo-fr", "logo-en", "logo-de", "old-fr"]
        );
        assert_eq!(pick(&v, &["moviebackground"], "fr", true), ["bg-none", "bg-en"]);
    }
}
