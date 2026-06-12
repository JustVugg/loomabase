//! Reference signed-token device authentication (feature `server`).
//!
//! Two pluggable [`DeviceAuthenticator`]s verify a compact JWT from the
//! `Authorization: Bearer` header and return the authenticated tenant and
//! device: [`JwtDeviceAuthenticator`] (symmetric `HS256`) and
//! [`RsaJwtDeviceAuthenticator`] (asymmetric `RS256`, for multi-issuer setups
//! where the server only holds a public key). Both accept exactly one algorithm
//! (rejecting `none`/algorithm-confusion), check `exp`, and verify the signature
//! before trusting any claim.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use jsonwebtoken::crypto;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;

use crate::crdt::validate_identifier;
use crate::http::{AuthenticatedDevice, DeviceAuthenticator, required_header};

type HmacSha256 = Hmac<Sha256>;
const MAX_BEARER_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
struct TokenHeader {
    alg: String,
}

#[derive(serde::Serialize, Deserialize)]
struct Claims {
    tenant_id: String,
    device_id: String,
    /// Expiry as seconds since the Unix epoch.
    exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    aud: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    iss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_tables: Option<BTreeSet<String>>,
}

/// Optional audience/issuer the verifier requires of every token.
#[derive(Clone, Debug, Default)]
struct ClaimPolicy {
    audience: Option<String>,
    issuer: Option<String>,
}

/// Verifies symmetric `HS256` bearer tokens against a shared secret.
#[derive(Clone)]
pub struct JwtDeviceAuthenticator {
    secret: Vec<u8>,
    policy: ClaimPolicy,
}

impl JwtDeviceAuthenticator {
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            policy: ClaimPolicy::default(),
        }
    }

    /// Requires tokens to carry this `aud` claim.
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.policy.audience = Some(audience.into());
        self
    }

    /// Requires tokens to carry this `iss` claim.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.policy.issuer = Some(issuer.into());
        self
    }
}

impl DeviceAuthenticator for JwtDeviceAuthenticator {
    fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<AuthenticatedDevice, String> {
        let token = bearer_token(headers)?;
        let (header_b64, payload_b64, signature) = split_token(token)?;
        expect_alg(header_b64, "HS256")?;

        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|_| "invalid signing key".to_owned())?;
        mac.update(signing_input.as_bytes());
        // Constant-time comparison.
        mac.verify_slice(&signature)
            .map_err(|_| "invalid token signature".to_owned())?;

        validated_identity(payload_b64, &self.policy)
    }
}

/// Verifies asymmetric `RS256` bearer tokens against an RSA public key.
#[derive(Clone)]
pub struct RsaJwtDeviceAuthenticator {
    decoding_key: DecodingKey,
    policy: ClaimPolicy,
}

impl RsaJwtDeviceAuthenticator {
    /// Builds a verifier from an SPKI PEM-encoded RSA public key.
    ///
    /// # Errors
    /// Returns an error if `pem` is not a valid RSA public key.
    pub fn from_public_key_pem(pem: &str) -> Result<Self, String> {
        let decoding_key = DecodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|error| format!("invalid RSA public key: {error}"))?;
        Ok(Self {
            decoding_key,
            policy: ClaimPolicy::default(),
        })
    }

    /// Requires tokens to carry this `aud` claim.
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.policy.audience = Some(audience.into());
        self
    }

    /// Requires tokens to carry this `iss` claim.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.policy.issuer = Some(issuer.into());
        self
    }
}

impl DeviceAuthenticator for RsaJwtDeviceAuthenticator {
    fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<AuthenticatedDevice, String> {
        let token = bearer_token(headers)?;
        let (header_b64, payload_b64, _) = split_token(token)?;
        expect_alg(header_b64, "RS256")?;

        let (signing_input, signature_b64) = token
            .rsplit_once('.')
            .ok_or_else(|| "malformed token".to_owned())?;
        let valid = crypto::verify(
            signature_b64,
            signing_input.as_bytes(),
            &self.decoding_key,
            Algorithm::RS256,
        )
        .map_err(|_| "invalid token signature".to_owned())?;
        if !valid {
            return Err("invalid token signature".to_owned());
        }

        validated_identity(payload_b64, &self.policy)
    }
}

// --- shared envelope helpers -------------------------------------------------

fn bearer_token(headers: &axum::http::HeaderMap) -> Result<&str, String> {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "missing authorization header".to_owned())?
        .strip_prefix("Bearer ")
        .ok_or_else(|| "authorization header must be a Bearer token".to_owned())?;
    if token.len() > MAX_BEARER_TOKEN_BYTES {
        return Err("bearer token is too large".to_owned());
    }
    Ok(token)
}

fn split_token(token: &str) -> Result<(&str, &str, Vec<u8>), String> {
    let mut parts = token.split('.');
    let header_b64 = parts.next().ok_or_else(|| "malformed token".to_owned())?;
    let payload_b64 = parts.next().ok_or_else(|| "malformed token".to_owned())?;
    let signature_b64 = parts.next().ok_or_else(|| "malformed token".to_owned())?;
    if parts.next().is_some() {
        return Err("malformed token".to_owned());
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|_| "invalid token signature encoding".to_owned())?;
    Ok((header_b64, payload_b64, signature))
}

fn expect_alg(header_b64: &str, expected: &str) -> Result<(), String> {
    let header: TokenHeader = decode_segment(header_b64)?;
    if header.alg == expected {
        Ok(())
    } else {
        Err("unsupported token algorithm".to_owned())
    }
}

fn validated_identity(
    payload_b64: &str,
    policy: &ClaimPolicy,
) -> Result<AuthenticatedDevice, String> {
    let claims: Claims = decode_segment(payload_b64)?;
    let now = unix_now();
    if claims.exp <= now {
        return Err("token expired".to_owned());
    }
    if claims.nbf.is_some_and(|nbf| nbf > now) {
        return Err("token is not yet valid".to_owned());
    }
    if let Some(expected) = &policy.audience
        && claims.aud.as_deref() != Some(expected.as_str())
    {
        return Err("token audience mismatch".to_owned());
    }
    if let Some(expected) = &policy.issuer
        && claims.iss.as_deref() != Some(expected.as_str())
    {
        return Err("token issuer mismatch".to_owned());
    }
    validate_identifier("token tenant_id", &claims.tenant_id)
        .map_err(|_| "token contains an invalid tenant".to_owned())?;
    validate_identifier("token device_id", &claims.device_id)
        .map_err(|_| "token contains an invalid device".to_owned())?;
    Ok(AuthenticatedDevice {
        tenant_id: claims.tenant_id,
        device_id: claims.device_id,
        allowed_tables: claims.allowed_tables,
    })
}

/// Verifies Supabase Auth access tokens against a rotatable asymmetric JWKS.
///
/// The token `sub` identifies the user. By default the tenant is read from
/// `app_metadata.tenant_id`, falling back to `sub` for per-user deployments.
/// Clients must provide `x-device-id`; Loomabase namespaces it by `sub` so one
/// user cannot spoof another user's CRDT device identity.
#[derive(Clone)]
pub struct SupabaseJwtAuthenticator {
    jwks: Arc<RwLock<JwkSet>>,
    issuer: String,
    audience: Option<String>,
    tenant_claim: String,
    tables_claim: String,
}

impl SupabaseJwtAuthenticator {
    /// Creates a verifier from the Supabase JWKS JSON and project Auth issuer.
    pub fn from_jwks_json(jwks_json: &str, issuer: impl Into<String>) -> Result<Self, String> {
        let jwks: JwkSet =
            serde_json::from_str(jwks_json).map_err(|_| "invalid JWKS JSON".to_owned())?;
        if jwks.keys.is_empty() {
            return Err("JWKS does not contain verification keys".to_owned());
        }
        Ok(Self {
            jwks: Arc::new(RwLock::new(jwks)),
            issuer: issuer.into(),
            audience: Some("authenticated".to_owned()),
            tenant_claim: "app_metadata.tenant_id".to_owned(),
            tables_claim: "app_metadata.loomabase_tables".to_owned(),
        })
    }

    #[must_use]
    pub fn with_audience(mut self, audience: Option<String>) -> Self {
        self.audience = audience;
        self
    }

    #[must_use]
    pub fn with_tenant_claim(mut self, path: impl Into<String>) -> Self {
        self.tenant_claim = path.into();
        self
    }

    #[must_use]
    pub fn with_tables_claim(mut self, path: impl Into<String>) -> Self {
        self.tables_claim = path.into();
        self
    }

    /// Atomically replaces trusted public keys after a JWKS refresh.
    pub fn replace_jwks_json(&self, jwks_json: &str) -> Result<(), String> {
        let replacement: JwkSet =
            serde_json::from_str(jwks_json).map_err(|_| "invalid JWKS JSON".to_owned())?;
        if replacement.keys.is_empty() {
            return Err("JWKS does not contain verification keys".to_owned());
        }
        let mut current = self
            .jwks
            .write()
            .map_err(|_| "JWKS lock is poisoned".to_owned())?;
        *current = replacement;
        Ok(())
    }
}

impl DeviceAuthenticator for SupabaseJwtAuthenticator {
    fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<AuthenticatedDevice, String> {
        let token = bearer_token(headers)?;
        let header = decode_header(token).map_err(|_| "invalid token header".to_owned())?;
        let kid = header
            .kid
            .as_deref()
            .ok_or_else(|| "token is missing kid".to_owned())?;
        if !matches!(
            header.alg,
            Algorithm::RS256 | Algorithm::ES256 | Algorithm::EdDSA
        ) {
            return Err("unsupported token algorithm".to_owned());
        }
        let jwk = self
            .jwks
            .read()
            .map_err(|_| "JWKS lock is poisoned".to_owned())?
            .find(kid)
            .cloned()
            .ok_or_else(|| "token kid is not trusted".to_owned())?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|_| "invalid JWK".to_owned())?;
        let mut validation = Validation::new(header.alg);
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        if let Some(audience) = &self.audience {
            validation.set_audience(&[audience]);
            validation.required_spec_claims.insert("aud".to_owned());
        } else {
            validation.validate_aud = false;
        }
        let claims = decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|_| "invalid token".to_owned())?
            .claims;
        let sub = claim_string(&claims, "sub")
            .ok_or_else(|| "token is missing a valid sub".to_owned())?;
        validate_identifier("token sub", sub)
            .map_err(|_| "token contains an invalid sub".to_owned())?;
        let tenant_id = claim_string(&claims, &self.tenant_claim)
            .unwrap_or(sub)
            .to_owned();
        validate_identifier("token tenant_id", &tenant_id)
            .map_err(|_| "token contains an invalid tenant".to_owned())?;

        let device_suffix = required_header(headers, "x-device-id")?;
        validate_identifier("x-device-id", &device_suffix)
            .map_err(|_| "invalid x-device-id header".to_owned())?;
        let device_id = format!("{sub}/{device_suffix}");
        validate_identifier("device_id", &device_id)
            .map_err(|_| "combined device identity is too long".to_owned())?;

        let allowed_tables = claim_string_set(&claims, &self.tables_claim)?;
        Ok(AuthenticatedDevice {
            tenant_id,
            device_id,
            allowed_tables,
        })
    }
}

fn claim_value<'a>(claims: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(claims, |value, segment| value.get(segment))
}

fn claim_string<'a>(claims: &'a serde_json::Value, path: &str) -> Option<&'a str> {
    claim_value(claims, path)?.as_str()
}

fn claim_string_set(
    claims: &serde_json::Value,
    path: &str,
) -> Result<Option<BTreeSet<String>>, String> {
    let Some(value) = claim_value(claims, path) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| "table authorization claim must be an array".to_owned())?;
    let mut tables = BTreeSet::new();
    for value in values {
        let table = value
            .as_str()
            .ok_or_else(|| "table authorization claim must contain strings".to_owned())?;
        validate_identifier("authorized table", table)
            .map_err(|_| "table authorization claim contains an invalid table".to_owned())?;
        tables.insert(table.to_owned());
    }
    Ok(Some(tables))
}

fn decode_segment<T: DeserializeOwned>(segment: &str) -> Result<T, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| "invalid token encoding".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|_| "invalid token claims".to_owned())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

// --- reference encoders (clients/tests) --------------------------------------

/// Mints an `HS256` token for `tenant_id`/`device_id` expiring at `exp_unix`
/// (seconds since the Unix epoch). The reference client encoder.
#[must_use]
pub fn encode_token(secret: &[u8], tenant_id: &str, device_id: &str, exp_unix: u64) -> String {
    encode_token_with_claims(secret, tenant_id, device_id, exp_unix, None, None, None)
}

/// Mints an `HS256` token with optional `nbf`/`aud`/`iss` claims, for tokens
/// scoped to an audience or issuer.
///
/// # Panics
/// Panics only if serializing the claims to JSON fails, which cannot occur for
/// the fixed string and integer fields used here.
#[must_use]
pub fn encode_token_with_claims(
    secret: &[u8],
    tenant_id: &str,
    device_id: &str,
    exp_unix: u64,
    not_before: Option<u64>,
    audience: Option<&str>,
    issuer: Option<&str>,
) -> String {
    let claims = Claims {
        tenant_id: tenant_id.to_owned(),
        device_id: device_id.to_owned(),
        exp: exp_unix,
        nbf: not_before,
        aud: audience.map(ToOwned::to_owned),
        iss: issuer.map(ToOwned::to_owned),
        allowed_tables: None,
    };
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims serialize"));
    let signing_input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{signing_input}.{signature}")
}

/// Mints an `RS256` token signed with a PKCS#8 PEM-encoded RSA private key. The
/// reference asymmetric client encoder.
///
/// # Errors
/// Returns an error if `private_key_pem` is not a valid RSA private key.
pub fn encode_token_rs256(
    private_key_pem: &str,
    tenant_id: &str,
    device_id: &str,
    exp_unix: u64,
) -> Result<String, String> {
    let private_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
        .map_err(|error| format!("invalid RSA private key: {error}"))?;
    encode(
        &Header::new(Algorithm::RS256),
        &claims(tenant_id, device_id, exp_unix),
        &private_key,
    )
    .map_err(|error| format!("failed to sign RSA token: {error}"))
}

fn claims(tenant_id: &str, device_id: &str, exp_unix: u64) -> Claims {
    Claims {
        tenant_id: tenant_id.to_owned(),
        device_id: device_id.to_owned(),
        exp: exp_unix,
        nbf: None,
        aud: None,
        iss: None,
        allowed_tables: None,
    }
}
