//! The OpenID Connect provider: discovery, signing keys, the authorization
//! code exchange (with PKCE) and userinfo.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::jwt::{self, Expect, Key, trim_slash};
use crate::config::OidcConfig;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Discovery and keys are refreshed this often.
const REFRESH: Duration = Duration::from_secs(3600);
/// Minimum delay between two refreshes triggered by an unknown key id.
const ON_DEMAND_REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: Option<String>,
    pub end_session_endpoint: Option<String>,
    pub jwks_uri: String,
    /// `client_secret_basic` unless the provider supports only `client_secret_post`.
    pub secret_in_body: bool,
}

/// The verified result of a sign-in.
#[derive(Clone, Debug)]
pub struct SignIn {
    /// ID token claims, completed by userinfo.
    pub claims: Value,
    pub id_token: String,
}

pub struct Provider {
    pub config: OidcConfig,
    pub redirect_uri: String,
    http: reqwest::Client,
    discovery: RwLock<Option<Arc<Discovery>>>,
    keys: RwLock<Vec<Key>>,
    last_refresh: Mutex<Option<Instant>>,
    /// Ends the refresh loop when a reload replaces this provider.
    stop: CancellationToken,
}

/// Random URL-safe text from `n` bytes of the system CSPRNG.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf).expect("system random generator");
    URL_SAFE_NO_PAD.encode(buf)
}

/// PKCE S256 challenge of a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn parse_discovery(doc: &Value, issuer: &str) -> anyhow::Result<Discovery> {
    // OpenID Connect Discovery 1.0, section 4.3.
    let announced = doc.get("issuer").and_then(Value::as_str).unwrap_or_default();
    if trim_slash(announced) != trim_slash(issuer) {
        bail!("discovery document announces issuer {announced:?}, expected {issuer:?}");
    }
    let s = |k: &str| doc.get(k).and_then(Value::as_str).map(str::to_owned);
    let methods: Vec<&str> = doc
        .get("token_endpoint_auth_methods_supported")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Ok(Discovery {
        authorization_endpoint: s("authorization_endpoint").context("discovery: no authorization_endpoint")?,
        token_endpoint: s("token_endpoint").context("discovery: no token_endpoint")?,
        userinfo_endpoint: s("userinfo_endpoint"),
        end_session_endpoint: s("end_session_endpoint"),
        jwks_uri: s("jwks_uri").context("discovery: no jwks_uri")?,
        secret_in_body: !methods.is_empty()
            && !methods.contains(&"client_secret_basic")
            && methods.contains(&"client_secret_post"),
    })
}

impl Provider {
    pub fn new(config: OidcConfig, public_url: &str, http: reqwest::Client) -> Self {
        Self {
            redirect_uri: format!("{}/auth/callback", trim_slash(public_url)),
            config,
            http,
            discovery: RwLock::new(None),
            keys: RwLock::new(Vec::new()),
            last_refresh: Mutex::new(None),
            stop: CancellationToken::new(),
        }
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }

    /// Keep discovery and keys fresh until `shutdown`.
    pub async fn refresh_loop(self: Arc<Self>, shutdown: CancellationToken) {
        let mut retry = Duration::from_secs(5);
        loop {
            let wait = match self.refresh(Duration::ZERO).await {
                Ok(()) => {
                    retry = Duration::from_secs(5);
                    REFRESH
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in = ?retry, "cannot reach the OIDC provider");
                    let w = retry;
                    retry = (retry * 2).min(Duration::from_secs(300));
                    w
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = self.stop.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| url.to_owned())?;
        if !resp.status().is_success() {
            bail!("{url}: HTTP {}", resp.status());
        }
        resp.json().await.with_context(|| url.to_owned())
    }

    /// Fetch discovery and keys unless done within `min_interval`.
    async fn refresh(&self, min_interval: Duration) -> anyhow::Result<()> {
        let mut last = self.last_refresh.lock().await;
        if let Some(t) = *last
            && t.elapsed() < min_interval
        {
            return Ok(());
        }
        *last = Some(Instant::now());
        let url = format!("{}/.well-known/openid-configuration", trim_slash(&self.config.issuer));
        let discovery = parse_discovery(&self.get_json(&url).await?, &self.config.issuer)?;
        let keys = jwt::parse_jwks(&self.get_json(&discovery.jwks_uri).await?).map_err(|e| anyhow!(e))?;
        tracing::debug!(keys = keys.len(), "OIDC discovery and keys refreshed");
        if let Ok(mut k) = self.keys.write() {
            *k = keys;
        }
        if let Ok(mut d) = self.discovery.write() {
            *d = Some(Arc::new(discovery));
        }
        Ok(())
    }

    pub async fn discovery(&self) -> anyhow::Result<Arc<Discovery>> {
        if let Some(d) = self.discovery.read().ok().and_then(|d| d.clone()) {
            return Ok(d);
        }
        self.refresh(Duration::from_secs(2)).await?;
        self.discovery
            .read()
            .ok()
            .and_then(|d| d.clone())
            .ok_or_else(|| anyhow!("the OIDC provider is unreachable"))
    }

    pub fn is_ready(&self) -> bool {
        self.discovery.read().is_ok_and(|d| d.is_some())
    }

    /// The URL to send the browser to.
    pub async fn authorization_url(&self, state: &str, nonce: &str, verifier: &str) -> anyhow::Result<String> {
        let d = self.discovery().await?;
        let mut url = url::Url::parse(&d.authorization_endpoint).context("invalid authorization_endpoint")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", &self.config.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", &pkce_challenge(verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Exchange the code, verify the ID token, and complete its claims with userinfo.
    pub async fn sign_in(&self, code: &str, verifier: &str, nonce: &str) -> anyhow::Result<SignIn> {
        let d = self.discovery().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", verifier),
            ("client_id", self.config.client_id.as_str()),
        ];
        let mut req = self.http.post(&d.token_endpoint).timeout(HTTP_TIMEOUT);
        if let Some(secret) = &self.config.client_secret {
            if d.secret_in_body {
                form.push(("client_secret", secret));
            } else {
                // RFC 6749, 2.3.1: form-encode the parts before Basic encoding.
                let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
                req = req.basic_auth(enc(&self.config.client_id), Some(enc(secret)));
            }
        }
        let resp = req.form(&form).send().await.context("token request failed")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("token response is not JSON")?;
        if !status.is_success() {
            let err = body.get("error").and_then(Value::as_str).unwrap_or("unknown");
            let desc = body
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            bail!("token endpoint refused the code: {err} {desc}");
        }
        let id_token = body
            .get("id_token")
            .and_then(Value::as_str)
            .context("token response has no id_token (is `openid` among the scopes?)")?
            .to_owned();
        let mut claims = self.verify(&id_token, nonce).await?;
        if let (Some(endpoint), Some(access)) = (&d.userinfo_endpoint, body.get("access_token").and_then(Value::as_str))
        {
            match self.userinfo(endpoint, access).await {
                Ok(info) if info.get("sub") == claims.get("sub") => {
                    // The ID token wins; userinfo fills what it lacks (groups, often).
                    if let (Some(c), Some(i)) = (claims.as_object_mut(), info.as_object()) {
                        for (k, v) in i {
                            c.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
                Ok(_) => bail!("userinfo answered for another subject"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "userinfo failed; using the ID token alone"),
            }
        }
        Ok(SignIn { claims, id_token })
    }

    async fn userinfo(&self, endpoint: &str, access_token: &str) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(endpoint)
            .bearer_auth(access_token)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("userinfo: HTTP {}", resp.status());
        }
        // Signed (JWT) userinfo responses are not supported; JSON is the default.
        resp.json().await.context("userinfo is not JSON")
    }

    async fn verify(&self, token: &str, nonce: &str) -> anyhow::Result<Value> {
        let parsed = jwt::split(token)?;
        let expect = Expect {
            issuer: &self.config.issuer,
            client_id: &self.config.client_id,
            nonce,
            leeway: self.config.leeway.0.as_secs() as i64,
            now: jiff::Timestamp::now().as_second(),
        };
        let keys = self.keys.read().map(|k| k.clone()).unwrap_or_default();
        let mut c = jwt::candidates(&keys, parsed.kid.as_deref(), parsed.alg);
        let fresh;
        if c.is_empty() {
            // Unknown key: the provider may have rotated its keys.
            self.refresh(ON_DEMAND_REFRESH).await?;
            fresh = self.keys.read().map(|k| k.clone()).unwrap_or_default();
            c = jwt::candidates(&fresh, parsed.kid.as_deref(), parsed.alg);
        }
        Ok(jwt::verify_id_token(&parsed, &c, &expect)?)
    }

    /// Where to send the browser after the local sign-out.
    pub async fn logout_url(&self, id_token: Option<&str>, after: &str) -> Option<String> {
        let d = self.discovery().await.ok()?;
        let mut url = url::Url::parse(d.end_session_endpoint.as_deref()?).ok()?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("client_id", &self.config.client_id)
                .append_pair("post_logout_redirect_uri", after);
            if let Some(t) = id_token {
                q.append_pair("id_token_hint", t);
            }
        }
        Some(url.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_matches_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_token(32).len(), 43);
        assert_ne!(random_token(16), random_token(16));
    }

    #[test]
    fn discovery_checks_the_issuer_and_auth_method() {
        let doc = json!({
            "issuer": "https://id.example.com/realms/home",
            "authorization_endpoint": "https://id.example.com/auth",
            "token_endpoint": "https://id.example.com/token",
            "jwks_uri": "https://id.example.com/certs",
            "token_endpoint_auth_methods_supported": ["client_secret_post", "private_key_jwt"]
        });
        let d = parse_discovery(&doc, "https://id.example.com/realms/home/").unwrap();
        assert!(d.secret_in_body);
        assert!(d.end_session_endpoint.is_none());
        assert!(parse_discovery(&doc, "https://evil.example.com").is_err());
    }
}
