//! The running configuration, described for administrators: every setting
//! with its value, default, description and, for settings with a fixed set
//! of values, the alternatives.
//!
//! Built field by field from an allow-list, so that a secret can never be
//! returned by accident: secrets are reported as set or not, and the
//! database URL loses its password. A test fails when a configuration field
//! has no description here.

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    AnilistConfig, AuthConfig, Config, DatabaseConfig, Dur, HwAccel, LibraryConfig, LibraryKind, MetadataConfig,
    OidcConfig, ScannerConfig, ServerConfig, SubtitlesConfig, TitleLanguage, TranscodingConfig, default_anidb_images,
    default_anidb_titles, default_anidb_url, default_claim, default_connect_timeout, default_fanart_url,
    default_language, default_leeway, default_max_connections, default_metadata_directory, default_omdb_url,
    default_opensubtitles_url, default_scopes, default_tmdb_images, default_tmdb_url, default_tvdb_url,
};

/// When a change to a section takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Applies {
    /// At the next reload (SIGHUP or `POST /api/v1/reload`).
    Reload,
    /// At the next restart.
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    String,
    Integer,
    Boolean,
    Duration,
    Path,
    Url,
    Address,
    List,
    Choice,
    Secret,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub value: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Field {
    /// Path within the section, e.g. `oidc.issuer`.
    pub key: String,
    pub kind: Kind,
    /// The running value; `null` when unset or secret.
    pub value: Value,
    /// The value when the setting is absent; `null` when it has none
    /// (required, or simply off).
    pub default: Value,
    pub description: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    /// Secrets only: whether a value is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Item {
    pub title: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Serialize)]
pub struct Section {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub applies: Applies,
    pub fields: Vec<Field>,
    /// List sections (`libraries`): one entry per element.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<Item>>,
}

#[derive(Debug, Serialize)]
pub struct Described {
    pub sections: Vec<Section>,
}

fn field(key: &str, kind: Kind, value: Value, default: Value, description: &'static str) -> Field {
    Field {
        key: key.to_owned(),
        kind,
        value,
        default,
        description,
        choices: Vec::new(),
        set: None,
    }
}

fn choice(key: &str, value: &str, default: &str, description: &'static str, choices: Vec<Choice>) -> Field {
    Field {
        choices,
        ..field(key, Kind::Choice, json!(value), json!(default), description)
    }
}

fn secret(key: &str, set: bool, description: &'static str) -> Field {
    Field {
        set: Some(set),
        ..field(key, Kind::Secret, Value::Null, Value::Null, description)
    }
}

fn dur(d: Dur) -> Value {
    json!(d.to_string())
}

fn opt<T: Serialize>(v: &Option<T>) -> Value {
    v.as_ref().map_or(Value::Null, |v| json!(v))
}

/// A database URL without its password, nor the value of any query
/// parameter that looks like a credential.
pub fn redact_url(raw: &str) -> String {
    let Ok(mut u) = url::Url::parse(raw) else {
        return "(unparsable URL hidden)".into();
    };
    if u.password().is_some() {
        let _ = u.set_password(Some("********"));
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                let lower = k.to_lowercase();
                let hide = ["pass", "secret", "key", "token"].iter().any(|s| lower.contains(s));
                (k.into_owned(), if hide { "********".into() } else { v.into_owned() })
            })
            .collect();
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.to_string()
}

const X264_PRESETS: [(&str, &str); 9] = [
    ("ultrafast", "Fastest; largest files for a given quality."),
    ("superfast", "Very fast, low compression efficiency."),
    ("veryfast", "A good balance for live transcoding (the default)."),
    ("faster", "Somewhat slower, slightly better compression."),
    ("fast", "Slower; needs a fast CPU for real-time 1080p."),
    ("medium", "x264's own default; too slow for real-time 4K."),
    ("slow", "Better compression; rarely real-time."),
    ("slower", "Much slower; not suited to live playback."),
    ("veryslow", "Slowest; not suited to live playback."),
];

/// The x264 presets accepted by `transcoding.x264_preset`.
pub fn x264_presets() -> impl Iterator<Item = &'static str> {
    X264_PRESETS.iter().map(|(p, _)| *p)
}

fn server(s: &ServerConfig) -> Section {
    let d = ServerConfig::default();
    Section {
        key: "server",
        title: "Server",
        description: "The HTTP listener and the public address of the UI and API.",
        applies: Applies::Restart,
        fields: vec![
            field(
                "listen",
                Kind::Address,
                json!(s.listen.to_string()),
                json!(d.listen.to_string()),
                "Address and port the server listens on.",
            ),
            field(
                "public_url",
                Kind::Url,
                json!(s.public_url),
                Value::Null,
                "The URL users open (the UI's origin). Gives the OpenID Connect redirect URI and the origin checked against cross-site requests; https makes the session cookie Secure.",
            ),
            field(
                "web_dir",
                Kind::Path,
                opt(&s.web_dir),
                Value::Null,
                "Optional directory of a built UI served at `/`. Usually unset: the UI image serves the app and proxies the API.",
            ),
            secret(
                "metrics_token",
                s.metrics_token.is_some(),
                "Bearer token required by `/metrics`; when unset, metrics are open. Applies at the next reload.",
            ),
            field(
                "worker_threads",
                Kind::Integer,
                json!(s.worker_threads),
                json!(d.worker_threads),
                "Worker threads of the async runtime; 0 means one per CPU.",
            ),
        ],
        items: None,
    }
}

fn database(db: &DatabaseConfig) -> Section {
    Section {
        key: "database",
        title: "Database",
        description: "PostgreSQL or MariaDB, holding state, metadata and artwork.",
        applies: Applies::Restart,
        fields: vec![
            Field {
                choices: vec![
                    Choice {
                        value: "postgres://",
                        description: "PostgreSQL (also postgresql://).",
                    },
                    Choice {
                        value: "mariadb://",
                        description: "MariaDB, the tested MySQL-family target.",
                    },
                    Choice {
                        value: "mysql://",
                        description: "MySQL protocol; MySQL itself is not tested.",
                    },
                ],
                ..field(
                    "url",
                    Kind::Url,
                    json!(redact_url(&db.url)),
                    Value::Null,
                    "Connection URL; its scheme selects the engine. The password is never shown.",
                )
            },
            field(
                "max_connections",
                Kind::Integer,
                json!(db.max_connections),
                json!(default_max_connections()),
                "Size of the connection pool (at least 4).",
            ),
            field(
                "connect_timeout",
                Kind::Duration,
                dur(db.connect_timeout),
                dur(default_connect_timeout()),
                "How long to wait for a connection from the pool.",
            ),
        ],
        items: None,
    }
}

fn auth(a: &AuthConfig) -> Section {
    let d = AuthConfig::default();
    let mut fields = vec![
        field(
            "session_idle",
            Kind::Duration,
            dur(a.idle()),
            dur(d.idle()),
            "A session ends after this long unused.",
        ),
        field(
            "session_lifetime",
            Kind::Duration,
            dur(a.lifetime()),
            dur(d.lifetime()),
            "A session ends this long after sign-in, used or not.",
        ),
        field(
            "device_idle",
            Kind::Duration,
            dur(a.device_idle()),
            dur(d.device_idle()),
            "A device signed in with Quick Connect (a TV app) is signed out after this long unused.",
        ),
    ];
    fields.extend(oidc(a.oidc.as_ref()));
    Section {
        key: "auth",
        title: "Sign-in",
        description: "OpenID Connect sign-in, sessions, and who may use and administer the server. Changes apply at the next reload; sessions keep the groups they signed in with until the next sign-in.",
        applies: Applies::Reload,
        fields,
        items: None,
    }
}

fn oidc(o: Option<&OidcConfig>) -> Vec<Field> {
    let get = |f: fn(&OidcConfig) -> Value| o.map_or(Value::Null, f);
    vec![
        field(
            "oidc.issuer",
            Kind::Url,
            get(|o| json!(o.issuer)),
            Value::Null,
            "The provider's issuer URL; its discovery document is read from `/.well-known/openid-configuration`. https, or http on localhost only.",
        ),
        field(
            "oidc.client_id",
            Kind::String,
            get(|o| json!(o.client_id)),
            Value::Null,
            "This server's client id at the provider; also the expected audience of ID tokens.",
        ),
        secret(
            "oidc.client_secret",
            o.is_some_and(|o| o.client_secret.is_some()),
            "Client secret of a confidential client; unset for a public client (PKCE alone).",
        ),
        field(
            "oidc.scopes",
            Kind::List,
            get(|o| json!(o.scopes)),
            json!(default_scopes()),
            "Scopes requested at sign-in; must include `openid`.",
        ),
        field(
            "oidc.groups_claim",
            Kind::String,
            get(|o| json!(o.groups_claim)),
            json!(default_claim()),
            "Dotted path of the claim listing the user's groups or roles, read from the ID token, else from userinfo (e.g. `realm_access.roles` for Keycloak realm roles).",
        ),
        field(
            "oidc.users",
            Kind::List,
            get(|o| json!(o.users)),
            json!([]),
            "Values of the groups claim allowed to sign in; empty means anyone the provider accepts.",
        ),
        field(
            "oidc.admins",
            Kind::List,
            get(|o| json!(o.admins)),
            json!([]),
            "Values of the groups claim granting administration: scans, metadata refreshes, reloads, this page.",
        ),
        field(
            "oidc.leeway",
            Kind::Duration,
            get(|o| dur(o.leeway)),
            dur(default_leeway()),
            "Clock skew tolerated when checking ID token times.",
        ),
    ]
}

fn transcoding(t: &TranscodingConfig) -> Section {
    let d = TranscodingConfig::default();
    let hw = |h: HwAccel| match h {
        HwAccel::Auto => "auto",
        HwAccel::None => "none",
        HwAccel::Vaapi => "vaapi",
        HwAccel::Qsv => "qsv",
        HwAccel::Nvenc => "nvenc",
    };
    Section {
        key: "transcoding",
        title: "Transcoding",
        description: "ffmpeg, hardware acceleration and the limits of HLS playback sessions.",
        applies: Applies::Restart,
        fields: vec![
            field(
                "ffmpeg",
                Kind::Path,
                json!(t.ffmpeg),
                json!(d.ffmpeg),
                "The ffmpeg binary (a name looked up in PATH, or a path).",
            ),
            field(
                "ffprobe",
                Kind::Path,
                json!(t.ffprobe),
                json!(d.ffprobe),
                "The ffprobe binary, used by scans.",
            ),
            choice(
                "hw_accel",
                hw(t.hw_accel),
                hw(d.hw_accel),
                "Hardware acceleration for transcodes. Each candidate must pass a test encode at start; every session falls back to software when the hardware fails.",
                vec![
                    Choice {
                        value: "auto",
                        description: "Try VA-API, then Quick Sync, then NVENC; software when none works.",
                    },
                    Choice {
                        value: "none",
                        description: "Software only (libx264).",
                    },
                    Choice {
                        value: "vaapi",
                        description: "VA-API: Intel and AMD GPUs, through the render node in `device`.",
                    },
                    Choice {
                        value: "qsv",
                        description: "Intel Quick Sync, through the render node in `device`.",
                    },
                    Choice {
                        value: "nvenc",
                        description: "NVIDIA NVENC/CUDA; needs the NVIDIA container toolkit.",
                    },
                ],
            ),
            field(
                "device",
                Kind::Path,
                json!(t.device),
                json!(d.device),
                "DRM render node for VA-API and Quick Sync.",
            ),
            field(
                "temp_dir",
                Kind::Path,
                json!(t.temp_dir),
                json!(d.temp_dir),
                "Scratch space for the HLS segments of running sessions; wiped at start. Disk-backed preferably: a session holds up to about 13 minutes of video.",
            ),
            field(
                "max_sessions",
                Kind::Integer,
                json!(t.max_sessions),
                json!(d.max_sessions),
                "Concurrent remux and transcode sessions; further playbacks get 503.",
            ),
            field(
                "max_height",
                Kind::Integer,
                json!(t.max_height),
                json!(d.max_height),
                "Transcoded video is never taller than this (240 to 4320).",
            ),
            field(
                "tone_mapping",
                Kind::Boolean,
                json!(t.tone_mapping),
                json!(d.tone_mapping),
                "Tone-map HDR sources (HDR10, HLG, Dolby Vision) to SDR when transcoding them; without it their colours look washed out.",
            ),
            field(
                "segment_seconds",
                Kind::Integer,
                json!(t.segment_seconds),
                json!(d.segment_seconds),
                "HLS segment duration (2 to 10 s): shorter starts faster, longer means fewer requests.",
            ),
            Field {
                choices: X264_PRESETS
                    .iter()
                    .map(|(value, description)| Choice { value, description })
                    .collect(),
                ..choice(
                    "x264_preset",
                    &t.x264_preset,
                    &d.x264_preset,
                    "libx264 speed preset for software transcodes: faster presets need less CPU for slightly larger streams.",
                    Vec::new(),
                )
            },
        ],
        items: None,
    }
}

fn metadata(m: &MetadataConfig) -> Section {
    let d = MetadataConfig::default();
    let tmdb = m.tmdb.as_ref();
    let get = |f: fn(&super::TmdbConfig) -> Value| tmdb.map_or(Value::Null, f);
    Section {
        key: "metadata",
        title: "Metadata",
        description: "Where titles, descriptions and artwork come from: local files first, then TMDB.",
        applies: Applies::Reload,
        fields: vec![
            secret(
                "tmdb.api_key",
                tmdb.is_some(),
                "TMDB v4 read access token or v3 API key; TMDB lookups are off when unset.",
            ),
            field(
                "tmdb.language",
                Kind::String,
                get(|t| json!(t.language)),
                json!(default_language()),
                "Language of TMDB metadata (a libraries' `language` overrides it); its country gives the certification.",
            ),
            field(
                "tmdb.api_url",
                Kind::Url,
                get(|t| json!(t.api_url)),
                json!(default_tmdb_url()),
                "TMDB API base URL.",
            ),
            field(
                "tmdb.image_url",
                Kind::Url,
                get(|t| json!(t.image_url)),
                json!(default_tmdb_images()),
                "TMDB image base URL.",
            ),
            secret(
                "tvdb.api_key",
                m.tvdb.is_some(),
                "TheTVDB v4 API key; TheTVDB is used for series when set: first for libraries whose `episode_order` is not `aired`, else to fill what TMDB lacks.",
            ),
            secret(
                "tvdb.pin",
                m.tvdb.as_ref().is_some_and(|t| t.pin.is_some()),
                "Subscriber PIN, for user-supported TheTVDB keys.",
            ),
            field(
                "tvdb.api_url",
                Kind::Url,
                m.tvdb.as_ref().map_or(Value::Null, |t| json!(t.api_url)),
                json!(default_tvdb_url()),
                "TheTVDB API base URL.",
            ),
            secret(
                "omdb.api_key",
                m.omdb.is_some(),
                "OMDb API key; adds IMDb, Rotten Tomatoes and Metacritic ratings to items with an IMDb id.",
            ),
            field(
                "omdb.api_url",
                Kind::Url,
                m.omdb.as_ref().map_or(Value::Null, |o| json!(o.api_url)),
                json!(default_omdb_url()),
                "OMDb API base URL.",
            ),
            secret(
                "fanart.api_key",
                m.fanart.is_some(),
                "fanart.tv project API key; adds title logos, backdrops and posters.",
            ),
            secret(
                "fanart.client_key",
                m.fanart.as_ref().is_some_and(|f| f.client_key.is_some()),
                "fanart.tv personal key: newer images sooner.",
            ),
            field(
                "fanart.api_url",
                Kind::Url,
                m.fanart.as_ref().map_or(Value::Null, |f| json!(f.api_url)),
                json!(default_fanart_url()),
                "fanart.tv API base URL.",
            ),
            field(
                "anilist.enabled",
                Kind::Boolean,
                json!(m.anilist.enabled),
                json!(AnilistConfig::default().enabled),
                "Use AniList (no key needed) first for libraries with `anime: true`.",
            ),
            choice(
                "anilist.title",
                title(m.anilist.title),
                title(AnilistConfig::default().title),
                "Which AniList title to show.",
                vec![
                    Choice {
                        value: "english",
                        description: "The English title when there is one, else the romanised one.",
                    },
                    Choice {
                        value: "romaji",
                        description: "The romanised Japanese title (e.g. Sousou no Frieren).",
                    },
                    Choice {
                        value: "native",
                        description: "The title in its original script.",
                    },
                ],
            ),
            field(
                "anilist.api_url",
                Kind::Url,
                json!(m.anilist.api_url),
                json!(AnilistConfig::default().api_url),
                "AniList GraphQL endpoint.",
            ),
            field(
                "anidb.client",
                Kind::String,
                m.anidb.as_ref().map_or(Value::Null, |a| json!(a.client)),
                Value::Null,
                "Client name registered at AniDB (required by its API); AniDB is off when unset. Gives anime episode titles in AniDB's numbering.",
            ),
            field(
                "anidb.client_version",
                Kind::Integer,
                m.anidb.as_ref().map_or(Value::Null, |a| json!(a.client_version)),
                Value::Null,
                "The registered client version.",
            ),
            field(
                "anidb.api_url",
                Kind::Url,
                m.anidb.as_ref().map_or(Value::Null, |a| json!(a.api_url)),
                json!(default_anidb_url()),
                "AniDB HTTP API endpoint (asked at most every 2.5 s).",
            ),
            field(
                "anidb.titles_url",
                Kind::Url,
                m.anidb.as_ref().map_or(Value::Null, |a| json!(a.titles_url)),
                json!(default_anidb_titles()),
                "AniDB title dump, downloaded at most once a day to find anime by name.",
            ),
            field(
                "anidb.image_url",
                Kind::Url,
                m.anidb.as_ref().map_or(Value::Null, |a| json!(a.image_url)),
                json!(default_anidb_images()),
                "AniDB image base URL.",
            ),
            field(
                "prefer_original_title",
                Kind::List,
                json!(m.prefer_original_title),
                json!([]),
                "Languages (ISO 639-1) whose works keep their original title instead of the library language's, e.g. `[ja, ko]`; `[\"*\"]` for every language. Applies at the next scan, without fetching metadata again.",
            ),
            field(
                "nfo",
                Kind::Boolean,
                json!(m.nfo),
                json!(d.nfo),
                "Read Kodi-style .nfo files next to the media; they win over TMDB.",
            ),
            field(
                "local_artwork",
                Kind::Boolean,
                json!(m.local_artwork),
                json!(d.local_artwork),
                "Use poster.jpg, fanart.jpg, <name>-poster.jpg… next to the media.",
            ),
            field(
                "extract_thumbnails",
                Kind::Boolean,
                json!(m.extract_thumbnails),
                json!(d.extract_thumbnails),
                "Extract a frame with ffmpeg when a movie or episode has no image.",
            ),
            field(
                "poster_width",
                Kind::Integer,
                json!(m.poster_width),
                json!(d.poster_width),
                "Stored width of posters and thumbnails, in pixels; backdrops get twice that (at most 1920).",
            ),
            field(
                "jpeg_quality",
                Kind::Integer,
                json!(m.jpeg_quality),
                json!(d.jpeg_quality),
                "JPEG quality of stored artwork (1 to 100).",
            ),
            field(
                "directory",
                Kind::Path,
                json!(m.directory),
                json!(default_metadata_directory()),
                "Where artwork, seek bar previews and downloaded subtitles are kept: a writable volume, shared by instances that share the database. Changing it needs a restart; nothing is moved.",
            ),
            field(
                "trickplay.enabled",
                Kind::Boolean,
                json!(m.trickplay.enabled),
                json!(d.trickplay.enabled),
                "Make seek bar previews of movies, episodes and extras after scans, one item at a time (keyframes only).",
            ),
            field(
                "trickplay.interval",
                Kind::Duration,
                dur(m.trickplay.interval),
                dur(d.trickplay.interval),
                "Time between two preview frames (at least 1s).",
            ),
            field(
                "trickplay.width",
                Kind::Integer,
                json!(m.trickplay.width),
                json!(d.trickplay.width),
                "Width of a preview frame, in pixels (80 to 640); a film takes about 2 to 3 MB at 240.",
            ),
        ],
        items: None,
    }
}

fn title(t: TitleLanguage) -> &'static str {
    match t {
        TitleLanguage::English => "english",
        TitleLanguage::Romaji => "romaji",
        TitleLanguage::Native => "native",
    }
}

fn subtitles(s: &SubtitlesConfig) -> Section {
    let d = SubtitlesConfig::default();
    let os = s.opensubtitles.as_ref();
    Section {
        key: "subtitles",
        title: "Subtitles",
        description: "Subtitles from OpenSubtitles.com: searched on demand from the player, and fetched automatically during scans for the listed languages. They are stored in the database; libraries stay read-only.",
        applies: Applies::Reload,
        fields: vec![
            secret(
                "opensubtitles.api_key",
                os.is_some(),
                "OpenSubtitles.com API key (of a registered consumer); subtitle search is off when unset.",
            ),
            field(
                "opensubtitles.username",
                Kind::String,
                os.map_or(Value::Null, |o| json!(o.username)),
                Value::Null,
                "OpenSubtitles.com account: the API only lets signed-in users download, and counts downloads against the account's daily quota.",
            ),
            secret("opensubtitles.password", os.is_some(), "Password of that account."),
            field(
                "opensubtitles.api_url",
                Kind::Url,
                os.map_or(Value::Null, |o| json!(o.api_url)),
                json!(default_opensubtitles_url()),
                "OpenSubtitles.com API base URL.",
            ),
            field(
                "languages",
                Kind::List,
                json!(s.languages),
                json!([]),
                "Languages (ISO 639-1, e.g. `en`, `fr`) fetched automatically for movies and episodes that have no text subtitles in them; empty means on demand only.",
            ),
            field(
                "max_downloads_per_scan",
                Kind::Integer,
                json!(s.max_downloads_per_scan),
                json!(d.max_downloads_per_scan),
                "Automatic downloads per library scan, to spare the daily quota; the rest waits for the next scan.",
            ),
            field(
                "hearing_impaired",
                Kind::Boolean,
                json!(s.hearing_impaired),
                json!(d.hearing_impaired),
                "Prefer subtitles for the deaf and hard of hearing when choosing automatically.",
            ),
            field(
                "skip_when_audio_matches",
                Kind::Boolean,
                json!(s.skip_when_audio_matches),
                json!(d.skip_when_audio_matches),
                "Do not fetch a language automatically when the main audio track is in it.",
            ),
        ],
        items: None,
    }
}

fn scanner(s: &ScannerConfig) -> Section {
    let d = ScannerConfig::default();
    Section {
        key: "scanner",
        title: "Scanner",
        description: "When libraries are scanned. Scans are incremental: unchanged files are not probed again.",
        applies: Applies::Reload,
        fields: vec![
            field(
                "interval",
                Kind::Duration,
                dur(s.interval),
                dur(d.interval),
                "Scan every library this often; 0 disables periodic scans.",
            ),
            field(
                "on_start",
                Kind::Boolean,
                json!(s.on_start),
                json!(d.on_start),
                "Scan every library when the server starts.",
            ),
            field(
                "probe_concurrency",
                Kind::Integer,
                json!(s.probe_concurrency),
                json!(d.probe_concurrency),
                "Files probed and processed at once during a scan.",
            ),
            field(
                "retry_unmatched",
                Kind::Duration,
                dur(s.retry_unmatched),
                dur(d.retry_unmatched),
                "Items TMDB did not find are looked up again after this long.",
            ),
        ],
        items: None,
    }
}

fn library(l: &LibraryConfig) -> Item {
    let kind = |k: LibraryKind| k.as_str();
    Item {
        title: l.name.clone(),
        fields: vec![
            field(
                "id",
                Kind::String,
                json!(l.id),
                Value::Null,
                "Stable identifier keying every item of the library; changing it creates a new library.",
            ),
            field(
                "name",
                Kind::String,
                json!(l.name),
                Value::Null,
                "Name shown in the UI.",
            ),
            choice(
                "kind",
                kind(l.kind),
                "",
                "What the library holds; decides how files are named and grouped.",
                vec![
                    Choice {
                        value: "movies",
                        description: "One movie per file: `Title (Year)/Title (Year).mkv` or release names.",
                    },
                    Choice {
                        value: "shows",
                        description: "Series per top-level directory, with seasons and episodes (`S01E02`).",
                    },
                ],
            ),
            field(
                "paths",
                Kind::List,
                json!(l.paths),
                Value::Null,
                "Directories of the library, mounted read-only; nothing is ever written there.",
            ),
            field(
                "access",
                Kind::List,
                json!(l.access),
                json!([]),
                "Groups allowed to see the library; empty means everyone. Administrators see every library.",
            ),
            field(
                "language",
                Kind::String,
                opt(&l.language),
                Value::Null,
                "TMDB language for this library, overriding `metadata.tmdb.language`.",
            ),
            field(
                "anime",
                Kind::Boolean,
                json!(l.anime),
                json!(false),
                "Anime: AniList and AniDB first, and file names with absolute episode numbers such as `[Group] Show - 13 [1080p].mkv`.",
            ),
            field(
                "prefer_original_title",
                Kind::List,
                opt(&l.prefer_original_title),
                Value::Null,
                "Overrides `metadata.prefer_original_title` for this library; `[]` turns it off here.",
            ),
            choice(
                "episode_order",
                l.episode_order.as_str(),
                "aired",
                "Episode numbering of the files (show libraries).",
                vec![
                    Choice {
                        value: "aired",
                        description: "Broadcast order, as TMDB and TheTVDB number them by default.",
                    },
                    Choice {
                        value: "dvd",
                        description: "DVD order (TheTVDB; needs `metadata.tvdb`).",
                    },
                    Choice {
                        value: "absolute",
                        description: "One absolute numbering across seasons (TheTVDB; needs `metadata.tvdb`).",
                    },
                ],
            ),
        ],
    }
}

/// Describe a configuration.
pub fn describe(c: &Config) -> Described {
    Described {
        sections: vec![
            server(&c.server),
            database(&c.database),
            auth(&c.auth),
            transcoding(&c.transcoding),
            metadata(&c.metadata),
            scanner(&c.scanner),
            subtitles(&c.subtitles),
            Section {
                key: "libraries",
                title: "Libraries",
                description: "The media libraries and who may see them. New libraries, and libraries whose paths change, are scanned after a reload.",
                applies: Applies::Reload,
                fields: Vec::new(),
                items: Some(c.libraries.iter().map(library).collect()),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// A configuration with every optional setting set and secrets in every
    /// secret field.
    fn full() -> Config {
        super::super::from_str(
            r#"
server:
  public_url: https://media.example.com
  web_dir: /srv/ui
  metrics_token: SECRET-METRICS
database:
  url: postgres://fc:SECRET-DB@db/fc?sslmode=require&password=SECRET-QUERY
auth:
  session_idle: 7d
  session_lifetime: 30d
  device_idle: 60d
  oidc:
    issuer: https://id.example.com
    client_id: ferrocine
    client_secret: SECRET-CLIENT
    admins: [admins]
metadata:
  tmdb: { api_key: SECRET-TMDB }
  tvdb: { api_key: SECRET-TVDB, pin: SECRET-PIN }
  omdb: { api_key: SECRET-OMDB }
  fanart: { api_key: SECRET-FANART, client_key: SECRET-FANART-CLIENT }
  anidb: { client: ferrocine, client_version: 1 }
  prefer_original_title: [ja, ko]
subtitles:
  opensubtitles: { api_key: SECRET-OS, username: someone, password: SECRET-OS-PASSWORD }
  languages: [en]
libraries:
  - { id: movies, name: Movies, kind: movies, paths: [/media/movies], access: [family], language: fr-FR }
  - { id: anime, name: Anime, kind: shows, paths: [/media/anime], anime: true, episode_order: dvd,
      prefer_original_title: ["*"] }
"#,
        )
        .unwrap()
    }

    #[test]
    fn secrets_never_appear() {
        let text = serde_json::to_string(&describe(&full())).unwrap();
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("postgres://fc:********@db/fc?sslmode=require&password=********"));
    }

    /// Leaf keys of the serialized configuration: `section.key`,
    /// `section.sub.key`, `libraries[].key`.
    fn leaves(v: &Value, prefix: &str, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    leaves(v, &p, out);
                }
            }
            Value::Array(items) if items.iter().all(Value::is_object) && !items.is_empty() => {
                for item in items {
                    leaves(item, &format!("{prefix}[]"), out);
                }
            }
            _ => {
                out.insert(prefix.to_owned());
            }
        }
    }

    #[test]
    fn every_setting_is_described() {
        let c = full();
        let mut schema = BTreeSet::new();
        leaves(&serde_json::to_value(&c).unwrap(), "", &mut schema);
        let mut described = BTreeSet::new();
        for s in describe(&c).sections {
            for f in s.fields {
                described.insert(format!("{}.{}", s.key, f.key));
            }
            for item in s.items.into_iter().flatten() {
                for f in item.fields {
                    described.insert(format!("{}[].{}", s.key, f.key));
                }
            }
        }
        let missing: Vec<_> = schema.difference(&described).collect();
        let extra: Vec<_> = described.difference(&schema).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "missing {missing:?}, extra {extra:?}"
        );
    }

    #[test]
    fn choices_include_the_running_value() {
        for s in describe(&full()).sections {
            for f in s
                .fields
                .iter()
                .chain(s.items.iter().flatten().flat_map(|i| i.fields.iter()))
            {
                if f.kind == Kind::Choice {
                    assert!(
                        f.choices.iter().any(|c| json!(c.value) == f.value),
                        "{}: {:?}",
                        f.key,
                        f.value
                    );
                }
            }
        }
        assert_eq!(redact_url("mariadb://u@h/db"), "mariadb://u@h/db");
    }
}
