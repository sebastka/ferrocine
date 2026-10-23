//! Typed configuration. Unknown keys are errors everywhere.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Dur;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(test, derive(Serialize))]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub transcoding: TranscodingConfig,
    pub metadata: MetadataConfig,
    pub scanner: ScannerConfig,
    pub subtitles: SubtitlesConfig,
    pub libraries: Vec<LibraryConfig>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    /// External URL of the UI and API (same origin), e.g. `https://media.example.com`.
    /// Used for the OIDC redirect URI, the CSRF origin check and cookie security.
    pub public_url: String,
    /// Optional directory of static UI files served at `/` (SPA fallback to `index.html`).
    pub web_dir: Option<PathBuf>,
    /// Bearer token for `/metrics`; open when unset.
    pub metrics_token: Option<String>,
    /// Tokio worker threads; 0 = one per CPU.
    pub worker_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8096)),
            public_url: String::new(),
            web_dir: None,
            metrics_token: None,
            worker_threads: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// `postgres://…`, `mariadb://…` or `mysql://…`.
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: Dur,
}

pub(crate) fn default_max_connections() -> u32 {
    10
}

pub(crate) fn default_connect_timeout() -> Dur {
    Dur::secs(10)
}

#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub oidc: Option<OidcConfig>,
    /// Sessions end after this long without use.
    pub session_idle: Option<Dur>,
    /// Sessions end this long after sign-in, used or not.
    pub session_lifetime: Option<Dur>,
}

impl AuthConfig {
    pub fn idle(&self) -> Dur {
        self.session_idle.unwrap_or(Dur::secs(14 * 86_400))
    }

    pub fn lifetime(&self) -> Dur {
        self.session_lifetime.unwrap_or(Dur::secs(90 * 86_400))
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// Absent for a public client (PKCE only).
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Dotted path of the claim holding the user's groups or roles, e.g.
    /// `groups` or `realm_access.roles`. Read from the ID token, else from userinfo.
    #[serde(default = "default_claim")]
    pub groups_claim: String,
    /// Values of `groups_claim` allowed to sign in; empty = every authenticated user.
    #[serde(default)]
    pub users: Vec<String>,
    /// Values of `groups_claim` granting administration.
    #[serde(default)]
    pub admins: Vec<String>,
    /// Clock skew tolerated on `exp`, `iat` and `nbf`.
    #[serde(default = "default_leeway")]
    pub leeway: Dur,
}

pub(crate) fn default_scopes() -> Vec<String> {
    ["openid", "profile", "email"].map(String::from).to_vec()
}

pub(crate) fn default_claim() -> String {
    "groups".into()
}

pub(crate) fn default_leeway() -> Dur {
    Dur::secs(60)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HwAccel {
    /// Probe VA-API, then QSV, then NVENC; software when none works.
    #[default]
    Auto,
    None,
    Vaapi,
    Qsv,
    Nvenc,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct TranscodingConfig {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub hw_accel: HwAccel,
    /// DRM render node for VA-API and QSV.
    pub device: PathBuf,
    /// Scratch space for HLS segments; ephemeral (an emptyDir or tmpfs is fine).
    pub temp_dir: PathBuf,
    /// Concurrent HLS sessions (remux and transcode); further requests get 503.
    pub max_sessions: usize,
    /// Upper bound of the transcoded video height.
    pub max_height: u32,
    /// Tone-map HDR sources (HDR10, HLG, Dolby Vision) to SDR when
    /// transcoding them; without it their colours look washed out.
    pub tone_mapping: bool,
    /// HLS segment duration.
    pub segment_seconds: u32,
    /// Software encoder preset (libx264).
    pub x264_preset: String,
}

impl Default for TranscodingConfig {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            hw_accel: HwAccel::Auto,
            device: "/dev/dri/renderD128".into(),
            temp_dir: std::env::temp_dir().join("ferrocine"),
            max_sessions: 8,
            max_height: 2160,
            tone_mapping: true,
            segment_seconds: 4,
            x264_preset: "veryfast".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct MetadataConfig {
    pub tmdb: Option<TmdbConfig>,
    /// TheTVDB, for series: other episode orders (DVD, absolute) and gaps.
    pub tvdb: Option<TvdbConfig>,
    /// OMDb: IMDb, Rotten Tomatoes and Metacritic ratings.
    pub omdb: Option<OmdbConfig>,
    /// fanart.tv: logos and more artwork.
    pub fanart: Option<FanartConfig>,
    /// AniList, for anime libraries (no key needed).
    pub anilist: AnilistConfig,
    /// AniDB, for anime libraries: episode titles in AniDB's numbering.
    pub anidb: Option<AnidbConfig>,
    /// Languages (ISO 639-1) whose works keep their original title instead
    /// of the library language's, e.g. `[ja, ko]`; `["*"]` for every language.
    pub prefer_original_title: Vec<String>,
    /// Read Kodi-style `.nfo` files next to the media.
    pub nfo: bool,
    /// Use artwork files next to the media (`poster.jpg`, `fanart.jpg`, …).
    pub local_artwork: bool,
    /// Extract a frame with ffmpeg when an episode or movie has no image.
    pub extract_thumbnails: bool,
    /// Width of stored posters and thumbnails; backdrops get twice that (at most 1920).
    pub poster_width: u32,
    /// JPEG quality of stored images.
    pub jpeg_quality: u8,
}

impl Default for MetadataConfig {
    fn default() -> Self {
        Self {
            tmdb: None,
            tvdb: None,
            omdb: None,
            fanart: None,
            anilist: AnilistConfig::default(),
            anidb: None,
            prefer_original_title: Vec::new(),
            nfo: true,
            local_artwork: true,
            extract_thumbnails: true,
            poster_width: 600,
            jpeg_quality: 85,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct TmdbConfig {
    /// A v4 "API read access token" (a JWT) or a v3 API key.
    pub api_key: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_tmdb_url")]
    pub api_url: String,
    #[serde(default = "default_tmdb_images")]
    pub image_url: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct TvdbConfig {
    /// A v4 API key (a subscriber's personal key, or a project key with `pin`).
    pub api_key: String,
    /// Subscriber PIN, for user-supported keys.
    #[serde(default)]
    pub pin: Option<String>,
    #[serde(default = "default_tvdb_url")]
    pub api_url: String,
}

pub(crate) fn default_tvdb_url() -> String {
    "https://api4.thetvdb.com/v4".into()
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct OmdbConfig {
    pub api_key: String,
    #[serde(default = "default_omdb_url")]
    pub api_url: String,
}

pub(crate) fn default_omdb_url() -> String {
    "https://www.omdbapi.com".into()
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct FanartConfig {
    /// The project API key.
    pub api_key: String,
    /// Optional personal key: fresher images.
    #[serde(default)]
    pub client_key: Option<String>,
    #[serde(default = "default_fanart_url")]
    pub api_url: String,
}

pub(crate) fn default_fanart_url() -> String {
    "https://webservice.fanart.tv/v3".into()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleLanguage {
    /// English title when there is one, else romaji.
    #[default]
    English,
    Romaji,
    Native,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct AnilistConfig {
    /// Use AniList for anime libraries.
    pub enabled: bool,
    /// Which title to show.
    pub title: TitleLanguage,
    pub api_url: String,
}

impl Default for AnilistConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            title: TitleLanguage::English,
            api_url: "https://graphql.anilist.co".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct AnidbConfig {
    /// The client name registered at AniDB (required by its API).
    pub client: String,
    /// The registered client version.
    pub client_version: u32,
    #[serde(default = "default_anidb_url")]
    pub api_url: String,
    /// The daily title dump used to find anime by name.
    #[serde(default = "default_anidb_titles")]
    pub titles_url: String,
    #[serde(default = "default_anidb_images")]
    pub image_url: String,
}

pub(crate) fn default_anidb_url() -> String {
    "http://api.anidb.net:9001/httpapi".into()
}

pub(crate) fn default_anidb_titles() -> String {
    "https://anidb.net/api/anime-titles.xml.gz".into()
}

pub(crate) fn default_anidb_images() -> String {
    "https://cdn-eu.anidb.net/images/main".into()
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct SubtitlesConfig {
    pub opensubtitles: Option<OpenSubtitlesConfig>,
    /// Languages (ISO 639-1, e.g. `en`) fetched automatically during scans
    /// for movies and episodes that have none; empty = on demand only.
    pub languages: Vec<String>,
    /// Automatic downloads per scan, to spare the daily quota.
    pub max_downloads_per_scan: u32,
    /// Prefer subtitles for the deaf and hard of hearing when choosing automatically.
    pub hearing_impaired: bool,
    /// Do not fetch a language automatically when the main audio is in it.
    pub skip_when_audio_matches: bool,
}

impl Default for SubtitlesConfig {
    fn default() -> Self {
        Self {
            opensubtitles: None,
            languages: Vec::new(),
            max_downloads_per_scan: 20,
            hearing_impaired: false,
            skip_when_audio_matches: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct OpenSubtitlesConfig {
    /// The API key of a consumer registered at opensubtitles.com.
    pub api_key: String,
    /// An opensubtitles.com account: the API only downloads for a signed-in
    /// user, and counts downloads against the account's daily quota.
    pub username: String,
    pub password: String,
    #[serde(default = "default_opensubtitles_url")]
    pub api_url: String,
}

pub(crate) fn default_opensubtitles_url() -> String {
    "https://api.opensubtitles.com/api/v1".into()
}

pub(crate) fn default_language() -> String {
    "en-US".into()
}

pub(crate) fn default_tmdb_url() -> String {
    "https://api.themoviedb.org/3".into()
}

pub(crate) fn default_tmdb_images() -> String {
    "https://image.tmdb.org/t/p".into()
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub struct ScannerConfig {
    /// Scan every library this often; `0` disables periodic scans.
    pub interval: Dur,
    /// Scan when the process starts.
    pub on_start: bool,
    /// Concurrent ffprobe processes.
    pub probe_concurrency: usize,
    /// Retry remote metadata lookups that found nothing after this long.
    pub retry_unmatched: Dur,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            interval: Dur::secs(6 * 3600),
            on_start: true,
            probe_concurrency: 4,
            retry_unmatched: Dur::secs(7 * 86_400),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    Movies,
    Shows,
}

/// Which episode numbering the files of a show library follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EpisodeOrder {
    /// Broadcast order (TMDB's and TheTVDB's default).
    #[default]
    Aired,
    /// DVD order (TheTVDB).
    Dvd,
    /// Absolute numbering across seasons (TheTVDB).
    Absolute,
}

impl EpisodeOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            EpisodeOrder::Aired => "aired",
            EpisodeOrder::Dvd => "dvd",
            EpisodeOrder::Absolute => "absolute",
        }
    }
}

impl LibraryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Movies => "movies",
            LibraryKind::Shows => "shows",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    /// Stable identifier (letters, digits, `_-.`): keys every item.
    pub id: String,
    pub name: String,
    pub kind: LibraryKind,
    /// Directories, mounted read-only. Nothing is ever written there.
    pub paths: Vec<PathBuf>,
    /// Values of the OIDC groups claim allowed to see the library; empty = every user.
    #[serde(default)]
    pub access: Vec<String>,
    /// Metadata language override (TMDB), e.g. `fr-FR`.
    #[serde(default)]
    pub language: Option<String>,
    /// Anime: AniList and AniDB first, and file names such as
    /// `[Group] Show - 13 [1080p].mkv` (absolute episode numbers).
    #[serde(default)]
    pub anime: bool,
    /// Episode numbering of the files (show libraries).
    #[serde(default)]
    pub episode_order: EpisodeOrder,
    /// Overrides `metadata.prefer_original_title` (`[]` turns it off here).
    #[serde(default)]
    pub prefer_original_title: Option<Vec<String>>,
}

impl LibraryConfig {
    /// The languages whose original titles this library prefers.
    pub fn original_title_languages<'a>(&'a self, m: &'a MetadataConfig) -> &'a [String] {
        self.prefer_original_title
            .as_deref()
            .unwrap_or(&m.prefer_original_title)
    }
}
