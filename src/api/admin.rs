//! Administration: system information, scans and metadata refreshes.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::items::{ScanOut, visible_item};
use super::{AppState, ScanRequest, error, internal, not_found};
use crate::media::ItemKind;
use crate::media::scanner::ScanOptions;
use crate::store::User;
use crate::transcode::SessionInfo;
use crate::transcode::hw::FfmpegInfo;

/// The refusal for non-administrators.
fn deny_unless_admin(user: &User) -> Option<Response> {
    (!user.admin).then(|| error(StatusCode::FORBIDDEN, "administrators only"))
}

#[derive(Serialize)]
struct MetadataOut {
    tmdb: bool,
}

#[derive(Serialize)]
struct LibraryScan {
    id: String,
    scan: ScanOut,
}

#[derive(Serialize)]
struct System<'a> {
    version: &'static str,
    ffmpeg: &'a FfmpegInfo,
    metadata: MetadataOut,
    transcodes: Vec<SessionInfo>,
    libraries: Vec<LibraryScan>,
}

pub async fn system(State(state): State<Arc<AppState>>, Extension(user): Extension<User>) -> Response {
    if let Some(r) = deny_unless_admin(&user) {
        return r;
    }
    let scans = match state.store.scan_states().await {
        Ok(s) => s,
        Err(e) => return internal(&e),
    };
    let libraries = state
        .config()
        .libraries
        .iter()
        .map(|l| LibraryScan {
            id: l.id.clone(),
            scan: ScanOut {
                running: state.scanner.is_running(&l.id),
                state: scans
                    .iter()
                    .find(|(id, _)| *id == l.id)
                    .map(|(_, s)| s.clone())
                    .unwrap_or_default(),
            },
        })
        .collect();
    Json(System {
        version: env!("CARGO_PKG_VERSION"),
        ffmpeg: &state.transcoder.ffmpeg,
        metadata: MetadataOut {
            tmdb: state.config().metadata.tmdb.is_some(),
        },
        transcodes: state.transcoder.list(),
        libraries,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct ScanParams {
    #[serde(default)]
    refresh_metadata: bool,
}

async fn request_scan(state: &AppState, library: &str, opts: ScanOptions) -> Response {
    let req = ScanRequest {
        library: library.to_owned(),
        opts,
    };
    match state.scan_tx.try_send(req) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many scans queued; try again later",
        ),
    }
}

pub async fn scan(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<String>,
    Query(p): Query<ScanParams>,
) -> Response {
    if let Some(r) = deny_unless_admin(&user) {
        return r;
    }
    if state.library(&id).is_none() {
        return not_found();
    }
    tracing::info!(user = %user.username, library = %id, refresh = p.refresh_metadata, "scan requested");
    request_scan(
        &state,
        &id,
        ScanOptions {
            refresh_metadata: p.refresh_metadata,
        },
    )
    .await
}

/// The running configuration, described, without secrets.
pub async fn config(State(state): State<Arc<AppState>>, Extension(user): Extension<User>) -> Response {
    if let Some(r) = deny_unless_admin(&user) {
        return r;
    }
    Json(crate::config::describe::describe(&state.config())).into_response()
}

/// Reload the configuration files.
pub async fn reload(State(state): State<Arc<AppState>>, Extension(user): Extension<User>) -> Response {
    if let Some(r) = deny_unless_admin(&user) {
        return r;
    }
    tracing::info!(user = %user.username, "configuration reload requested");
    match crate::reload::reload(&state).await {
        Ok(report) => Json(report).into_response(),
        Err(crate::reload::ReloadError::Invalid(msg)) => {
            tracing::warn!(error = %msg, "invalid configuration refused; the running one is kept");
            error(StatusCode::UNPROCESSABLE_ENTITY, &msg)
        }
        Err(crate::reload::ReloadError::Failed(e)) => internal(&e),
    }
}

/// Forget an item's metadata state (and its descendants'), then rescan its
/// library: the next pass rebuilds it from the file name, `.nfo` and provider.
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = deny_unless_admin(&user) {
        return r;
    }
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let mut ids = vec![id];
    if matches!(row.kind, ItemKind::Series | ItemKind::Season) {
        match state.store.episode_ids_under(id).await {
            Ok(e) => ids.extend(e),
            Err(e) => return internal(&e),
        }
        match state.store.child_rows(id).await {
            Ok(c) => ids.extend(c.iter().map(|c| c.id)),
            Err(e) => return internal(&e),
        }
    }
    if let Err(e) = state.store.reset_remote(&ids).await {
        return internal(&e);
    }
    tracing::info!(user = %user.username, item = id, items = ids.len(), "metadata refresh requested");
    request_scan(&state, &row.library_id, ScanOptions::default()).await
}
