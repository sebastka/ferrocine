//! Configuration: YAML files (or a directory of them) with `${ENV}`/`${file:}`
//! substitution, merged and validated into a [`Config`].

pub mod describe;
mod duration;
mod interpolate;
mod schema;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

pub use duration::Dur;
pub use schema::*;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: invalid YAML: {message}")]
    Yaml { path: PathBuf, message: String },
    #[error(transparent)]
    Interpolate(#[from] interpolate::InterpolateError),
    #[error("{file}: {path}: {message}")]
    Schema {
        file: String,
        path: String,
        message: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("configuration is invalid:\n  - {}", .0.join("\n  - "))]
    Many(Vec<String>),
}

const SECTIONS: [&str; 7] = [
    "server",
    "database",
    "auth",
    "transcoding",
    "metadata",
    "scanner",
    "subtitles",
];

/// Load configuration from a file, or from every `*.yaml`/`*.yml` file of a
/// directory (sorted by name, merged).
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let files = collect_files(path)?;
    if files.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{}: no *.yaml or *.yml file found",
            path.display()
        )));
    }
    let mut merged = Merged::default();
    for file in &files {
        let text = fs::read_to_string(file).map_err(|source| ConfigError::Io {
            path: file.clone(),
            source,
        })?;
        merged.add(file, &text)?;
    }
    let mut config = merged.finish()?;
    validate(&config)?;
    switch_off_keyless(&mut config);
    Ok(config)
}

/// Parse and validate a single YAML document.
pub fn from_str(text: &str) -> Result<Config, ConfigError> {
    let mut merged = Merged::default();
    merged.add(Path::new("<inline>"), text)?;
    let mut config = merged.finish()?;
    validate(&config)?;
    switch_off_keyless(&mut config);
    Ok(config)
}

fn collect_files(path: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let io = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let meta = fs::metadata(path).map_err(io)?;
    if !meta.is_dir() {
        return Ok(vec![path.to_owned()]);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(path).map_err(io)? {
        let entry = entry.map_err(io)?;
        let p = entry.path();
        let name = entry.file_name();
        // Skip hidden files, which includes Kubernetes ConfigMap `..data` links.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let is_yaml = p.extension().is_some_and(|e| e == "yaml" || e == "yml");
        if is_yaml && fs::metadata(&p).is_ok_and(|m| m.is_file()) {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

/// Parsed and interpolated documents, before typed deserialization.
#[derive(Default)]
struct Merged {
    sections: BTreeMap<&'static str, (String, Value)>,
    libraries: Vec<(String, String, Value)>,
    originals: HashMap<String, String>,
}

impl Merged {
    fn add(&mut self, file: &Path, text: &str) -> Result<(), ConfigError> {
        let file_name = file.display().to_string();
        let mut doc: Value = if text.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            // YAML 1.2 booleans only: `no`, `y` or `off` stay strings.
            let mut options = serde_saphyr::Options::default();
            options.strict_booleans = true;
            serde_saphyr::from_str_with_options(text, options).map_err(|e| ConfigError::Yaml {
                path: file.to_owned(),
                message: e.to_string(),
            })?
        };
        if doc.is_null() {
            doc = Value::Object(Map::new());
        }
        let Value::Object(top) = doc else {
            return Err(schema_err(&file_name, "<root>", "expected a mapping"));
        };
        for (key, mut value) in top {
            // Paths of interpolated values are prefixed by the file name so
            // that the same path in two files does not collide.
            let mut path = format!("{file_name}#{key}");
            interpolate::interpolate_tree(&mut value, &mut path, &mut self.originals)?;
            if let Some(k) = SECTIONS.iter().find(|s| **s == key).copied() {
                if let Some((other, _)) = self.sections.get(k) {
                    return Err(ConfigError::Invalid(format!(
                        "section `{k}` is defined in both {other} and {file_name}"
                    )));
                }
                self.sections.insert(k, (file_name.clone(), value));
            } else if key == "libraries" {
                let list = match value {
                    Value::Array(list) => list,
                    Value::Null => Vec::new(),
                    _ => return Err(schema_err(&file_name, &key, "expected a list")),
                };
                for (i, v) in list.into_iter().enumerate() {
                    self.libraries.push((file_name.clone(), format!("libraries[{i}]"), v));
                }
            } else {
                return Err(schema_err(
                    &file_name,
                    &key,
                    "unknown top-level key (expected one of server, database, auth, transcoding, metadata, scanner, subtitles, libraries)",
                ));
            }
        }
        Ok(())
    }

    fn section<T: DeserializeOwned + Default>(&mut self, name: &'static str) -> Result<T, ConfigError> {
        match self.sections.remove(name) {
            Some((file, value)) => deserialize(value, &file, name, &self.originals),
            None => Ok(T::default()),
        }
    }

    fn finish(mut self) -> Result<Config, ConfigError> {
        let server: ServerConfig = self.section("server")?;
        let database = match self.sections.remove("database") {
            Some((file, value)) => deserialize(value, &file, "database", &self.originals)?,
            None => return Err(ConfigError::Invalid("missing `database` section".into())),
        };
        let auth: AuthConfig = self.section("auth")?;
        let transcoding: TranscodingConfig = self.section("transcoding")?;
        let metadata: MetadataConfig = self.section("metadata")?;
        let scanner: ScannerConfig = self.section("scanner")?;
        let subtitles: SubtitlesConfig = self.section("subtitles")?;
        let mut libraries = Vec::with_capacity(self.libraries.len());
        for (file, path, value) in std::mem::take(&mut self.libraries) {
            libraries.push(deserialize(value, &file, &path, &self.originals)?);
        }
        Ok(Config {
            server,
            database,
            auth,
            transcoding,
            metadata,
            scanner,
            subtitles,
            libraries,
        })
    }
}

fn schema_err(file: &str, path: &str, message: &str) -> ConfigError {
    ConfigError::Schema {
        file: file.to_owned(),
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

/// Deserialize `value`, which lives at `path` in `file`.
///
/// A value that was exactly one `${...}` reference is first offered as a
/// number or boolean; when the target type wants a string instead, the
/// original text is restored and deserialization is retried.
fn deserialize<T: DeserializeOwned>(
    mut value: Value,
    file: &str,
    path: &str,
    originals: &HashMap<String, String>,
) -> Result<T, ConfigError> {
    for _ in 0..256 {
        match serde_path_to_error::deserialize::<_, T>(&value) {
            Ok(v) => return Ok(v),
            Err(err) => {
                let segments: Vec<Segment> = err.path().iter().map(Segment::from).collect();
                let full = render_path(path, &segments);
                let key = format!("{file}#{full}");
                if let Some(original) = originals.get(&key)
                    && let Some(slot) = lookup_mut(&mut value, &segments)
                    && !slot.is_string()
                {
                    *slot = Value::String(original.clone());
                    continue;
                }
                return Err(ConfigError::Schema {
                    file: file.to_owned(),
                    path: full,
                    message: err.into_inner().to_string(),
                });
            }
        }
    }
    Err(schema_err(file, path, "too many type coercions"))
}

enum Segment {
    Index(usize),
    Key(String),
    Other,
}

impl From<&serde_path_to_error::Segment> for Segment {
    fn from(s: &serde_path_to_error::Segment) -> Self {
        match s {
            serde_path_to_error::Segment::Seq { index } => Segment::Index(*index),
            serde_path_to_error::Segment::Map { key } => Segment::Key(key.clone()),
            _ => Segment::Other,
        }
    }
}

fn render_path(prefix: &str, segments: &[Segment]) -> String {
    let mut out = prefix.to_owned();
    for s in segments {
        match s {
            Segment::Index(i) => out.push_str(&format!("[{i}]")),
            Segment::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Segment::Other => {}
        }
    }
    out
}

fn lookup_mut<'a>(value: &'a mut Value, segments: &[Segment]) -> Option<&'a mut Value> {
    let mut v = value;
    for s in segments {
        v = match s {
            Segment::Index(i) => v.get_mut(*i)?,
            Segment::Key(k) => v.get_mut(k.as_str())?,
            Segment::Other => return None,
        };
    }
    Some(v)
}

/// Identifiers: letters, digits, `_`, `-` and `.`, at most 64 characters.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A provider whose key is empty is off: one configuration can then name
/// every provider with `${VAR:-}` and let the environment pick. Runs after
/// validation, so settings that need a switched-off provider stay valid and
/// fall back at run time.
fn switch_off_keyless(c: &mut Config) {
    let m = &mut c.metadata;
    if m.tmdb.as_ref().is_some_and(|t| t.api_key.is_empty()) {
        m.tmdb = None;
    }
    if m.tvdb.as_ref().is_some_and(|t| t.api_key.is_empty()) {
        m.tvdb = None;
    }
    if let Some(t) = &mut m.tvdb
        && t.pin.as_deref() == Some("")
    {
        t.pin = None;
    }
    if m.omdb.as_ref().is_some_and(|o| o.api_key.is_empty()) {
        m.omdb = None;
    }
    if m.fanart.as_ref().is_some_and(|f| f.api_key.is_empty()) {
        m.fanart = None;
    }
    if let Some(f) = &mut m.fanart
        && f.client_key.as_deref() == Some("")
    {
        f.client_key = None;
    }
    if m.anidb.as_ref().is_some_and(|a| a.client.is_empty()) {
        m.anidb = None;
    }
    let s = &mut c.subtitles;
    if s.opensubtitles.as_ref().is_some_and(|o| o.api_key.is_empty()) {
        s.opensubtitles = None;
    }
}

/// Collect every semantic error, so that one run lists them all.
fn validate(c: &Config) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    match url::Url::parse(&c.server.public_url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.path() == "/" && u.query().is_none() => {}
        Ok(_) => errors
            .push("server.public_url: must be an http(s) origin without path, e.g. https://media.example.com".into()),
        Err(_) if c.server.public_url.is_empty() => errors.push("server.public_url is required".into()),
        Err(e) => errors.push(format!("server.public_url: {e}")),
    }
    if !(c.database.url.starts_with("postgres://")
        || c.database.url.starts_with("postgresql://")
        || c.database.url.starts_with("mysql://")
        || c.database.url.starts_with("mariadb://"))
    {
        errors.push("database.url: expected postgres://, mariadb:// or mysql:// (SQLite is not supported)".into());
    }
    match &c.auth.oidc {
        None => errors.push("auth.oidc is required: Ferrocine signs users in with OpenID Connect".into()),
        Some(o) => {
            match url::Url::parse(&o.issuer) {
                Ok(u) if u.scheme() == "https" => {}
                Ok(u) if u.scheme() == "http" && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => {}
                Ok(_) => errors.push("auth.oidc.issuer: must be https:// (http:// only on localhost)".into()),
                Err(e) => errors.push(format!("auth.oidc.issuer: {e}")),
            }
            if o.client_id.is_empty() {
                errors.push("auth.oidc.client_id is required".into());
            }
            if !o.scopes.iter().any(|s| s == "openid") {
                errors.push("auth.oidc.scopes must include `openid`".into());
            }
        }
    }
    let t = &c.transcoding;
    if t.max_sessions == 0 {
        errors.push("transcoding.max_sessions must be at least 1".into());
    }
    if !(2..=10).contains(&t.segment_seconds) {
        errors.push("transcoding.segment_seconds must be between 2 and 10".into());
    }
    if !(240..=4320).contains(&t.max_height) {
        errors.push("transcoding.max_height must be between 240 and 4320".into());
    }
    if !describe::x264_presets().any(|p| p == t.x264_preset) {
        errors.push(format!(
            "transcoding.x264_preset {:?}: expected one of {}",
            t.x264_preset,
            describe::x264_presets().collect::<Vec<_>>().join(", ")
        ));
    }
    if !c.metadata.directory.is_absolute() {
        errors.push("metadata.directory must be an absolute path".into());
    }
    let t = &c.metadata.trickplay;
    if t.interval.0 < std::time::Duration::from_secs(1) {
        errors.push("metadata.trickplay.interval must be at least 1s".into());
    }
    if !(80..=640).contains(&t.width) {
        errors.push("metadata.trickplay.width must be between 80 and 640".into());
    }
    if !(1..=100).contains(&c.metadata.jpeg_quality) {
        errors.push("metadata.jpeg_quality must be between 1 and 100".into());
    }
    if !(100..=2000).contains(&c.metadata.poster_width) {
        errors.push("metadata.poster_width must be between 100 and 2000".into());
    }
    for lang in &c.subtitles.languages {
        if crate::lang::iso1(lang).is_none() {
            errors.push(format!(
                "subtitles.languages: unknown language {lang:?} (use ISO 639-1 codes such as en, fr)"
            ));
        }
    }
    let original_titles = std::iter::once((
        "metadata.prefer_original_title".to_owned(),
        &c.metadata.prefer_original_title,
    ))
    .chain(c.libraries.iter().filter_map(|l| {
        l.prefer_original_title
            .as_ref()
            .map(|p| (format!("libraries[{}].prefer_original_title", l.id), p))
    }));
    for (key, list) in original_titles {
        for lang in list {
            if lang != "*" && crate::lang::iso1(lang).is_none() {
                errors.push(format!(
                    "{key}: unknown language {lang:?} (use ISO 639-1 codes such as ja, ko, or \"*\" for every language)"
                ));
            }
        }
    }
    if !c.subtitles.languages.is_empty() && c.subtitles.opensubtitles.is_none() {
        errors.push("subtitles.languages needs subtitles.opensubtitles".into());
    }
    if let Some(o) = &c.subtitles.opensubtitles
        && !o.api_key.is_empty()
        && (o.username.is_empty() || o.password.is_empty())
    {
        errors.push(
            "subtitles.opensubtitles: username and password are required (the API only lets signed-in users download)"
                .into(),
        );
    }
    if c.scanner.probe_concurrency == 0 {
        errors.push("scanner.probe_concurrency must be at least 1".into());
    }
    let mut ids = HashSet::new();
    let mut roots: Vec<(&str, &Path)> = Vec::new();
    for (i, l) in c.libraries.iter().enumerate() {
        if !valid_id(&l.id) {
            errors.push(format!(
                "libraries[{i}].id {:?}: use letters, digits, `_-.`, at most 64 characters",
                l.id
            ));
        }
        if !ids.insert(l.id.as_str()) {
            errors.push(format!("libraries[{i}]: duplicate id {:?}", l.id));
        }
        if l.episode_order != EpisodeOrder::Aired {
            if l.kind != LibraryKind::Shows {
                errors.push(format!(
                    "libraries.{}: episode_order applies to show libraries only",
                    l.id
                ));
            } else if c.metadata.tvdb.is_none() {
                errors.push(format!(
                    "libraries.{}: episode_order {} needs metadata.tvdb",
                    l.id,
                    l.episode_order.as_str()
                ));
            }
        }
        if l.paths.is_empty() {
            errors.push(format!("libraries.{}: at least one path is required", l.id));
        }
        for p in &l.paths {
            if !p.is_absolute() {
                errors.push(format!("libraries.{}: path {} must be absolute", l.id, p.display()));
            }
            for (other, q) in &roots {
                if p.starts_with(q) || q.starts_with(p) {
                    errors.push(format!(
                        "libraries.{}: path {} overlaps {} of library {other}",
                        l.id,
                        p.display(),
                        q.display()
                    ));
                }
            }
        }
        for p in &l.paths {
            roots.push((&l.id, p));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Many(errors))
    }
}

#[cfg(test)]
mod tests;
