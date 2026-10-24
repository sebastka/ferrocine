//! Sign-in with OpenID Connect (authorization code flow with PKCE), browser
//! sessions, and devices paired with Quick Connect.
//!
//! Ferrocine is the relying party: the browser never sees a token. The
//! session cookie holds a random value; the database keeps only its SHA-256,
//! so a database leak does not hand out sessions. Devices (TV apps) send a
//! token of their own as `Authorization: Bearer`, stored the same way. Both
//! are cached in memory for a minute to spare the database on segment
//! requests.

pub mod avatar;
pub mod jwt;
pub mod oidc;
pub mod pairing;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::Value;

use crate::api::{AppState, error};
use crate::config::OidcConfig;
use crate::store::{Identity, LoginRequest, NewSession, User, now_ms, sha256_hex};
use oidc::random_token;

const SESSION_COOKIE: &str = "ferrocine_session";
const LOGIN_COOKIE: &str = "ferrocine_login";
/// Sign-ins must complete within this time.
pub const LOGIN_TTL: Duration = Duration::from_secs(600);
const CACHE_TTL: Duration = Duration::from_secs(60);
/// `last_seen_at` is written at most this often per session.
const TOUCH_EVERY_MS: i64 = 5 * 60_000;

#[derive(Clone)]
struct Cached {
    user: User,
    /// Set for a device token.
    device: Option<i64>,
    expires_at: i64,
    last_seen_at: i64,
    at: Instant,
}

/// The device a request came from, when signed with a device token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentDevice(pub i64);

/// Resolves session cookies and device tokens to users.
pub struct Sessions {
    cache: Mutex<HashMap<String, Cached>>,
    /// `https` public URL: cookies are `Secure` and `__Host-` prefixed.
    secure: bool,
}

impl Sessions {
    pub fn new(public_url: &str) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            secure: public_url.starts_with("https://"),
        }
    }

    fn name(&self, base: &str) -> String {
        if self.secure {
            format!("__Host-{base}")
        } else {
            base.to_owned()
        }
    }

    fn cookie(&self, base: &str, value: &str, max_age: i64) -> HeaderValue {
        let secure = if self.secure { "; Secure" } else { "" };
        let c = format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
            self.name(base)
        );
        HeaderValue::from_str(&c).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    pub fn forget(&self, hash: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.remove(hash);
        }
    }

    /// Drop stale cache entries.
    pub fn sweep(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.retain(|_, v| v.at.elapsed() < CACHE_TTL);
        }
    }
}

/// The value of cookie `name`.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// The token of `Authorization: Bearer`.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Look up the device token, else the session, of a request.
pub async fn current_user(state: &AppState, headers: &HeaderMap) -> Option<(User, Option<CurrentDevice>)> {
    let (value, device) = match bearer(headers) {
        Some(token) => (token, true),
        None => (cookie_value(headers, &state.sessions.name(SESSION_COOKIE))?, false),
    };
    let hash = sha256_hex(value.as_bytes());
    let now = now_ms();
    let auth = &state.config().auth;
    let idle = if device { auth.device_idle() } else { auth.idle() }.as_millis_i64();
    let cached = state
        .sessions
        .cache
        .lock()
        .ok()
        .and_then(|c| c.get(&hash).cloned())
        .filter(|c| c.at.elapsed() < CACHE_TTL);
    let entry = match cached {
        Some(c) => c,
        None => {
            let c = if device {
                let row = match state.store.device_by_token(&hash).await {
                    Ok(r) => r?,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot read device");
                        return None;
                    }
                };
                Cached {
                    user: row.user,
                    device: Some(row.device_id),
                    expires_at: i64::MAX,
                    last_seen_at: row.last_seen_at,
                    at: Instant::now(),
                }
            } else {
                let row = match state.store.session(&hash).await {
                    Ok(r) => r?,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot read session");
                        return None;
                    }
                };
                Cached {
                    user: row.user,
                    device: None,
                    expires_at: row.expires_at,
                    last_seen_at: row.last_seen_at,
                    at: Instant::now(),
                }
            };
            if let Ok(mut cache) = state.sessions.cache.lock() {
                cache.insert(hash.clone(), c.clone());
            }
            c
        }
    };
    if now > entry.expires_at || now - entry.last_seen_at > idle {
        state.sessions.forget(&hash);
        return None;
    }
    if now - entry.last_seen_at > TOUCH_EVERY_MS {
        if let Ok(mut cache) = state.sessions.cache.lock()
            && let Some(c) = cache.get_mut(&hash)
        {
            c.last_seen_at = now;
        }
        let store = state.store.clone();
        let device = entry.device;
        tokio::spawn(async move {
            let touched = match device {
                Some(id) => store.touch_device(id, now).await,
                None => store.touch_session(&hash, now).await,
            };
            if let Err(e) = touched {
                tracing::debug!(error = %e, "cannot touch session");
            }
        });
    }
    Some((entry.user, entry.device.map(CurrentDevice)))
}

/// Same-origin check for state-changing requests (CSRF): Fetch Metadata
/// when the browser sends it, else the `Origin` header.
pub fn same_origin(headers: &HeaderMap, public_url: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return site == "same-origin" || site == "none";
    }
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| jwt::trim_slash(o) == jwt::trim_slash(public_url))
}

/// Middleware of `/api/v1`: a valid session and same-origin unsafe
/// requests, or a device token (which no browser sends on its own, so
/// needs no CSRF check).
pub async fn require_user(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let unsafe_method = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if unsafe_method
        && bearer(req.headers()).is_none()
        && !same_origin(req.headers(), &state.config().server.public_url)
    {
        return error(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    match current_user(&state, req.headers()).await {
        Some((user, device)) => {
            req.extensions_mut().insert(user);
            if let Some(d) = device {
                req.extensions_mut().insert(d);
            }
            next.run(req).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "unauthenticated", "login": "/auth/login"})),
        )
            .into_response(),
    }
}

/// A local path to return to after sign-in (never another origin).
fn safe_return(to: Option<&str>) -> String {
    match to {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\") && !p.contains(['\r', '\n']) => {
            p.chars().take(1024).collect()
        }
        _ => "/".into(),
    }
}

fn page(status: StatusCode, title: &str, message: &str) -> Response {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width\"><title>{t}</title></head><body><h1>{t}</h1><p>{m}</p>\
         <p><a href=\"/auth/login\">Sign in again</a> · <a href=\"/\">Home</a></p></body></html>",
        t = esc(title),
        m = esc(message)
    );
    (status, Html(body)).into_response()
}

#[derive(Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

/// `GET /auth/login`: start the authorization code flow.
pub async fn login(State(state): State<Arc<AppState>>, Query(q): Query<LoginQuery>) -> Response {
    let Some(provider) = state.oidc() else {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in unavailable",
            "OpenID Connect is not configured.",
        );
    };
    let state_token = random_token(32);
    let login = LoginRequest {
        verifier: random_token(48),
        nonce: random_token(24),
        return_to: safe_return(q.return_to.as_deref()),
        created_at: now_ms(),
    };
    let url = match provider
        .authorization_url(&state_token, &login.nonce, &login.verifier)
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "cannot start sign-in");
            return page(
                StatusCode::BAD_GATEWAY,
                "Sign-in unavailable",
                "The identity provider cannot be reached. Try again later.",
            );
        }
    };
    if let Err(e) = state
        .store
        .create_login(&sha256_hex(state_token.as_bytes()), &login)
        .await
    {
        tracing::error!(error = %e, "cannot store sign-in");
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in unavailable",
            "Try again later.",
        );
    }
    // The cookie binds the callback to this browser (login CSRF).
    let mut resp = Redirect::to(&url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        state
            .sessions
            .cookie(LOGIN_COOKIE, &state_token, LOGIN_TTL.as_secs() as i64),
    );
    no_store(&mut resp);
    resp
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Map ID token (and userinfo) claims to an identity, or refuse the user.
pub fn identity(claims: &Value, cfg: &OidcConfig) -> Result<Identity, String> {
    let s = |k: &str| {
        claims
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let subject = s("sub").ok_or("the ID token has no subject")?;
    let groups = jwt::claim_values(claims, &cfg.groups_claim);
    let member = |list: &[String]| groups.iter().any(|g| list.contains(g));
    let admin = member(&cfg.admins);
    if !cfg.users.is_empty() && !member(&cfg.users) && !admin {
        return Err("your account is not allowed to use this server".into());
    }
    Ok(Identity {
        issuer: cfg.issuer.clone(),
        username: s("preferred_username")
            .or_else(|| s("email"))
            .unwrap_or_else(|| subject.clone()),
        display_name: s("name"),
        email: s("email"),
        subject,
        admin,
        groups,
    })
}

/// `GET /auth/callback`: finish the sign-in and create the session.
pub async fn callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(provider) = state.oidc() else {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sign-in unavailable",
            "OpenID Connect is not configured.",
        );
    };
    if let Some(err) = &q.error {
        let desc = q.error_description.as_deref().unwrap_or_default();
        tracing::info!(error = %err, description = %desc, "the provider refused the sign-in");
        return page(
            StatusCode::FORBIDDEN,
            "Sign-in refused",
            &format!("The identity provider said: {err} {desc}"),
        );
    }
    let (Some(code), Some(st)) = (q.code.as_deref(), q.state.as_deref()) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "The callback lacks its parameters.",
        );
    };
    let bound = cookie_value(&headers, &state.sessions.name(LOGIN_COOKIE))
        .is_some_and(|c| jwt::constant_time_eq(c.as_bytes(), st.as_bytes()));
    if !bound {
        return page(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "This sign-in was not started in this browser, or it took too long.",
        );
    }
    let login = match state.store.take_login(&sha256_hex(st.as_bytes())).await {
        Ok(Some(l)) if now_ms() - l.created_at < LOGIN_TTL.as_millis() as i64 => l,
        Ok(_) => {
            return page(
                StatusCode::BAD_REQUEST,
                "Sign-in failed",
                "This sign-in expired or was already used.",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot read sign-in");
            return page(StatusCode::SERVICE_UNAVAILABLE, "Sign-in failed", "Try again later.");
        }
    };
    let signed = match provider.sign_in(code, &login.verifier, &login.nonce).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "sign-in failed");
            return page(
                StatusCode::BAD_GATEWAY,
                "Sign-in failed",
                "The identity provider's answer was not accepted.",
            );
        }
    };
    let identity = match identity(&signed.claims, &provider.config) {
        Ok(i) => i,
        Err(msg) => {
            tracing::info!(subject = ?signed.claims.get("sub"), "sign-in refused: not in an allowed group");
            return page(StatusCode::FORBIDDEN, "Access denied", &msg);
        }
    };
    let now = now_ms();
    let user = match state.store.upsert_user(&identity, now).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "cannot store user");
            return page(StatusCode::SERVICE_UNAVAILABLE, "Sign-in failed", "Try again later.");
        }
    };
    update_avatar(&state, user.id, signed.claims.get("picture").and_then(Value::as_str)).await;
    let value = random_token(32);
    let lifetime = state.config().auth.lifetime().as_millis_i64();
    let session = NewSession {
        id_hash: &sha256_hex(value.as_bytes()),
        user_id: user.id,
        expires_at: now + lifetime,
        id_token: Some(&signed.id_token),
        user_agent: headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()),
    };
    if let Err(e) = state.store.create_session(&session, now).await {
        tracing::error!(error = %e, "cannot store session");
        return page(StatusCode::SERVICE_UNAVAILABLE, "Sign-in failed", "Try again later.");
    }
    tracing::info!(user = %user.username, admin = user.admin, "signed in");
    let mut resp = Redirect::to(&login.return_to).into_response();
    let h = resp.headers_mut();
    h.append(
        header::SET_COOKIE,
        state.sessions.cookie(SESSION_COOKIE, &value, lifetime / 1000),
    );
    h.append(header::SET_COOKIE, state.sessions.cookie(LOGIN_COOKIE, "", 0));
    no_store(&mut resp);
    resp
}

/// Fetch the user's picture when it changed (or drop it when the claim is
/// gone). Failures are logged and never block the sign-in.
async fn update_avatar(state: &AppState, user_id: i64, picture: Option<&str>) {
    let current = match state.store.avatar_source(user_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read avatar state");
            return;
        }
    };
    if current.as_deref() == picture {
        return;
    }
    let result = match picture {
        None => state.store.set_avatar(user_id, None).await.map(|_| ()),
        Some(url) => match avatar::fetch(url).await {
            Ok(jpeg) => state.store.set_avatar(user_id, Some((&jpeg, url))).await.map(|_| ()),
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "cannot fetch the user's picture; initials are shown");
                Ok(())
            }
        },
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "cannot store the user's picture");
    }
}

/// `POST /auth/logout`: end the session, then the provider's.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !same_origin(&headers, &state.config().server.public_url) {
        return error(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    let mut id_token = None;
    if let Some(value) = cookie_value(&headers, &state.sessions.name(SESSION_COOKIE)) {
        let hash = sha256_hex(value.as_bytes());
        state.sessions.forget(&hash);
        match state.store.delete_session(&hash).await {
            Ok(t) => id_token = t,
            Err(e) => tracing::warn!(error = %e, "cannot delete session"),
        }
    }
    let after = format!("{}/", jwt::trim_slash(&state.config().server.public_url));
    let target = match state.oidc() {
        Some(p) => p
            .logout_url(id_token.as_deref(), &after)
            .await
            .unwrap_or_else(|| "/".into()),
        None => "/".into(),
    };
    let mut resp = Redirect::to(&target).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, state.sessions.cookie(SESSION_COOKIE, "", 0));
    no_store(&mut resp);
    resp
}

fn no_store(resp: &mut Response) {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Dur;
    use serde_json::json;

    fn cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "ferrocine".into(),
            client_secret: None,
            scopes: vec!["openid".into()],
            groups_claim: "realm_access.roles".into(),
            users: vec!["media".into()],
            admins: vec!["media-admin".into()],
            leeway: Dur::secs(60),
        }
    }

    #[test]
    fn identities_and_groups() {
        let c = json!({"sub": "u1", "preferred_username": "alice", "name": "Alice",
                       "realm_access": {"roles": ["media"]}});
        let i = identity(&c, &cfg()).unwrap();
        assert_eq!((i.username.as_str(), i.admin), ("alice", false));
        let c = json!({"sub": "u2", "email": "bob@example.com", "realm_access": {"roles": ["media-admin"]}});
        let i = identity(&c, &cfg()).unwrap();
        assert_eq!((i.username.as_str(), i.admin), ("bob@example.com", true));
        assert!(identity(&json!({"sub": "u3"}), &cfg()).is_err());
        let mut open = cfg();
        open.users.clear();
        assert_eq!(identity(&json!({"sub": "u3"}), &open).unwrap().username, "u3");
    }

    #[test]
    fn return_paths_stay_local() {
        assert_eq!(safe_return(Some("/items/3?x=1")), "/items/3?x=1");
        for bad in ["//evil.example.com", "https://evil.example.com", "/\\evil", "x"] {
            assert_eq!(safe_return(Some(bad)), "/", "{bad}");
        }
        assert_eq!(safe_return(None), "/");
    }

    #[test]
    fn origin_checks() {
        let mut h = HeaderMap::new();
        assert!(!same_origin(&h, "https://media.example.com"));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://media.example.com"));
        assert!(same_origin(&h, "https://media.example.com/"));
        h.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&h, "https://media.example.com"));
        h.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&h, "https://media.example.com"));
    }

    #[test]
    fn cookies() {
        let s = Sessions::new("https://media.example.com");
        let c = s.cookie(SESSION_COOKIE, "abc", 60);
        assert_eq!(
            c.to_str().unwrap(),
            "__Host-ferrocine_session=abc; Path=/; HttpOnly; SameSite=Lax; Max-Age=60; Secure"
        );
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __Host-ferrocine_session=xyz"),
        );
        assert_eq!(cookie_value(&h, &s.name(SESSION_COOKIE)), Some("xyz"));
        assert_eq!(cookie_value(&h, "ferrocine_session"), None);
    }
}
