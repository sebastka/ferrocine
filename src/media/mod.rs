//! Media files: what they are (probe), what their names say (naming), local
//! metadata (nfo), artwork, and the library scanner tying them together.

pub mod artwork;
pub mod disc;
pub mod dotignore;
pub mod naming;
pub mod nfo;
pub mod probe;
pub mod scanner;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Movie,
    Series,
    Season,
    Episode,
}

impl ItemKind {
    pub fn code(self) -> i16 {
        match self {
            ItemKind::Movie => 1,
            ItemKind::Series => 2,
            ItemKind::Season => 3,
            ItemKind::Episode => 4,
        }
    }

    pub fn from_code(code: i16) -> Self {
        match code {
            2 => ItemKind::Series,
            3 => ItemKind::Season,
            4 => ItemKind::Episode,
            _ => ItemKind::Movie,
        }
    }

    /// Movies and episodes have a file.
    pub fn playable(self) -> bool {
        matches!(self, ItemKind::Movie | ItemKind::Episode)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    /// `actor`, `director`, `writer`, …
    pub role: String,
    pub character: Option<String>,
}

/// A rating from one source, e.g. IMDb 8.7/10 or Rotten Tomatoes 88/100.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rating {
    /// `tmdb`, `imdb`, `rotten_tomatoes`, `metacritic`, `anilist`, `anidb`, `tvdb`.
    pub source: String,
    pub value: f64,
    /// The scale's maximum: 10 or 100.
    pub max: f64,
}

impl Rating {
    pub fn new(source: &str, value: f64, max: f64) -> Self {
        Self {
            source: source.to_owned(),
            value: (value * 100.0).round() / 100.0,
            max,
        }
    }
}

/// Descriptive metadata of an item, whatever its source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub title: String,
    pub original_title: Option<String>,
    /// ISO 639-1 code of the work's original language.
    pub original_language: Option<String>,
    /// The title a provider gave in the library's language, when it replaced
    /// the one from the file name; `title` is this or `original_title`
    /// (see [`Meta::choose_title`]).
    pub provider_title: Option<String>,
    pub sort_title: Option<String>,
    pub year: Option<i32>,
    pub premiere_date: Option<String>,
    pub overview: Option<String>,
    pub tagline: Option<String>,
    pub rating: Option<f64>,
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    pub studios: Vec<String>,
    pub people: Vec<Person>,
    pub tmdb_id: Option<String>,
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub anidb_id: Option<String>,
    pub anilist_id: Option<String>,
    pub runtime_ms: Option<i64>,
    /// Ratings by source; `rating` is the one used for sorting.
    pub ratings: Vec<Rating>,
}

impl Meta {
    /// Apply what a `.nfo` file says; it wins over everything else.
    pub fn overlay_nfo(&mut self, n: &nfo::Nfo) {
        fn set<T: Clone>(slot: &mut Option<T>, v: &Option<T>) {
            if v.is_some() {
                slot.clone_from(v);
            }
        }
        if let Some(t) = &n.title {
            self.title.clone_from(t);
            self.provider_title = None;
        }
        set(&mut self.original_title, &n.original_title);
        set(&mut self.sort_title, &n.sort_title);
        set(&mut self.year, &n.year);
        set(&mut self.premiere_date, &n.premiered);
        set(&mut self.overview, &n.plot);
        set(&mut self.tagline, &n.tagline);
        set(&mut self.rating, &n.rating);
        set(&mut self.official_rating, &n.mpaa);
        set(&mut self.tmdb_id, &n.tmdb);
        set(&mut self.imdb_id, &n.imdb);
        set(&mut self.tvdb_id, &n.tvdb);
        if let Some(m) = n.runtime_minutes {
            self.runtime_ms = Some(m * 60_000);
        }
        if !n.genres.is_empty() {
            self.genres.clone_from(&n.genres);
        }
        if !n.studios.is_empty() {
            self.studios.clone_from(&n.studios);
        }
        if !n.people.is_empty() {
            self.people.clone_from(&n.people);
        }
        if self.year.is_none() {
            self.year = year_of(self.premiere_date.as_deref());
        }
    }

    /// Fill what is still missing from a remote source. The title is
    /// replaced unless `keep_title` (it came from an `.nfo`).
    pub fn fill_from(&mut self, r: &Meta, keep_title: bool) {
        fn fill<T: Clone>(slot: &mut Option<T>, v: &Option<T>) {
            if slot.is_none() {
                slot.clone_from(v);
            }
        }
        if !keep_title && !r.title.is_empty() {
            self.title.clone_from(&r.title);
            self.provider_title = Some(r.title.clone());
        }
        // The original language goes with the original title it describes.
        if self.original_title.is_none() {
            self.original_title.clone_from(&r.original_title);
            self.original_language.clone_from(&r.original_language);
        } else if self.original_title == r.original_title || self.original_language.is_none() {
            fill(&mut self.original_language, &r.original_language);
        }
        fill(&mut self.year, &r.year);
        fill(&mut self.premiere_date, &r.premiere_date);
        fill(&mut self.overview, &r.overview);
        fill(&mut self.tagline, &r.tagline);
        fill(&mut self.rating, &r.rating);
        fill(&mut self.official_rating, &r.official_rating);
        fill(&mut self.tmdb_id, &r.tmdb_id);
        fill(&mut self.imdb_id, &r.imdb_id);
        fill(&mut self.tvdb_id, &r.tvdb_id);
        fill(&mut self.anidb_id, &r.anidb_id);
        fill(&mut self.anilist_id, &r.anilist_id);
        fill(&mut self.runtime_ms, &r.runtime_ms);
        for rating in &r.ratings {
            if !self.ratings.iter().any(|x| x.source == rating.source) {
                self.ratings.push(rating.clone());
            }
        }
        if self.genres.is_empty() {
            self.genres.clone_from(&r.genres);
        }
        if self.studios.is_empty() {
            self.studios.clone_from(&r.studios);
        }
        if self.people.is_empty() {
            self.people.clone_from(&r.people);
        }
    }
}

impl Meta {
    /// Show the original title instead of the provider's localized one when
    /// the original language is in `languages` (`"*"`: any). Titles from
    /// `.nfo` files and file names are left alone. Returns whether the title changed.
    pub fn choose_title(&mut self, languages: &[String]) -> bool {
        let Some(localized) = &self.provider_title else {
            return false;
        };
        let original = self.original_title.as_deref().filter(|t| !t.trim().is_empty());
        let wanted = languages.iter().any(|l| {
            l == "*"
                || self
                    .original_language
                    .as_deref()
                    .is_some_and(|o| crate::lang::same(o, l))
        });
        let title = match original {
            Some(o) if wanted => o.to_owned(),
            _ => localized.clone(),
        };
        if title == self.title {
            return false;
        }
        self.title = title;
        true
    }
}

/// Year of a `YYYY-MM-DD` date.
pub fn year_of(date: Option<&str>) -> Option<i32> {
    date.and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok())
        .filter(|y| *y > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_titles_by_language() {
        let remote = Meta {
            title: "Spirited Away".into(),
            original_title: Some("千と千尋の神隠し".into()),
            original_language: Some("ja".into()),
            ..Default::default()
        };
        let mut m = Meta {
            title: "spirited away".into(),
            ..Default::default()
        };
        m.fill_from(&remote, false);
        let langs = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!m.choose_title(&langs(&["ko"])));
        assert_eq!(m.title, "Spirited Away");
        assert!(m.choose_title(&langs(&["ko", "jpn"])));
        assert_eq!(m.title, "千と千尋の神隠し");
        // Back when the language is no longer listed.
        assert!(m.choose_title(&[]));
        assert_eq!(m.title, "Spirited Away");
        assert!(m.choose_title(&langs(&["*"])));
        assert_eq!(m.title, "千と千尋の神隠し");
        // A title from an .nfo file is the owner's: never switched.
        m.overlay_nfo(&nfo::Nfo {
            title: Some("Chihiro".into()),
            ..Default::default()
        });
        assert!(!m.choose_title(&langs(&["*"])));
        assert_eq!(m.title, "Chihiro");
    }

    #[test]
    fn nfo_wins_and_remote_fills_gaps() {
        let mut m = Meta {
            title: "the matrix".into(),
            year: Some(1999),
            ..Default::default()
        };
        m.overlay_nfo(&nfo::Nfo {
            title: Some("The Matrix".into()),
            plot: Some("Local plot".into()),
            premiered: Some("1999-03-31".into()),
            ..Default::default()
        });
        let remote = Meta {
            title: "Matrix".into(),
            overview: Some("Remote plot".into()),
            tagline: Some("Welcome".into()),
            genres: vec!["Action".into()],
            ..Default::default()
        };
        m.fill_from(&remote, true);
        assert_eq!(m.title, "The Matrix");
        assert_eq!(m.overview.as_deref(), Some("Local plot"));
        assert_eq!(m.tagline.as_deref(), Some("Welcome"));
        assert_eq!(m.genres, ["Action"]);
        m.fill_from(&remote, false);
        assert_eq!(m.title, "Matrix");
    }
}
