//! Configuration reload (SIGHUP or `POST /api/v1/reload`).
//!
//! An invalid configuration is refused and the running one kept. Libraries,
//! sign-in rules, metadata and scanner settings apply at once; the server,
//! database and transcoding sections hold resources (the listener, the pool,
//! running ffmpeg processes and the detected acceleration) and keep their
//! running values until a restart, which the report says.

use std::sync::Arc;

use serde::Serialize;

use crate::api::{AppState, ScanRequest};
use crate::auth::oidc::Provider;
use crate::config::{self, Config, ServerConfig};
use crate::media::scanner::ScanOptions;

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Sections whose changes now apply.
    pub applied: Vec<&'static str>,
    /// Sections that changed but keep their running values until a restart.
    pub restart_required: Vec<&'static str>,
    /// Libraries scanned because they are new or their paths changed.
    pub scans_queued: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// What changes between `old` and `new`, and the configuration to run with:
/// `new`, except for the sections that need a restart.
pub fn plan(old: &Config, new: Config) -> (Config, Report) {
    let mut report = Report::default();
    let mut effective = new;
    // `metrics_token` is read per request: it applies at once.
    let server_rest = |s: &ServerConfig| ServerConfig {
        metrics_token: None,
        ..s.clone()
    };
    if server_rest(&effective.server) != server_rest(&old.server) {
        report.restart_required.push("server");
        effective.server = ServerConfig {
            metrics_token: effective.server.metrics_token.clone(),
            ..old.server.clone()
        };
    }
    if effective.server.metrics_token != old.server.metrics_token {
        report.applied.push("server.metrics_token");
    }
    if effective.database != old.database {
        report.restart_required.push("database");
        effective.database = old.database.clone();
    }
    if effective.transcoding != old.transcoding {
        report.restart_required.push("transcoding");
        effective.transcoding = old.transcoding.clone();
    }
    if effective.auth != old.auth {
        report.applied.push("auth");
    }
    // The directory holds files written by the running instance.
    if effective.metadata.directory != old.metadata.directory {
        report.restart_required.push("metadata.directory");
        effective.metadata.directory = old.metadata.directory.clone();
    }
    if effective.metadata != old.metadata {
        report.applied.push("metadata");
    }
    if effective.scanner != old.scanner {
        report.applied.push("scanner");
    }
    if effective.libraries != old.libraries {
        report.applied.push("libraries");
    }
    for l in &effective.libraries {
        let before = old.libraries.iter().find(|o| o.id == l.id);
        if before.is_none_or(|o| o.paths != l.paths || o.kind != l.kind) {
            report.scans_queued.push(l.id.clone());
        }
    }
    (effective, report)
}

/// Reload the configuration from disk and apply it.
pub async fn reload(state: &AppState) -> Result<Report, ReloadError> {
    let _serialized = state.reload_lock.lock().await;
    let path = state.config_path.clone();
    let new = tokio::task::spawn_blocking(move || config::load(&path))
        .await
        .map_err(anyhow::Error::from)?
        .map_err(|e| ReloadError::Invalid(e.to_string()))?;
    let old = state.config();
    let (effective, report) = plan(&old, new);
    if report.applied.is_empty() && report.restart_required.is_empty() {
        tracing::info!("configuration reloaded: no change");
        return Ok(report);
    }
    state.store.sync_libraries(&effective.libraries).await?;
    if effective.auth.oidc != old.auth.oidc {
        let provider = effective.auth.oidc.clone().map(|o| {
            let p = Arc::new(Provider::new(o, &effective.server.public_url, state.http.clone()));
            tokio::spawn(p.clone().refresh_loop(state.shutdown.child_token()));
            p
        });
        state.replace_oidc(provider);
    }
    let retitle: Vec<(String, Vec<String>)> = effective
        .libraries
        .iter()
        .filter(|l| {
            old.libraries.iter().find(|o| o.id == l.id).is_some_and(|o| {
                o.original_title_languages(&old.metadata) != l.original_title_languages(&effective.metadata)
            })
        })
        .map(|l| (l.id.clone(), l.original_title_languages(&effective.metadata).to_vec()))
        .collect();
    state.replace_config(Arc::new(effective));
    // Titles follow a changed `prefer_original_title` at once, without a scan.
    for (library, languages) in retitle {
        match state.store.retitle(&library, &languages).await {
            Ok(n) => tracing::info!(library = %library, items = n, "titles switched for prefer_original_title"),
            Err(e) => tracing::warn!(library = %library, error = %format!("{e:#}"), "cannot switch titles"),
        }
    }
    for id in &report.scans_queued {
        let _ = state.scan_tx.try_send(ScanRequest {
            library: id.clone(),
            opts: ScanOptions::default(),
        });
    }
    tracing::info!(
        applied = ?report.applied,
        restart_required = ?report.restart_required,
        scans = ?report.scans_queued,
        "configuration reloaded"
    );
    if !report.restart_required.is_empty() {
        tracing::warn!(sections = ?report.restart_required, "these changes apply after a restart");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
server: { public_url: "https://media.example.com" }
database: { url: "postgres://fc@db/fc" }
auth: { oidc: { issuer: "https://id.example.com", client_id: ferrocine } }
libraries:
  - { id: movies, name: Movies, kind: movies, paths: [/media/movies] }
  - { id: shows, name: Shows, kind: shows, paths: [/media/shows] }
"#;

    fn cfg(text: &str) -> Config {
        config::from_str(text).unwrap()
    }

    #[test]
    fn nothing_changed() {
        let (_, r) = plan(&cfg(BASE), cfg(BASE));
        assert_eq!(r, Report::default());
    }

    #[test]
    fn libraries_and_rules_apply_resources_wait() {
        let changed = BASE
            .replace("paths: [/media/shows]", "paths: [/media/tv], access: [grown-ups]")
            .replace("client_id: ferrocine", "client_id: ferrocine, admins: [admins]")
            .replace("https://media.example.com", "https://films.example.com")
            .replace(
                "libraries:",
                "transcoding: { max_sessions: 2 }\nserver_extra: x\nlibraries:",
            )
            .replace("server_extra: x\n", "")
            + "  - { id: kids, name: Kids, kind: movies, paths: [/media/kids] }\n";
        let old = cfg(BASE);
        let (effective, r) = plan(&old, cfg(&changed));
        assert_eq!(r.applied, ["auth", "libraries"]);
        assert_eq!(r.restart_required, ["server", "transcoding"]);
        assert_eq!(r.scans_queued, ["shows", "kids"]);
        // Running values are kept for what needs a restart.
        assert_eq!(effective.server.public_url, "https://media.example.com");
        assert_eq!(effective.transcoding.max_sessions, old.transcoding.max_sessions);
        assert_eq!(effective.libraries[1].access, ["grown-ups"]);
        assert_eq!(effective.auth.oidc.unwrap().admins, ["admins"]);
    }

    #[test]
    fn metrics_token_applies_at_once() {
        let changed = BASE.replace(
            "server: { public_url: \"https://media.example.com\" }",
            "server: { public_url: \"https://media.example.com\", metrics_token: s3cret }",
        );
        let (effective, r) = plan(&cfg(BASE), cfg(&changed));
        assert_eq!(r.applied, ["server.metrics_token"]);
        assert!(r.restart_required.is_empty());
        assert_eq!(effective.server.metrics_token.as_deref(), Some("s3cret"));
    }
}
