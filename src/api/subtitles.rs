//! On-demand subtitles: search OpenSubtitles for an item, download one,
//! remove a downloaded one.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use super::items::visible_item;
use super::{AppState, error, internal, not_found};
use crate::media::ItemKind;
use crate::media::probe::SubtitleStream;
use crate::store::{NewSubtitle, StoredSubtitle, User, now_ms};
use crate::subtitles::opensubtitles::QuotaExceeded;
use crate::subtitles::{Candidate, OpenSubtitles, STORED_BASE, hash_file, query_for};

/// A stored subtitle as a stream of the item's media.
pub fn stream_of(s: &StoredSubtitle) -> SubtitleStream {
    SubtitleStream {
        index: STORED_BASE + s.id as i32,
        codec: "webvtt".into(),
        language: Some(s.language.clone()),
        title: Some(s.release_name.clone().unwrap_or_else(|| "OpenSubtitles".into())),
        default: false,
        forced: s.forced,
        external: true,
        text: true,
        path: None,
        source: Some(s.source.clone()),
    }
}

fn client(state: &AppState) -> Option<OpenSubtitles> {
    let cfg = state.config().subtitles.opensubtitles.clone()?;
    Some(OpenSubtitles::new(cfg, state.http.clone(), state.providers.clone()))
}

#[derive(Deserialize)]
pub struct SearchParams {
    /// Comma-separated ISO 639-1 codes; default: the configured languages, else `en`.
    language: Option<String>,
}

#[derive(Serialize)]
struct SearchResults {
    results: Vec<Candidate>,
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
    Query(p): Query<SearchParams>,
) -> Response {
    let Some(os) = client(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no subtitle provider is configured");
    };
    let row = match visible_item(&state, &user, id).await {
        Ok(r) if r.kind.playable() => r,
        Ok(_) => return error(StatusCode::BAD_REQUEST, "only movies and episodes have subtitles"),
        Err(resp) => return resp,
    };
    let languages: Vec<String> = match p.language.as_deref().filter(|l| !l.trim().is_empty()) {
        Some(list) => {
            let mut out = Vec::new();
            for l in list.split(',') {
                match crate::lang::iso1(l) {
                    Some(code) => out.push(code.to_owned()),
                    None => return error(StatusCode::BAD_REQUEST, &format!("language: unknown code {l:?}")),
                }
            }
            out
        }
        None => {
            let configured = state.config().subtitles.languages.clone();
            if configured.is_empty() {
                vec!["en".into()]
            } else {
                configured
            }
        }
    };
    let series = match (row.kind, row.series_id) {
        (ItemKind::Episode, Some(sid)) => match state.store.item_row(sid).await {
            Ok(s) => s,
            Err(e) => return internal(&e),
        },
        _ => None,
    };
    // A DVD part's hash is not the movie's.
    let hash = match row
        .path
        .clone()
        .filter(|p| !crate::media::disc::is_dvd(std::path::Path::new(p)))
    {
        Some(path) => tokio::task::spawn_blocking(move || hash_file(std::path::Path::new(&path)).ok())
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let q = query_for(&row, series.as_ref(), hash, &languages);
    match os.search(&q).await {
        Ok(mut results) => {
            results.sort_by_key(|c| {
                (
                    !c.hash_match,
                    c.machine_translated || c.ai_translated,
                    -(c.downloads as i64),
                )
            });
            Json(SearchResults { results }).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "OpenSubtitles search failed");
            error(StatusCode::BAD_GATEWAY, "the subtitle provider did not answer")
        }
    }
}

/// What the client sends back from a search result.
#[derive(Deserialize)]
pub struct DownloadRequest {
    file_id: i64,
    language: String,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    hearing_impaired: bool,
    #[serde(default)]
    foreign_parts_only: bool,
    #[serde(default)]
    hash_match: bool,
}

pub async fn download(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
    Json(req): Json<DownloadRequest>,
) -> Response {
    let Some(os) = client(&state) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no subtitle provider is configured");
    };
    match visible_item(&state, &user, id).await {
        Ok(r) if r.kind.playable() => {}
        Ok(_) => return error(StatusCode::BAD_REQUEST, "only movies and episodes have subtitles"),
        Err(resp) => return resp,
    }
    let Some(language) = crate::lang::iso1(&req.language) else {
        return error(StatusCode::BAD_REQUEST, "language: unknown code");
    };
    let d = match os.download(req.file_id).await {
        Ok(d) => d,
        Err(e) if e.is::<QuotaExceeded>() => return error(StatusCode::TOO_MANY_REQUESTS, &e.to_string()),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "OpenSubtitles download failed");
            return error(StatusCode::BAD_GATEWAY, "the subtitle provider did not answer");
        }
    };
    let stored = state
        .store
        .add_subtitle(
            &NewSubtitle {
                item_id: id,
                language,
                source: "opensubtitles",
                source_id: &req.file_id.to_string(),
                release_name: req.release.as_deref(),
                hearing_impaired: req.hearing_impaired,
                forced: req.foreign_parts_only,
                hash_match: req.hash_match,
                content: &d.vtt,
                created_by: &user.username,
            },
            now_ms(),
        )
        .await;
    match stored {
        Ok(s) => {
            tracing::info!(user = %user.username, item = id, language, remaining = ?d.remaining, "subtitle downloaded");
            (StatusCode::CREATED, Json(stream_of(&s))).into_response()
        }
        Err(e) => internal(&e),
    }
}

/// Remove a downloaded subtitle (administrators).
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path((id, index)): Path<(i64, String)>,
) -> Response {
    if !user.admin {
        return error(StatusCode::FORBIDDEN, "administrators only");
    }
    let Ok(index) = index.trim_end_matches(".vtt").parse::<i32>() else {
        return not_found();
    };
    if index < STORED_BASE {
        return error(StatusCode::BAD_REQUEST, "only downloaded subtitles can be removed");
    }
    if let Err(resp) = visible_item(&state, &user, id).await {
        return resp;
    }
    match state.store.delete_subtitle(id, i64::from(index - STORED_BASE)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(&e),
    }
}
