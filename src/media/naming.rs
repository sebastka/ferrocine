//! What file and directory names say: titles, years, seasons, episodes and
//! provider ids (`{tmdb-603}`, `[imdbid-tt0133093]`).

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

pub const VIDEO_EXTENSIONS: [&str; 17] = [
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "ts", "m2ts", "mts", "webm", "mpg", "mpeg", "flv", "ogv", "3gp", "vob",
    "divx",
];
pub const SUBTITLE_EXTENSIONS: [&str; 4] = ["srt", "vtt", "ass", "ssa"];
pub const IMAGE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// Directories never scanned: extras, NAS metadata, recycle bins.
const SKIPPED_DIRS: [&str; 17] = [
    "extras",
    "featurettes",
    "behind the scenes",
    "deleted scenes",
    "interviews",
    "scenes",
    "shorts",
    "trailers",
    "sample",
    "samples",
    "other",
    "@eadir",
    "#recycle",
    "#snapshot",
    ".trickplay",
    "lost+found",
    "$recycle.bin",
];

/// Release-name tokens after which nothing belongs to the title.
const JUNK: [&str; 46] = [
    "2160p",
    "1080p",
    "1080i",
    "720p",
    "576p",
    "480p",
    "4k",
    "uhd",
    "hdr",
    "hdr10",
    "dv",
    "bluray",
    "blu-ray",
    "bdrip",
    "brrip",
    "bdremux",
    "remux",
    "webrip",
    "web-dl",
    "webdl",
    "web",
    "hdtv",
    "dvdrip",
    "dvd",
    "x264",
    "x265",
    "h264",
    "h265",
    "h.264",
    "h.265",
    "hevc",
    "avc",
    "xvid",
    "divx",
    "10bit",
    "aac",
    "ac3",
    "dts",
    "atmos",
    "proper",
    "repack",
    "extended",
    "unrated",
    "multi",
    "truefrench",
    "vostfr",
];

static PROVIDER_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[{]\s*(tmdb|tmdbid|imdb|imdbid|tvdb|tvdbid)\s*[-=]\s*([a-z0-9]+)\s*[\]}]").unwrap()
});
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]|\{[^}]*\}").unwrap());
static EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^a-z0-9])s(\d{1,2})[ ._-]?e(\d{1,3})(?:[ ._-]?-?e\d{1,3})*(?:[^0-9]|$)|(?:^|[^0-9])(\d{1,2})x(\d{2,3})(?:[^0-9]|$)").unwrap()
});
static PARENTHESISED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([^)]*\)").unwrap());
static ABSOLUTE_DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)-\s*(\d{1,4})(?:v\d+)?(?:\s|$)").unwrap());
static ABSOLUTE_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[\s_.])(?:ep?|episode)\.?\s*(\d{1,4})(?:v\d+)?(?:[\s_.]|$)").unwrap());
static SEASON_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:season|series|saison|staffel|temporada|s)[ ._-]*(\d{1,3})$").unwrap());
static LEADING_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:e|ep|episode)?[ ._-]*(\d{1,3})(?:[ ._-]|$)").unwrap());

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderIds {
    pub tmdb: Option<String>,
    pub imdb: Option<String>,
    pub tvdb: Option<String>,
}

impl ProviderIds {
    pub fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.imdb.is_none() && self.tvdb.is_none()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TitleYear {
    pub title: String,
    pub year: Option<i32>,
    pub ids: ProviderIds,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeName {
    pub season: Option<i32>,
    pub episode: i32,
    /// Text after the episode number, if any.
    pub title: Option<String>,
    /// Text before the episode number (the series, for files at the root).
    pub prefix: Option<String>,
}

pub fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub fn is_skipped_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    name.starts_with('.') || SKIPPED_DIRS.contains(&lower.as_str())
}

/// Trailers and samples next to the main file.
pub fn is_extra_file(stem: &str) -> bool {
    let lower = stem.to_lowercase();
    lower == "sample"
        || lower == "trailer"
        || [
            "-trailer",
            ".trailer",
            "-sample",
            ".sample",
            "-featurette",
            "-extra",
            "-behindthescenes",
        ]
        .iter()
        .any(|s| lower.ends_with(s))
}

fn provider_ids(s: &str) -> ProviderIds {
    let mut ids = ProviderIds::default();
    for c in PROVIDER_ID.captures_iter(s) {
        let value = c[2].to_owned();
        match c[1].to_lowercase().as_str() {
            "tmdb" | "tmdbid" => ids.tmdb = Some(value),
            "imdb" | "imdbid" => ids.imdb = Some(value),
            _ => ids.tvdb = Some(value),
        }
    }
    ids
}

fn is_year(token: &str) -> Option<i32> {
    let t = token.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']'));
    if t.len() != 4 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = t.parse().ok()?;
    (1880..=2100).contains(&y).then_some(y)
}

/// Replace separators by spaces: dots and underscores when the name has no
/// space (release names), underscores only otherwise ("Mr. Robot").
fn normalize(s: &str) -> String {
    let spaced = if s.contains(' ') {
        s.replace('_', " ")
    } else {
        s.replace(['.', '_'], " ")
    };
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tidy(s: &str) -> String {
    s.trim()
        .trim_end_matches(|c: char| matches!(c, '-' | '(' | '[' | ',' | ':' | '.') || c.is_whitespace())
        .trim_start_matches(|c: char| matches!(c, '-' | ')' | ']' | ',' | ':' | '.') || c.is_whitespace())
        .to_owned()
}

/// Title, year and provider ids from a movie or series name such as
/// `The.Matrix.1999.1080p.BluRay.x264` or `Alien (1979) {tmdb-348}`.
pub fn parse_title_year(name: &str) -> TitleYear {
    let ids = provider_ids(name);
    let without_ids = BRACKETED.replace_all(name, " ");
    let normalized = normalize(&without_ids);
    let tokens: Vec<&str> = normalized.split(' ').collect();
    // The last year that is not the first token ("1917", "2001 A Space Odyssey").
    let year_at = tokens
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(i, t)| is_year(t).map(|y| (i, y)))
        .next_back();
    let junk_at = tokens
        .iter()
        .position(|t| JUNK.contains(&t.to_lowercase().trim_matches(|c| c == '(' || c == ')')));
    // The title ends at the year when there is one ("Space HDR (2023)",
    // "Web Clip (2021)"), else at the first release-name token.
    let end = match (year_at, junk_at) {
        (Some((y, _)), _) => y,
        (None, Some(j)) if j > 0 => j,
        _ => tokens.len(),
    };
    let mut title = tidy(&tokens[..end].join(" "));
    if title.is_empty() {
        title = tidy(&normalized);
    }
    TitleYear {
        title,
        year: year_at.map(|(_, y)| y),
        ids,
    }
}

/// Season and episode numbers from an episode file name.
pub fn parse_episode(stem: &str) -> Option<EpisodeName> {
    let c = EPISODE.captures(stem)?;
    let (season, episode, m) = if let (Some(s), Some(e)) = (c.get(1), c.get(2)) {
        (s.as_str().parse().ok()?, e.as_str().parse().ok()?, c.get(0)?)
    } else {
        (
            c.get(3)?.as_str().parse().ok()?,
            c.get(4)?.as_str().parse().ok()?,
            c.get(0)?,
        )
    };
    let prefix = tidy(&normalize(&BRACKETED.replace_all(&stem[..m.start()], " ")));
    let rest = &stem[m.end()..];
    // "S01E01.1080p.WEB" leaves only junk: no title.
    let title = (!looks_like_junk(rest))
        .then(|| parse_title_year(rest).title)
        .filter(|t| !t.is_empty());
    Some(EpisodeName {
        season: Some(season),
        episode,
        title,
        prefix: (!prefix.is_empty()).then_some(prefix),
    })
}

fn looks_like_junk(s: &str) -> bool {
    let n = normalize(s);
    let first = n.split(' ').next().unwrap_or_default().to_lowercase();
    n.is_empty() || JUNK.contains(&first.as_str())
}

/// An episode without SxxEyy inside a season directory: `03 - Title.mkv`.
pub fn parse_numbered_episode(stem: &str) -> Option<EpisodeName> {
    let normalized = normalize(stem);
    let lower = normalized.to_lowercase();
    let c = LEADING_NUMBER.captures(&lower)?;
    let episode: i32 = c[1].parse().ok()?;
    let rest = &normalized[c.get(0)?.end().min(normalized.len())..];
    let title = parse_title_year(rest).title;
    Some(EpisodeName {
        season: None,
        episode,
        title: (!title.is_empty() && !looks_like_junk(&title)).then_some(title),
        prefix: None,
    })
}

/// An anime episode with an absolute number: `[Group] Show - 13 [1080p]`,
/// `Show - 13v2`, `Show Ep 13`. Release groups, CRCs and resolutions in
/// brackets or parentheses are ignored.
pub fn parse_absolute_episode(stem: &str) -> Option<EpisodeName> {
    let cleaned = PARENTHESISED
        .replace_all(&BRACKETED.replace_all(stem, " "), " ")
        .into_owned();
    let normalized = normalize(&cleaned);
    let m = ABSOLUTE_DASH
        .captures_iter(&normalized)
        .last()
        .or_else(|| ABSOLUTE_EP.captures_iter(&normalized).last())?;
    let number = m.get(1)?;
    let episode: i32 = number.as_str().parse().ok()?;
    // Not a year, and not a resolution such as 1080.
    if (1900..=2100).contains(&episode) || episode == 0 {
        return None;
    }
    let whole = m.get(0)?;
    let prefix = tidy(&normalized[..whole.start()]);
    let rest = tidy(&normalized[whole.end().min(normalized.len())..]);
    Some(EpisodeName {
        season: None,
        episode,
        title: (!rest.is_empty() && !looks_like_junk(&rest)).then_some(rest),
        prefix: (!prefix.is_empty()).then_some(prefix),
    })
}

/// Season number of a directory name: `Season 1`, `S02`, `Specials` (0).
pub fn parse_season_dir(name: &str) -> Option<i32> {
    let n = normalize(name);
    if n.eq_ignore_ascii_case("specials") || n.eq_ignore_ascii_case("extras specials") {
        return Some(0);
    }
    SEASON_DIR.captures(&n).and_then(|c| c[1].parse().ok())
}

/// Sort key: lower case without a leading article.
pub fn sort_title(title: &str) -> String {
    let lower = title.trim().to_lowercase();
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = lower.strip_prefix(article)
            && !rest.is_empty()
        {
            return rest.to_owned();
        }
    }
    lower
}

/// Language and flags of a sidecar subtitle: `Movie.en.forced.srt` → (`en`, forced).
pub fn subtitle_tags(video_stem: &str, subtitle_name: &str) -> Option<(Option<String>, bool, Option<String>)> {
    let rest = subtitle_name.strip_prefix(video_stem)?;
    if !rest.starts_with('.') {
        return None;
    }
    // `.en.forced.srt` → `en.forced`.
    let inner = rest
        .rsplit_once('.')
        .map_or("", |(tags, _)| tags)
        .trim_start_matches('.');
    let mut language = None;
    let mut forced = false;
    let mut labels = Vec::new();
    for tag in inner.split('.').filter(|t| !t.is_empty()) {
        let lower = tag.to_lowercase();
        if lower == "forced" || lower == "foreign" {
            forced = true;
        } else if language.is_none() && (2..=3).contains(&tag.len()) && tag.chars().all(|c| c.is_ascii_alphabetic()) {
            language = Some(lower);
        } else {
            labels.push(tag.to_owned());
        }
    }
    let title = (!labels.is_empty()).then(|| labels.join(" "));
    Some((language, forced, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> (String, Option<i32>) {
        let t = parse_title_year(s);
        (t.title, t.year)
    }

    #[test]
    fn movie_names() {
        assert_eq!(
            ty("The.Matrix.1999.1080p.BluRay.x264"),
            ("The Matrix".into(), Some(1999))
        );
        assert_eq!(ty("Alien (1979)"), ("Alien".into(), Some(1979)));
        assert_eq!(ty("1917 (2019)"), ("1917".into(), Some(2019)));
        assert_eq!(
            ty("2001 A Space Odyssey (1968)"),
            ("2001 A Space Odyssey".into(), Some(1968))
        );
        assert_eq!(ty("Blade Runner 2049 (2017)"), ("Blade Runner 2049".into(), Some(2017)));
        assert_eq!(ty("Mr. Nobody"), ("Mr. Nobody".into(), None));
        assert_eq!(ty("Heat [1995] [1080p]"), ("Heat".into(), None));
        assert_eq!(ty("Some_Movie_720p"), ("Some Movie".into(), None));
        assert_eq!(ty("Space HDR (2023)"), ("Space HDR".into(), Some(2023)));
        assert_eq!(ty("Web Clip (2021)"), ("Web Clip".into(), Some(2021)));
        assert_eq!(ty("Web.2021.1080p.WEB-DL"), ("Web".into(), Some(2021)));
        let t = parse_title_year("Alien (1979) {tmdb-348} [imdbid-tt0078748]");
        assert_eq!(t.title, "Alien");
        assert_eq!(t.ids.tmdb.as_deref(), Some("348"));
        assert_eq!(t.ids.imdb.as_deref(), Some("tt0078748"));
    }

    #[test]
    fn episode_names() {
        let e = parse_episode("Breaking Bad - S01E02 - Cat's in the Bag").unwrap();
        assert_eq!((e.season, e.episode), (Some(1), 2));
        assert_eq!(e.title.as_deref(), Some("Cat's in the Bag"));
        assert_eq!(e.prefix.as_deref(), Some("Breaking Bad"));
        let e = parse_episode("show.name.s02e10.720p.hdtv.x264").unwrap();
        assert_eq!((e.season, e.episode, e.title), (Some(2), 10, None));
        assert_eq!(e.prefix.as_deref(), Some("show name"));
        let e = parse_episode("Show 3x07 Title").unwrap();
        assert_eq!((e.season, e.episode), (Some(3), 7));
        assert_eq!(e.title.as_deref(), Some("Title"));
        let e = parse_episode("S01E01E02").unwrap();
        assert_eq!(e.episode, 1);
        assert!(parse_episode("The Matrix 1999").is_none());
        assert!(parse_episode("Movie 1920x1080").is_none());
    }

    #[test]
    fn numbered_episodes_and_season_dirs() {
        let e = parse_numbered_episode("03 - The Third").unwrap();
        assert_eq!((e.episode, e.title.as_deref()), (3, Some("The Third")));
        assert_eq!(parse_numbered_episode("Episode 12").unwrap().episode, 12);
        assert_eq!(parse_season_dir("Season 01"), Some(1));
        assert_eq!(parse_season_dir("S2"), Some(2));
        assert_eq!(parse_season_dir("Specials"), Some(0));
        assert_eq!(parse_season_dir("Saison 3"), Some(3));
        assert_eq!(parse_season_dir("Extras"), None);
    }

    #[test]
    fn absolute_anime_episodes() {
        let e = parse_absolute_episode("[SubsPlease] Sousou no Frieren - 13 (1080p) [ABCD1234]").unwrap();
        assert_eq!((e.episode, e.prefix.as_deref()), (13, Some("Sousou no Frieren")));
        assert_eq!(parse_absolute_episode("Frieren - 07v2").unwrap().episode, 7);
        assert_eq!(
            parse_absolute_episode("One Piece Ep 1071 - Title").unwrap().episode,
            1071
        );
        let e = parse_absolute_episode("Re Zero - Starting Life - 05 - The Morning").unwrap();
        assert_eq!(
            (e.episode, e.prefix.as_deref(), e.title.as_deref()),
            (5, Some("Re Zero - Starting Life"), Some("The Morning"))
        );
        assert!(parse_absolute_episode("Akira (1988)").is_none());
        assert!(parse_absolute_episode("Movie - 2001").is_none());
    }

    #[test]
    fn sorting_and_skipping() {
        assert_eq!(sort_title("The Matrix"), "matrix");
        assert_eq!(sort_title("A"), "a");
        assert!(is_skipped_dir("Extras") && is_skipped_dir(".hidden") && is_skipped_dir("@eaDir"));
        assert!(is_extra_file("Movie-trailer") && is_extra_file("sample"));
        assert!(!is_extra_file("Sampler"));
    }

    #[test]
    fn subtitle_sidecars() {
        assert_eq!(
            subtitle_tags("Movie (2000)", "Movie (2000).en.forced.srt"),
            Some((Some("en".into()), true, None))
        );
        assert_eq!(
            subtitle_tags("Movie", "Movie.fre.SDH.srt"),
            Some((Some("fre".into()), false, Some("SDH".into())))
        );
        assert_eq!(subtitle_tags("Movie", "Movie.srt"), Some((None, false, None)));
        assert_eq!(subtitle_tags("Movie", "Movie2.srt"), None);
    }
}
