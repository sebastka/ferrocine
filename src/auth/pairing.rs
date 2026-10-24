//! Quick Connect: signing in a device that has no browser (a TV app).
//!
//! The device asks for a pairing and shows its code (and a QR code of the
//! approval page); a signed-in user enters or scans it and approves; the
//! device, polling meanwhile, then receives a token of its own, which it
//! sends as `Authorization: Bearer`. The polling answers follow the OAuth
//! device authorization grant (RFC 8628): `authorization_pending` until
//! approved, `expired_token` once gone. The device secret and token are
//! stored hashed; codes are short-lived and single use.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::CurrentDevice;
use super::oidc::random_token;
use crate::api::{AppState, error, internal, not_found};
use crate::store::{Device, NewPairing, Redeemed, User, now_ms, sha256_hex};

/// How long a code can be approved.
pub const PAIRING_TTL: Duration = Duration::from_secs(600);
/// How often the device should poll, in seconds.
const POLL_INTERVAL: u64 = 5;
/// Pairings open at once, server-wide (the request needs no sign-in).
const MAX_OPEN: i64 = 100;
/// Without 0/O, 1/I: easy to read from across a room and to type.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 8;

/// A fresh code (stored and compared without its dash).
fn new_code() -> String {
    let mut buf = [0u8; CODE_LEN];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf).expect("system random generator");
    // 256 is a multiple of 32: no bias.
    buf.iter().map(|b| char::from(ALPHABET[usize::from(b % 32)])).collect()
}

/// A code as typed: case, spaces and dashes ignored; `None` when it cannot
/// be one.
pub fn normalize_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// `K7TQM2XD` → `K7TQ-M2XD`.
fn display_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

/// A name or platform as given by the device: trimmed, without control
/// characters, `None` when empty or longer than `max`.
fn label(s: &str, max: usize) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)).then(|| s.to_owned())
}

#[derive(Deserialize)]
pub struct PairRequest {
    device_name: String,
    #[serde(default)]
    platform: Option<String>,
}

#[derive(Serialize)]
pub struct PairResponse {
    /// To show, e.g. `K7TQ-M2XD`.
    code: String,
    /// To poll with; never shown.
    secret: String,
    /// The approval page with the code filled in (for a QR code).
    approve_url: String,
    expires_in: u64,
    interval: u64,
}

/// `POST /api/v1/pair` (no sign-in): start pairing a device.
pub async fn start(State(state): State<Arc<AppState>>, Json(req): Json<PairRequest>) -> Response {
    let Some(name) = label(&req.device_name, 64) else {
        return error(
            StatusCode::BAD_REQUEST,
            "device_name must be 1 to 64 printable characters",
        );
    };
    let platform = match req.platform.as_deref().map(|p| label(p, 32)) {
        Some(None) => return error(StatusCode::BAD_REQUEST, "platform must be 1 to 32 printable characters"),
        Some(p) => p,
        None => None,
    };
    let now = now_ms();
    match state.store.open_pairings(now).await {
        Ok(n) if n >= MAX_OPEN => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pairings in progress; try again later",
            );
        }
        Ok(_) => {}
        Err(e) => return internal(&e),
    }
    let secret = random_token(32);
    let secret_hash = sha256_hex(secret.as_bytes());
    // 32^8 codes: a collision with an open one is all but impossible, but cheap to retry.
    for _ in 0..5 {
        let code = new_code();
        let pairing = NewPairing {
            secret_hash: &secret_hash,
            code: &code,
            device_name: &name,
            platform: platform.as_deref(),
            expires_at: now + PAIRING_TTL.as_millis() as i64,
        };
        match state.store.create_pairing(&pairing, now).await {
            Ok(true) => {
                tracing::info!(device = %name, platform = ?platform, "device pairing started");
                let base = super::jwt::trim_slash(&state.config().server.public_url).to_owned();
                return Json(PairResponse {
                    approve_url: format!("{base}/pair?code={code}"),
                    code: display_code(&code),
                    secret,
                    expires_in: PAIRING_TTL.as_secs(),
                    interval: POLL_INTERVAL,
                })
                .into_response();
            }
            Ok(false) => continue,
            Err(e) => return internal(&e),
        }
    }
    internal(&anyhow::anyhow!("no free pairing code"))
}

#[derive(Deserialize)]
pub struct TokenRequest {
    secret: String,
}

#[derive(Serialize)]
pub struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    device: Device,
    user: User,
}

/// An RFC 8628 error: 400 with `error`.
fn grant_error(code: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": code, "error_description": description})),
    )
        .into_response()
}

/// `POST /api/v1/pair/token` (no sign-in): the device polls for its token.
pub async fn token(State(state): State<Arc<AppState>>, Json(req): Json<TokenRequest>) -> Response {
    let now = now_ms();
    let token = random_token(32);
    let redeemed = state
        .store
        .redeem_pairing(&sha256_hex(req.secret.as_bytes()), &sha256_hex(token.as_bytes()), now)
        .await;
    match redeemed {
        Ok(Redeemed::Pending) => grant_error("authorization_pending", "the code has not been approved yet"),
        Ok(Redeemed::Gone) => grant_error("expired_token", "the code expired or was already used; start over"),
        Ok(Redeemed::Device { device, user_id }) => {
            let user = match state.store.device_by_token(&sha256_hex(token.as_bytes())).await {
                Ok(Some(row)) if row.user.id == user_id => row.user,
                Ok(_) => return grant_error("expired_token", "the device was signed out"),
                Err(e) => return internal(&e),
            };
            tracing::info!(device = %device.name, user = %user.username, "device paired");
            let mut resp = Json(TokenResponse {
                access_token: token,
                token_type: "Bearer",
                device,
                user,
            })
            .into_response();
            resp.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            resp
        }
        Err(e) => internal(&e),
    }
}

/// `GET /api/v1/pair/{code}`: the device awaiting approval under a code.
pub async fn show(State(state): State<Arc<AppState>>, Path(code): Path<String>) -> Response {
    let Some(code) = normalize_code(&code) else {
        return not_found();
    };
    match state.store.pairing(&code, now_ms()).await {
        Ok(Some(p)) => Json(json!({
            "code": display_code(&code),
            "device_name": p.device_name,
            "platform": p.platform,
            "expires_at": p.expires_at,
        }))
        .into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(&e),
    }
}

/// `POST /api/v1/pair/{code}/approve`: sign the device in as the user.
pub async fn approve(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    device: Option<Extension<CurrentDevice>>,
    Path(code): Path<String>,
) -> Response {
    // A device token cannot mint more of itself.
    if device.is_some() {
        return error(StatusCode::FORBIDDEN, "approve from a signed-in browser");
    }
    let Some(code) = normalize_code(&code) else {
        return not_found();
    };
    match state.store.approve_pairing(&code, user.id, now_ms()).await {
        Ok(true) => {
            tracing::info!(user = %user.username, "device pairing approved");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => not_found(),
        Err(e) => internal(&e),
    }
}

#[derive(Serialize)]
pub struct DeviceOut {
    #[serde(flatten)]
    device: Device,
    /// The device making this request.
    current: bool,
}

/// `GET /api/v1/me/devices`: the user's paired devices.
pub async fn devices(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    current: Option<Extension<CurrentDevice>>,
) -> Response {
    match state.store.devices(user.id).await {
        Ok(list) => Json(
            list.into_iter()
                .map(|d| DeviceOut {
                    current: current.as_ref().is_some_and(|c| c.0.0 == d.id),
                    device: d,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(&e),
    }
}

/// `DELETE /api/v1/me/devices/{id}`: sign a device out (a device may sign
/// itself out).
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    Extension(user): Extension<User>,
    Path(id): Path<i64>,
) -> Response {
    match state.store.delete_device(user.id, id).await {
        Ok(Some(hash)) => {
            state.sessions.forget(&hash);
            tracing::info!(user = %user.username, device = id, "device signed out");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(None) => not_found(),
        Err(e) => internal(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = new_code();
        assert_eq!(c.len(), CODE_LEN);
        assert_eq!(normalize_code(&c).as_deref(), Some(c.as_str()));
        assert_eq!(normalize_code(" k7tq-m2xd ").as_deref(), Some("K7TQM2XD"));
        assert_eq!(display_code("K7TQM2XD"), "K7TQ-M2XD");
        // 0, O, 1 and I are never drawn, nor accepted.
        for bad in ["K7TQ-M2X0", "K7TQ-M2XO", "K7TQ-M2X", "K7TQ-M2XDD", "../../x"] {
            assert_eq!(normalize_code(bad), None, "{bad}");
        }
    }

    #[test]
    fn labels() {
        assert_eq!(label("  Living room TV ", 64).as_deref(), Some("Living room TV"));
        assert_eq!(label("   ", 64), None);
        assert_eq!(label("a\nb", 64), None);
        assert_eq!(label(&"x".repeat(65), 64), None);
    }
}
