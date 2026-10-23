//! AniList (GraphQL, no key): anime series and films.

use serde_json::{Value, json};

use super::{RemoteImage, RemoteItem, get_json, plain_text};
use crate::config::{AnilistConfig, TitleLanguage};
use crate::media::{Meta, Person, Rating};
use crate::store::ImageKind;

const FIELDS: &str = "id idMal title { romaji english native } description(asHtml: false) \
     startDate { year month day } episodes duration genres averageScore format countryOfOrigin \
     coverImage { extraLarge } bannerImage studios(isMain: true) { nodes { name } } \
     staff(perPage: 4, sort: RELEVANCE) { edges { role node { name { full } } } }";

pub struct Anilist {
    config: AnilistConfig,
    http: reqwest::Client,
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn date(v: &Value) -> Option<String> {
    let y = v.pointer("/startDate/year")?.as_i64()?;
    let m = v.pointer("/startDate/month").and_then(Value::as_i64).unwrap_or(1);
    let d = v.pointer("/startDate/day").and_then(Value::as_i64).unwrap_or(1);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// The media to trust among search results: with a year, the first one that
/// started within a year of it; without, the first one.
fn choose(results: &[Value], year: Option<i32>) -> Option<&Value> {
    match year {
        None => results.first(),
        Some(y) => results.iter().find(|m| {
            m.pointer("/startDate/year")
                .and_then(Value::as_i64)
                .is_some_and(|my| (my as i32 - y).abs() <= 1)
        }),
    }
}

pub fn to_remote(m: &Value, title: TitleLanguage) -> RemoteItem {
    let english = s(m, "/title/english");
    let romaji = s(m, "/title/romaji");
    let native = s(m, "/title/native");
    let shown = match title {
        TitleLanguage::English => english.clone().or(romaji.clone()),
        TitleLanguage::Romaji => romaji.clone().or(english.clone()),
        TitleLanguage::Native => native.clone().or(romaji.clone()),
    };
    let premiere = date(m);
    let mut people: Vec<Person> = Vec::new();
    for e in m
        .pointer("/staff/edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(name) = s(e, "/node/name/full") {
            people.push(Person {
                name,
                role: s(e, "/role").unwrap_or_default().to_lowercase(),
                character: None,
            });
        }
    }
    let score = m.get("averageScore").and_then(Value::as_f64);
    let mut images = Vec::new();
    if let Some(url) = s(m, "/coverImage/extraLarge") {
        images.push(RemoteImage {
            kind: ImageKind::Poster,
            url,
        });
    }
    if let Some(url) = s(m, "/bannerImage") {
        images.push(RemoteImage {
            kind: ImageKind::Backdrop,
            url,
        });
    }
    RemoteItem {
        meta: Meta {
            title: shown.unwrap_or_default(),
            original_language: native.as_ref().and(match s(m, "/countryOfOrigin").as_deref() {
                Some("JP") => Some("ja".to_owned()),
                Some("KR") => Some("ko".to_owned()),
                Some("CN" | "TW") => Some("zh".to_owned()),
                _ => None,
            }),
            original_title: native.or(romaji),
            year: crate::media::year_of(premiere.as_deref()),
            premiere_date: premiere,
            overview: s(m, "/description").map(|d| plain_text(&d)),
            rating: score.map(|v| v / 10.0),
            ratings: score.map(|v| Rating::new("anilist", v, 100.0)).into_iter().collect(),
            genres: m
                .get("genres")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            studios: m
                .pointer("/studios/nodes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|n| s(n, "/name")).collect())
                .unwrap_or_default(),
            people,
            anilist_id: m.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
            runtime_ms: m
                .get("duration")
                .and_then(Value::as_i64)
                .filter(|d| *d > 0)
                .map(|d| d * 60_000),
            ..Default::default()
        },
        images,
    }
}

impl Anilist {
    pub fn new(config: AnilistConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    async fn query(&self, query: &str, variables: Value) -> anyhow::Result<Option<Value>> {
        let req = self
            .http
            .post(&self.config.api_url)
            .json(&json!({ "query": query, "variables": variables }));
        Ok(get_json(req, "AniList").await?.map(|mut v| v["data"].take()))
    }

    /// Find a film (`movie`) or a series by AniList id, else by title and year.
    pub async fn search(&self, hint: &Meta, movie: bool) -> anyhow::Result<Option<RemoteItem>> {
        let media = if let Some(id) = hint.anilist_id.as_ref().and_then(|i| i.parse::<i64>().ok()) {
            let q = format!("query ($id: Int) {{ Media(id: $id, type: ANIME) {{ {FIELDS} }} }}");
            self.query(&q, json!({ "id": id })).await?.and_then(|mut v| {
                let m = v["Media"].take();
                (!m.is_null()).then_some(m)
            })
        } else {
            let formats = if movie {
                json!(["MOVIE"])
            } else {
                json!(["TV", "TV_SHORT", "ONA", "OVA"])
            };
            let q = format!(
                "query ($search: String, $formats: [MediaFormat]) {{ Page(perPage: 8) {{ \
                 media(search: $search, type: ANIME, format_in: $formats) {{ {FIELDS} }} }} }}"
            );
            let page = self
                .query(&q, json!({ "search": hint.title, "formats": formats }))
                .await?;
            let results = page
                .and_then(|v| v.pointer("/Page/media").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            choose(&results, hint.year).cloned()
        };
        Ok(media.map(|m| to_remote(&m, self.config.title)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frieren() -> Value {
        json!({
            "id": 154587, "idMal": 52991,
            "title": {"romaji": "Sousou no Frieren", "english": "Frieren: Beyond Journey's End", "native": "葬送のフリーレン"},
            "description": "An elf mage <i>outlives</i> her party.<br><br>(Source: Crunchyroll)",
            "startDate": {"year": 2023, "month": 9, "day": 29},
            "episodes": 28, "duration": 24, "genres": ["Adventure", "Drama", "Fantasy"], "averageScore": 91,
            "coverImage": {"extraLarge": "https://img.anili.st/cover.jpg"}, "bannerImage": "https://img.anili.st/banner.jpg",
            "studios": {"nodes": [{"name": "Madhouse"}]},
            "staff": {"edges": [{"role": "Director", "node": {"name": {"full": "Keiichirou Saitou"}}}]}
        })
    }

    #[test]
    fn maps_media() {
        let r = to_remote(&frieren(), TitleLanguage::English);
        assert_eq!(r.meta.title, "Frieren: Beyond Journey's End");
        assert_eq!(r.meta.original_title.as_deref(), Some("葬送のフリーレン"));
        assert_eq!(r.meta.premiere_date.as_deref(), Some("2023-09-29"));
        assert_eq!(
            r.meta.overview.as_deref(),
            Some("An elf mage outlives her party.\n\n(Source: Crunchyroll)")
        );
        assert_eq!((r.meta.rating, r.meta.ratings[0].max), (Some(9.1), 100.0));
        assert_eq!(r.meta.anilist_id.as_deref(), Some("154587"));
        assert_eq!(r.meta.people[0].role, "director");
        assert_eq!(r.images.len(), 2);
        assert_eq!(
            to_remote(&frieren(), TitleLanguage::Romaji).meta.title,
            "Sousou no Frieren"
        );
        let list = [json!({"startDate": {"year": 2010}}), frieren()];
        assert_eq!(choose(&list, Some(2023)).unwrap()["id"], 154587);
    }
}
