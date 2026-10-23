//! JWT signature and claim checks with `ring`, against keys published by the
//! provider (JWKS). Only asymmetric algorithms are accepted: never `none`,
//! never HMAC.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JwtError {
    #[error("malformed token")]
    Malformed,
    #[error("unsupported signing algorithm {0:?}")]
    UnsupportedAlgorithm(String),
    #[error("no matching signing key")]
    UnknownKey,
    #[error("invalid signature")]
    BadSignature,
    #[error("token expired")]
    Expired,
    #[error("token issued in the future")]
    NotYetValid,
    #[error("wrong issuer")]
    Issuer,
    #[error("wrong audience")]
    Audience,
    #[error("nonce mismatch")]
    Nonce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alg {
    Rs256,
    Rs384,
    Rs512,
    Ps256,
    Ps384,
    Ps512,
    Es256,
    Es384,
    EdDsa,
}

impl Alg {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "RS256" => Alg::Rs256,
            "RS384" => Alg::Rs384,
            "RS512" => Alg::Rs512,
            "PS256" => Alg::Ps256,
            "PS384" => Alg::Ps384,
            "PS512" => Alg::Ps512,
            "ES256" => Alg::Es256,
            "ES384" => Alg::Es384,
            "EdDSA" => Alg::EdDsa,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Alg::Rs256 => "RS256",
            Alg::Rs384 => "RS384",
            Alg::Rs512 => "RS512",
            Alg::Ps256 => "PS256",
            Alg::Ps384 => "PS384",
            Alg::Ps512 => "PS512",
            Alg::Es256 => "ES256",
            Alg::Es384 => "ES384",
            Alg::EdDsa => "EdDSA",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyMaterial {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    P256(Vec<u8>),
    P384(Vec<u8>),
    Ed25519(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub kid: Option<String>,
    pub alg: Option<String>,
    pub material: KeyMaterial,
}

impl Key {
    pub fn supports(&self, alg: Alg) -> bool {
        if self.alg.as_deref().is_some_and(|a| a != alg.name()) {
            return false;
        }
        matches!(
            (&self.material, alg),
            (
                KeyMaterial::Rsa { .. },
                Alg::Rs256 | Alg::Rs384 | Alg::Rs512 | Alg::Ps256 | Alg::Ps384 | Alg::Ps512
            ) | (KeyMaterial::P256(_), Alg::Es256)
                | (KeyMaterial::P384(_), Alg::Es384)
                | (KeyMaterial::Ed25519(_), Alg::EdDsa)
        )
    }

    fn verify(&self, alg: Alg, message: &[u8], sig: &[u8]) -> bool {
        let ok = match (&self.material, alg) {
            (KeyMaterial::Rsa { n, e }, _) => {
                let params: &signature::RsaParameters = match alg {
                    Alg::Rs256 => &signature::RSA_PKCS1_2048_8192_SHA256,
                    Alg::Rs384 => &signature::RSA_PKCS1_2048_8192_SHA384,
                    Alg::Rs512 => &signature::RSA_PKCS1_2048_8192_SHA512,
                    Alg::Ps256 => &signature::RSA_PSS_2048_8192_SHA256,
                    Alg::Ps384 => &signature::RSA_PSS_2048_8192_SHA384,
                    Alg::Ps512 => &signature::RSA_PSS_2048_8192_SHA512,
                    _ => return false,
                };
                RsaPublicKeyComponents { n, e }.verify(params, message, sig)
            }
            (KeyMaterial::P256(point), Alg::Es256) => {
                UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(message, sig)
            }
            (KeyMaterial::P384(point), Alg::Es384) => {
                UnparsedPublicKey::new(&signature::ECDSA_P384_SHA384_FIXED, point).verify(message, sig)
            }
            (KeyMaterial::Ed25519(x), Alg::EdDsa) => {
                UnparsedPublicKey::new(&signature::ED25519, x).verify(message, sig)
            }
            _ => return false,
        };
        ok.is_ok()
    }
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

pub fn b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Left-pad an EC coordinate to the curve's field size.
fn coordinate(s: &str, len: usize) -> Option<Vec<u8>> {
    let mut v = b64(s)?;
    if v.len() > len {
        return None;
    }
    let mut padded = vec![0; len - v.len()];
    padded.append(&mut v);
    Some(padded)
}

impl Jwk {
    fn into_key(self) -> Option<Key> {
        if self.usage.as_deref().is_some_and(|u| u != "sig") {
            return None;
        }
        let material = match (self.kty.as_str(), self.crv.as_deref()) {
            ("RSA", _) => KeyMaterial::Rsa {
                n: b64(self.n.as_deref()?)?,
                e: b64(self.e.as_deref()?)?,
            },
            ("EC", Some(crv @ ("P-256" | "P-384"))) => {
                let size = if crv == "P-256" { 32 } else { 48 };
                let mut point = vec![0x04];
                point.extend(coordinate(self.x.as_deref()?, size)?);
                point.extend(coordinate(self.y.as_deref()?, size)?);
                if crv == "P-256" {
                    KeyMaterial::P256(point)
                } else {
                    KeyMaterial::P384(point)
                }
            }
            ("OKP", Some("Ed25519")) => KeyMaterial::Ed25519(b64(self.x.as_deref()?)?),
            _ => return None,
        };
        Some(Key {
            kid: self.kid,
            alg: self.alg,
            material,
        })
    }
}

pub fn parse_jwks(body: &Value) -> Result<Vec<Key>, String> {
    let keys = body
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("JWKS has no `keys` array")?;
    let parsed: Vec<Key> = keys
        .iter()
        .filter_map(|k| serde_json::from_value::<Jwk>(k.clone()).ok())
        .filter_map(Jwk::into_key)
        .collect();
    if parsed.is_empty() {
        return Err("JWKS holds no usable signing key".into());
    }
    Ok(parsed)
}

pub fn trim_slash(s: &str) -> &str {
    s.trim_end_matches('/')
}

/// The values of a dotted claim path: a space-separated string, a string
/// list, or a single scalar.
pub fn claim_values(claims: &Value, path: &str) -> Vec<String> {
    let mut v = claims;
    for seg in path.split('.') {
        match v.get(seg) {
            Some(next) => v = next,
            None => return Vec::new(),
        }
    }
    match v {
        Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        Value::Bool(_) | Value::Number(_) => vec![v.to_string()],
        _ => Vec::new(),
    }
}

/// A token split into its parts, before verification.
pub struct Parsed<'a> {
    pub alg: Alg,
    pub kid: Option<String>,
    signed: &'a str,
    sig: Vec<u8>,
    payload: &'a str,
}

pub fn split(token: &str) -> Result<Parsed<'_>, JwtError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(JwtError::Malformed);
    };
    let header: Value = b64(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(JwtError::Malformed)?;
    let alg_name = header.get("alg").and_then(Value::as_str).unwrap_or_default();
    let alg = Alg::parse(alg_name).ok_or_else(|| JwtError::UnsupportedAlgorithm(alg_name.to_owned()))?;
    Ok(Parsed {
        alg,
        kid: header.get("kid").and_then(Value::as_str).map(str::to_owned),
        signed: &token[..h.len() + 1 + p.len()],
        sig: b64(s).ok_or(JwtError::Malformed)?,
        payload: p,
    })
}

/// Keys that may have signed the token: the one named by `kid`, or every key
/// compatible with the algorithm when the token names none.
pub fn candidates<'k>(keys: &'k [Key], kid: Option<&str>, alg: Alg) -> Vec<&'k Key> {
    keys.iter()
        .filter(|k| k.supports(alg))
        .filter(|k| kid.is_none_or(|kid| k.kid.as_deref() == Some(kid)))
        .collect()
}

/// What an ID token must satisfy (OpenID Connect Core 1.0, 3.1.3.7).
pub struct Expect<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    pub leeway: i64,
    pub now: i64,
}

/// Verify the signature with one of `keys`, then the claims; return the claims.
pub fn verify_id_token(parsed: &Parsed<'_>, keys: &[&Key], expect: &Expect<'_>) -> Result<Value, JwtError> {
    if keys.is_empty() {
        return Err(JwtError::UnknownKey);
    }
    if !keys
        .iter()
        .any(|k| k.verify(parsed.alg, parsed.signed.as_bytes(), &parsed.sig))
    {
        return Err(JwtError::BadSignature);
    }
    let claims: Value = b64(parsed.payload)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(JwtError::Malformed)?;
    let exp = claims.get("exp").and_then(Value::as_i64).ok_or(JwtError::Malformed)?;
    if expect.now > exp + expect.leeway {
        return Err(JwtError::Expired);
    }
    if let Some(iat) = claims.get("iat").and_then(Value::as_i64)
        && iat > expect.now + expect.leeway
    {
        return Err(JwtError::NotYetValid);
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_i64)
        && expect.now + expect.leeway < nbf
    {
        return Err(JwtError::NotYetValid);
    }
    let iss = claims.get("iss").and_then(Value::as_str).unwrap_or_default();
    if trim_slash(iss) != trim_slash(expect.issuer) {
        return Err(JwtError::Issuer);
    }
    let aud = claim_values(&claims, "aud");
    if !aud.iter().any(|a| a == expect.client_id) {
        return Err(JwtError::Audience);
    }
    // With several audiences, the authorized party must be this client.
    if aud.len() > 1
        && let Some(azp) = claims.get("azp").and_then(Value::as_str)
        && azp != expect.client_id
    {
        return Err(JwtError::Audience);
    }
    let nonce = claims.get("nonce").and_then(Value::as_str).unwrap_or_default();
    if !constant_time_eq(nonce.as_bytes(), expect.nonce.as_bytes()) {
        return Err(JwtError::Nonce);
    }
    Ok(claims)
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, RsaKeyPair};
    use serde_json::json;

    /// Throwaway key, generated for these tests only.
    pub const RSA_PKCS1: &[u8] = include_bytes!("test-rsa-2048.pkcs1.der");
    const NOW: i64 = 1_800_000_000;

    pub fn enc(v: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(v)
    }

    /// Sign `claims` with the test RSA key (RS256, kid `k1`).
    pub fn rs256(claims: &Value) -> String {
        let pair = RsaKeyPair::from_der(RSA_PKCS1).unwrap();
        let header = json!({"alg": "RS256", "kid": "k1", "typ": "JWT"});
        let signed = format!(
            "{}.{}",
            enc(header.to_string().as_bytes()),
            enc(claims.to_string().as_bytes())
        );
        let mut sig = vec![0; pair.public().modulus_len()];
        pair.sign(
            &signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signed.as_bytes(),
            &mut sig,
        )
        .unwrap();
        format!("{signed}.{}", enc(&sig))
    }

    /// The JWKS of the test RSA key.
    pub fn rsa_jwks() -> Value {
        let pair = RsaKeyPair::from_der(RSA_PKCS1).unwrap();
        let parts: signature::RsaPublicKeyComponents<Vec<u8>> = pair.public().into();
        json!({"keys": [{"kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256", "n": enc(&parts.n), "e": enc(&parts.e)}]})
    }

    fn claims() -> Value {
        json!({"iss": "https://idp.example.com/", "aud": "ferrocine", "sub": "u1", "exp": NOW + 300,
               "iat": NOW, "nonce": "n0nce"})
    }

    fn expect() -> Expect<'static> {
        Expect {
            issuer: "https://idp.example.com",
            client_id: "ferrocine",
            nonce: "n0nce",
            leeway: 60,
            now: NOW,
        }
    }

    fn check(token: &str) -> Result<Value, JwtError> {
        let keys = parse_jwks(&rsa_jwks()).unwrap();
        let p = split(token)?;
        let c = candidates(&keys, p.kid.as_deref(), p.alg);
        verify_id_token(&p, &c, &expect())
    }

    #[test]
    fn accepts_a_valid_token() {
        assert_eq!(check(&rs256(&claims())).unwrap()["sub"], "u1");
    }

    #[test]
    fn rejects_bad_claims() {
        let with = |k: &str, v: Value| {
            let mut c = claims();
            c[k] = v;
            check(&rs256(&c)).unwrap_err()
        };
        assert_eq!(with("exp", json!(NOW - 61)), JwtError::Expired);
        assert_eq!(with("iat", json!(NOW + 600)), JwtError::NotYetValid);
        assert_eq!(with("iss", json!("https://evil.example.com")), JwtError::Issuer);
        assert_eq!(with("aud", json!(["other"])), JwtError::Audience);
        assert_eq!(with("nonce", json!("replayed")), JwtError::Nonce);
        let mut c = claims();
        c["aud"] = json!(["ferrocine", "other"]);
        c["azp"] = json!("other");
        assert_eq!(check(&rs256(&c)).unwrap_err(), JwtError::Audience);
    }

    #[test]
    fn rejects_forgeries() {
        let token = rs256(&claims());
        let (head, _) = token.rsplit_once('.').unwrap();
        let mut forged = claims();
        forged["sub"] = json!("admin");
        let tampered = format!(
            "{}.{}.{}",
            head.split('.').next().unwrap(),
            enc(forged.to_string().as_bytes()),
            token.rsplit('.').next().unwrap()
        );
        assert_eq!(check(&tampered).unwrap_err(), JwtError::BadSignature);
        let none = format!(
            "{}.{}.",
            enc(br#"{"alg":"none"}"#),
            enc(claims().to_string().as_bytes())
        );
        assert!(matches!(check(&none).unwrap_err(), JwtError::UnsupportedAlgorithm(_)));
        let hs = format!(
            "{}.{}.x",
            enc(br#"{"alg":"HS256"}"#),
            enc(claims().to_string().as_bytes())
        );
        assert!(matches!(check(&hs).unwrap_err(), JwtError::UnsupportedAlgorithm(_)));
    }

    #[test]
    fn ec_keys_parse_and_verify() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref();
        let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "e1", "x": enc(&point[1..33]), "y": enc(&point[33..])}]});
        let keys = parse_jwks(&jwks).unwrap();
        let header = json!({"alg": "ES256", "kid": "e1"});
        let signed = format!(
            "{}.{}",
            enc(header.to_string().as_bytes()),
            enc(claims().to_string().as_bytes())
        );
        let sig = pair.sign(&rng, signed.as_bytes()).unwrap();
        let token = format!("{signed}.{}", enc(sig.as_ref()));
        let p = split(&token).unwrap();
        let c = candidates(&keys, p.kid.as_deref(), p.alg);
        assert_eq!(verify_id_token(&p, &c, &expect()).unwrap()["sub"], "u1");
    }
}
