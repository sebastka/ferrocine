//! Playback: the decision, direct streams, HLS sessions and subtitles.

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::items::visible_item;
use super::{AppState, error, internal, not_found};
use crate::store::{ItemRow, User};
use crate::transcode::plan::{self, ClientCaps, Method};
use crate::transcode::{StartError, StartRequest, hw::HwKind};

#[derive(Deserialize)]
pub struct PlaybackRequest {
    #[serde(default)]
    start_ms: i64,
    audio_index: Option<i32>,
    max_height: Option<u32>,
    max_bitrate: Option<u64>,
    #[serde(default)]
    force_transcode: bool,
    #[serde(default)]
    capabilities: ClientCaps,
    /// A bitmap subtitle stream (`text: false`) to burn into the picture.
    burn_subtitle: Option<i32>,
}

#[derive(Serialize)]
struct PlaybackResponse {
    method: Method,
    url: String,
    session_id: Option<String>,
    start_ms: i64,
    duration_ms: Option<i64>,
    hw_accel: Option<HwKind>,
    /// HDR video tone-mapped to SDR.
    tone_mapping: bool,
    /// The bitmap subtitle stream burned into the picture.
    burned_subtitle: Option<i32>,
    reasons: Vec<String>,
}

/// The file of a playable item, if it is still there.
async fn media_file(row: &ItemRow) -> Result<std::path::PathBuf, Response> {
    let Some(path) = row.path.as_deref().filter(|_| row.kind.playable()) else {
        return Err(error(StatusCode::BAD_REQUEST, "this item has no media file"));
    };
    let path = std::path::PathBuf::from(path);
    // A disc title names the disc (folder or image) and a title of it.
    let on_disk = crate::media::disc::locate(&path).map_or_else(|| path.clone(), |(s, _)| s.path().to_path_buf());
    if tokio::fs::metadata(&on_disk).await.is_err() {
        tracing::warn!(file = %path.display(), "media file unavailable");
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "the media file is unavailable"));
    }
    Ok(path)
}

pub async fn playback(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
    Json(req): Json<PlaybackRequest>,
) -> Response {
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let file = match media_file(&row).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let Some(media) = &row.media else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "this item has not been probed yet");
    };
    if let Some(a) = req.audio_index
        && !media.audio.iter().any(|s| s.index == a)
    {
        return error(StatusCode::BAD_REQUEST, "audio_index: no such audio stream");
    }
    if let Some(i) = req.burn_subtitle
        && !media.subtitle(i).is_some_and(|s| !s.text && !s.external)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "burn_subtitle: no such bitmap subtitle stream (text subtitles are sent as WebVTT)",
        );
    }
    let decision = plan::decide(
        media,
        &plan::Request {
            caps: req.capabilities,
            audio_index: req.audio_index,
            max_height: req.max_height,
            max_bitrate: req.max_bitrate,
            force_transcode: req.force_transcode,
            burn_subtitle: req.burn_subtitle,
        },
        plan::Limits {
            max_height: state.config().transcoding.max_height,
            tone_mapping: state.config().transcoding.tone_mapping && state.transcoder.ffmpeg.tone_mapper.is_some(),
        },
    );
    let duration = media.duration_ms;
    state.metrics.playback(decision.method.as_str());
    if decision.method == Method::Direct {
        return Json(PlaybackResponse {
            method: Method::Direct,
            url: format!("/api/v1/items/{id}/stream"),
            session_id: None,
            start_ms: 0,
            duration_ms: duration,
            hw_accel: None,
            tone_mapping: false,
            burned_subtitle: None,
            reasons: decision.reasons,
        })
        .into_response();
    }
    // Never start at (or past) the very end.
    let start_ms = match duration {
        Some(d) => req.start_ms.clamp(0, (d - 5_000).max(0)),
        None => req.start_ms.max(0),
    };
    let started = state
        .transcoder
        .start(StartRequest {
            plan: &decision,
            file: &file,
            // A DVD's stream order varies along a title: streams go by id.
            stream_ids: if crate::media::disc::is_dvd(&file) {
                media.stream_ids()
            } else {
                Default::default()
            },
            start_ms,
            user_id: user.id,
            username: &user.username,
            item_id: id,
            title: &row.meta.title,
        })
        .await;
    match started {
        Ok(session) => Json(PlaybackResponse {
            method: decision.method,
            url: format!("/api/v1/hls/{}/index.m3u8", session.id),
            session_id: Some(session.id.clone()),
            start_ms,
            duration_ms: duration,
            hw_accel: session.hw_accel,
            tone_mapping: decision.tone_mapped(),
            burned_subtitle: decision.burned(),
            reasons: decision.reasons,
        })
        .into_response(),
        Err(StartError::Busy) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many playback sessions; try again later",
        ),
        Err(StartError::Failed(e)) => {
            tracing::error!(item = id, error = %format!("{e:#}"), "cannot start playback");
            error(StatusCode::INTERNAL_SERVER_ERROR, "the stream could not be started")
        }
    }
}

/// Serve a file with range and conditional request support.
async fn serve(path: &std::path::Path, req: Request, cache: &'static str) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(resp) => {
            let mut resp = resp.into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            resp
        }
        Err(e) => match e {},
    }
}

pub async fn stream(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
    req: Request,
) -> Response {
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match media_file(&row).await {
        // The file is one part of the movie: DVDs play through a transcode.
        Ok(f) if crate::media::disc::is_dvd(&f) => error(StatusCode::CONFLICT, "a DVD plays through a transcode"),
        Ok(f) => serve(&f, req, "private, no-transform").await,
        Err(resp) => resp,
    }
}

pub async fn hls_file(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path((session, file)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(path) = state.transcoder.file(&session, user.id, &file) else {
        return not_found();
    };
    if file == "index.m3u8" {
        // The playlist grows: never cache it.
        return serve(&path, req, "no-store").await;
    }
    serve(&path, req, "private, max-age=3600").await
}

pub async fn stop(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(session): Path<String>,
) -> Response {
    if state.transcoder.stop(&session, Some(user.id)).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

#[derive(Deserialize)]
pub struct SubtitleParams {
    #[serde(default)]
    start_ms: i64,
}

pub async fn subtitles(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path((id, index)): Path<(i64, String)>,
    Query(p): Query<SubtitleParams>,
) -> Response {
    // Accept `3` and `3.vtt`.
    let Ok(index) = index.trim_end_matches(".vtt").parse::<i32>() else {
        return not_found();
    };
    let row = match visible_item(&state, &user, id).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // Downloaded subtitles are stored as WebVTT.
    if index >= crate::subtitles::STORED_BASE {
        return match state
            .store
            .subtitle_content(id, i64::from(index - crate::subtitles::STORED_BASE))
            .await
        {
            Ok(Some(vtt)) => vtt_response(crate::transcode::subtitles::shift(&vtt, p.start_ms)),
            Ok(None) => not_found(),
            Err(e) => internal(&e),
        };
    }
    let file = match media_file(&row).await {
        Ok(f) => f,
        Err(resp) => return resp,
    };
    let Some(track) = row.media.as_ref().and_then(|m| m.subtitle(index)) else {
        return not_found();
    };
    if !track.text {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bitmap subtitles cannot be converted to WebVTT",
        );
    }
    let external = track.path.as_deref().map(std::path::Path::new);
    let key = (id, index, row.file_mtime.unwrap_or(0));
    match state.subtitles.webvtt(key, &file, index, external).await {
        Ok(vtt) => vtt_response(crate::transcode::subtitles::shift(&vtt, p.start_ms)),
        Err(e) => internal(&e),
    }
}

fn vtt_response(vtt: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        vtt,
    )
        .into_response()
}
