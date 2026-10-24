//! HTTP: the JSON API under `/api/v1`, sign-in under `/auth`, probes,
//! metrics, and optionally the static UI.

mod admin;
mod items;
mod playback;
pub mod subtitles;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::oidc::Provider;
use crate::auth::{self, Sessions};
use crate::config::{Config, LibraryConfig};
use crate::media::scanner::{ScanOptions, Scanner};
use crate::metrics::Metrics;
use crate::store::{Store, User};
use crate::transcode::Transcoder;
use crate::transcode::subtitles::Subtitles;

/// A scan asked for through the API.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub library: String,
    pub opts: ScanOptions,
}

pub struct AppState {
    /// Where the configuration is read from, for reloads.
    pub config_path: PathBuf,
    /// The running configuration; replaced by reloads.
    config: RwLock<Arc<Config>>,
    pub store: Store,
    pub sessions: Sessions,
    oidc: RwLock<Option<Arc<Provider>>>,
    pub http: reqwest::Client,
    pub providers: Arc<crate::metadata::ProviderState>,
    pub shutdown: CancellationToken,
    pub reload_lock: tokio::sync::Mutex<()>,
    pub scanner: Scanner,
    pub scan_tx: mpsc::Sender<ScanRequest>,
    pub transcoder: Arc<Transcoder>,
    pub subtitles: Subtitles,
    pub metrics: Metrics,
    /// The metadata directory: artwork, subtitles, previews.
    pub assets: Arc<crate::assets::Assets>,
    /// Wakes the seek bar preview pass (after a scan).
    pub trickplay_wake: tokio::sync::Notify,
}

/// What [`AppState`] is built from.
pub struct Parts {
    pub config_path: PathBuf,
    pub config: Arc<Config>,
    pub store: Store,
    pub oidc: Option<Arc<Provider>>,
    pub http: reqwest::Client,
    pub providers: Arc<crate::metadata::ProviderState>,
    pub shutdown: CancellationToken,
    pub scanner: Scanner,
    pub scan_tx: mpsc::Sender<ScanRequest>,
    pub transcoder: Arc<Transcoder>,
    pub metrics: Metrics,
    pub assets: Arc<crate::assets::Assets>,
}

impl AppState {
    pub fn new(p: Parts) -> Self {
        Self {
            sessions: Sessions::new(&p.config.server.public_url),
            subtitles: Subtitles::new(p.config.transcoding.ffmpeg.clone()),
            config_path: p.config_path,
            config: RwLock::new(p.config),
            store: p.store,
            oidc: RwLock::new(p.oidc),
            http: p.http,
            providers: p.providers,
            shutdown: p.shutdown,
            reload_lock: tokio::sync::Mutex::new(()),
            scanner: p.scanner,
            scan_tx: p.scan_tx,
            transcoder: p.transcoder,
            metrics: p.metrics,
            assets: p.assets,
            trickplay_wake: tokio::sync::Notify::new(),
        }
    }

    /// The running configuration.
    pub fn config(&self) -> Arc<Config> {
        match self.config.read() {
            Ok(c) => c.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn replace_config(&self, config: Arc<Config>) {
        match self.config.write() {
            Ok(mut c) => *c = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
    }

    /// The OpenID Connect provider, if configured.
    pub fn oidc(&self) -> Option<Arc<Provider>> {
        match self.oidc.read() {
            Ok(o) => o.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Swap the provider; the previous one stops refreshing its keys.
    pub fn replace_oidc(&self, provider: Option<Arc<Provider>>) {
        let old = match self.oidc.write() {
            Ok(mut o) => std::mem::replace(&mut *o, provider),
            Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), provider),
        };
        if let Some(old) = old {
            old.stop();
        }
    }

    /// Libraries `user` may see: every one for admins, else those without
    /// `access` or sharing a group with the user.
    pub fn visible(&self, user: &User) -> Vec<LibraryConfig> {
        self.config()
            .libraries
            .iter()
            .filter(|l| user.admin || l.access.is_empty() || l.access.iter().any(|g| user.groups.contains(g)))
            .cloned()
            .collect()
    }

    pub fn can_see(&self, user: &User, library: &str) -> bool {
        self.visible(user).iter().any(|l| l.id == library)
    }

    pub fn library(&self, id: &str) -> Option<LibraryConfig> {
        self.config().libraries.iter().find(|l| l.id == id).cloned()
    }
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

/// Log an internal error and answer 500 without details.
pub fn internal(e: &anyhow::Error) -> Response {
    tracing::error!(error = %format!("{e:#}"), "request failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

pub fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/me", get(items::me))
        .route("/me/avatar", get(items::avatar))
        .route("/home", get(items::home))
        .route("/search", get(items::search))
        .route("/libraries", get(items::libraries))
        .route("/libraries/{id}/items", get(items::library_items))
        .route("/libraries/{id}/genres", get(items::genres))
        .route("/libraries/{id}/scan", post(admin::scan))
        .route("/items/{id}", get(items::item))
        .route("/items/{id}/children", get(items::children))
        .route("/items/{id}/images/{kind}", get(items::image))
        .route("/items/{id}/trickplay/{tag}/{sheet}", get(items::trickplay_sheet))
        .route("/items/{id}/progress", post(items::progress))
        .route("/items/{id}/played", post(items::played).delete(items::unplayed))
        .route("/items/{id}/favorite", post(items::favorite).delete(items::unfavorite))
        .route("/items/{id}/refresh", post(admin::refresh))
        .route("/items/{id}/playback", post(playback::playback))
        .route("/items/{id}/stream", get(playback::stream))
        .route("/items/{id}/subtitles/search", get(subtitles::search))
        .route("/items/{id}/subtitles", post(subtitles::download))
        .route(
            "/items/{id}/subtitles/{index}",
            get(playback::subtitles).delete(subtitles::remove),
        )
        .route("/hls/{session}/{file}", get(playback::hls_file))
        .route("/hls/{session}", delete(playback::stop))
        .route("/system", get(admin::system))
        .route("/reload", post(admin::reload))
        .route("/config", get(admin::config))
        .fallback(|| async { not_found() })
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_user));

    let mut app = Router::new()
        .nest("/api/v1", api)
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics));
    app = match &state.config().server.web_dir {
        Some(dir) => {
            let index = ServeFile::new(dir.join("index.html"));
            app.fallback_service(ServeDir::new(dir).fallback(index))
                .layer(SetResponseHeaderLayer::if_not_present(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static(
                        "default-src 'self'; img-src 'self' data:; media-src 'self' blob:; worker-src 'self' blob:; \
                     style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
                    ),
                ))
        }
        None => app.fallback(|| async { not_found() }),
    };
    app.layer(SetResponseHeaderLayer::if_not_present(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    ))
    .with_state(state)
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    match state.store.ping().await {
        Ok(()) => "ready".into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "database unreachable");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
        }
    }
}

async fn metrics(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(token) = &state.config().server.metrics_token {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| auth::jwt::constant_time_eq(g.as_bytes(), token.as_bytes())) {
            return error(StatusCode::UNAUTHORIZED, "metrics token required");
        }
    }
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}
