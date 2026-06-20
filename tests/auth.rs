#![cfg(feature = "server")]

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use loomabase::auth::{
    JwtDeviceAuthenticator, RsaJwtDeviceAuthenticator, SupabaseJwtAuthenticator, encode_token,
    encode_token_rs256, encode_token_with_claims,
};
use loomabase::http::DeviceAuthenticator;

const RSA_PRIVATE_KEY: &str = r"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC795YvSW46vFTg
xB+L1Y3HerEzfa/lhM/TLsAzMfdX3Q3fXzUTfeat0W+WYDjdi+JsPGli5/BstTcz
uex/9qAUIzMl8k3S4jx9D5uVnTzJqJhOMCq1DZfDBzuSRgkDAzFJ9UifKgyWrygD
yt38cXM611C06UpshbNcnJoFwtyYoIoIT7sgZGcGZ1GPRysT9FNaUG/ElT2503kd
ZfA58zbl1gSwG5NtrEBq/Kai4Ch/OwYJKHhtWVd740hYIHT6Q4/SyvpWJMh/SdPv
ujoumkwo6mwMIks0aVcq/H2VVvOBjH5neCHtxBzseBVt8jas4YYkigReFVWx/KN1
Gkan3i9BAgMBAAECggEAHCsMZXYLyp802ba6WC0UFqE4felncINN1lggCGwD+jWl
IbogTOql+kVeWLrUZWr+7vkXUpxEfN35RvX6pECJbmB0sUqVnAqNmwY5lXBfTtnG
R+1S8Rf3jAwA7Y4zmoZ3c/MJ7i7nJ7ZZo5varBKKUfaOCyvCHJo6rfXFOnjLBc9t
nsDPhL9muG9BdNbPpg7OSP2vFtKn4WKozVGSbR6T0Vg39cWCJc2JOErr1eXSwsdz
ow7v1kO6MDy+kYsTP/CtCo8lwxkBGfLPzkjG1WlzobcyL1bkbSei6obzIvr6QAzT
fFuCMbNDN1TZ+RudGECfrz/UQ594SLeFoslXcBGjCwKBgQD1EeDAXY3oS4hRkzvT
wLCWvhPmVfc0PL9ce6qn1xeNNHu2QdXUptxzG14EVahZeEDsok4A8vvHPPZdFf4n
PBq2e1jmDUptvTUk9wQK1/Q140O3wfP9Zb4ogrtlB20gipyoflF8ApEf7n9BU5y8
U7OEk2J+AzwUFXHNXiecgQILPwKBgQDEWbrfa6ctV7qnThbXSHYb/dv+kr0W7uz/
wWEQ1qt8ThH4HgdT3M6IaNxt3Htrj30P5WO72T2A2a6UAec8B8xiV+5+R9D+9W7x
pW/Vaf25f4/G/uMEmWsSGhNbzFNnQjAWdFXuRvvKf97yEkFm1kUSbA2fl65SzBis
u2QiKaWlfwKBgFgpDkklXp9qTKfL54HNl7kit9XspvlLwStr8YBfiEFr1/VAycOu
Iy/lcHTuu5k0AWcfHCCLSLfr3lSuTLegj5uF0/0uWtAPeMbLddDQzzFziDDavQMz
Tq0UGoXFniROuPyENJv/8GUkTvMZOREmqzXOL2hVkY9IB6Bxdp5+alXRAoGBAMCu
8YS4xyjm86OlLSL81/LmL1JmO6tasjbVVWTJ1SU6E8Yx6azxfbg9dztUZ8WI3QiR
akr4h7N/ayORrpKpcHd9pOxFm6HnxoTafaGnzraPqM92Z9+mkn0EG8U1AQ/O0xPl
/EHFZOg2jdlt8sJxOP04DjJ0DjzwTrKLfltMFWMPAoGAC1Ci/4SH/s2SquqMNBc7
nACs6Ub3SQu9mfOGFRaAQI0u+rk32Dc2JmnM3naHom37QPEKdrUfptZy5XORKREU
AzktxTCPEyBx+yRRIXBPWDFkPgmjbbNNfwmSeLusXqJtggCEBFUnnW1tpqT+xyW/
fFa5tujWCDnA6vs7f1lKbCY=
-----END PRIVATE KEY-----
";

const RSA_PUBLIC_KEY: &str = r"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAu/eWL0luOrxU4MQfi9WN
x3qxM32v5YTP0y7AMzH3V90N3181E33mrdFvlmA43YvibDxpYufwbLU3M7nsf/ag
FCMzJfJN0uI8fQ+blZ08yaiYTjAqtQ2Xwwc7kkYJAwMxSfVInyoMlq8oA8rd/HFz
OtdQtOlKbIWzXJyaBcLcmKCKCE+7IGRnBmdRj0crE/RTWlBvxJU9udN5HWXwOfM2
5dYEsBuTbaxAavymouAofzsGCSh4bVlXe+NIWCB0+kOP0sr6ViTIf0nT77o6LppM
KOpsDCJLNGlXKvx9lVbzgYx+Z3gh7cQc7HgVbfI2rOGGJIoEXhVVsfyjdRpGp94v
QQIDAQAB
-----END PUBLIC KEY-----
";

const SECRET: &[u8] = b"a-shared-server-secret";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

fn supabase_jwks() -> String {
    serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "alg": "RS256",
            "use": "sig",
            "kid": "supabase-test-key",
            "n": "u_eWL0luOrxU4MQfi9WNx3qxM32v5YTP0y7AMzH3V90N3181E33mrdFvlmA43YvibDxpYufwbLU3M7nsf_agFCMzJfJN0uI8fQ-blZ08yaiYTjAqtQ2Xwwc7kkYJAwMxSfVInyoMlq8oA8rd_HFzOtdQtOlKbIWzXJyaBcLcmKCKCE-7IGRnBmdRj0crE_RTWlBvxJU9udN5HWXwOfM25dYEsBuTbaxAavymouAofzsGCSh4bVlXe-NIWCB0-kOP0sr6ViTIf0nT77o6LppMKOpsDCJLNGlXKvx9lVbzgYx-Z3gh7cQc7HgVbfI2rOGGJIoEXhVVsfyjdRpGp94vQQ",
            "e": "AQAB"
        }]
    })
    .to_string()
}

fn supabase_token(claims: &serde_json::Value) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("supabase-test-key".to_owned());
    jsonwebtoken::encode(
        &header,
        claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(RSA_PRIVATE_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

#[test]
fn accepts_a_valid_token() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    let token = encode_token(SECRET, "tenant-a", "device-1", now() + 3600);
    let identity = authenticator.authenticate(&bearer(&token)).unwrap();
    assert_eq!(identity.tenant_id, "tenant-a");
    assert_eq!(identity.device_id, "device-1");
}

#[test]
fn rejects_expired_token() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    let token = encode_token(SECRET, "tenant-a", "device-1", now() - 10);
    assert!(authenticator.authenticate(&bearer(&token)).is_err());
}

#[test]
fn rejects_a_token_signed_with_another_secret() {
    let authenticator = JwtDeviceAuthenticator::new(b"a-different-secret".to_vec());
    let token = encode_token(SECRET, "tenant-a", "device-1", now() + 3600);
    assert!(authenticator.authenticate(&bearer(&token)).is_err());
}

#[test]
fn rejects_a_tampered_signature() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    let token = encode_token(SECRET, "tenant-a", "device-1", now() + 3600);
    let (head, signature) = token.rsplit_once('.').unwrap();
    let mut chars: Vec<char> = signature.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    let tampered = format!("{head}.{}", chars.into_iter().collect::<String>());
    assert!(authenticator.authenticate(&bearer(&tampered)).is_err());
}

#[test]
fn rejects_a_non_hs256_algorithm() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    // A forged "alg":"none" token must be refused before any signature work.
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = URL_SAFE_NO_PAD.encode(br#"{"tenant_id":"t","device_id":"d","exp":9999999999}"#);
    let forged = format!("{header}.{payload}.");
    assert!(authenticator.authenticate(&bearer(&forged)).is_err());
}

#[test]
fn rejects_missing_or_non_bearer_header() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    assert!(authenticator.authenticate(&HeaderMap::new()).is_err());

    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Token abc".parse().unwrap());
    assert!(authenticator.authenticate(&headers).is_err());
}

#[test]
fn accepts_a_valid_rs256_token() {
    let authenticator = RsaJwtDeviceAuthenticator::from_public_key_pem(RSA_PUBLIC_KEY).unwrap();
    let token =
        encode_token_rs256(RSA_PRIVATE_KEY, "tenant-rsa", "device-9", now() + 3600).unwrap();
    let identity = authenticator.authenticate(&bearer(&token)).unwrap();
    assert_eq!(identity.tenant_id, "tenant-rsa");
    assert_eq!(identity.device_id, "device-9");
}

#[test]
fn rejects_expired_rs256_token() {
    let authenticator = RsaJwtDeviceAuthenticator::from_public_key_pem(RSA_PUBLIC_KEY).unwrap();
    let token = encode_token_rs256(RSA_PRIVATE_KEY, "tenant-rsa", "device-9", now() - 10).unwrap();
    assert!(authenticator.authenticate(&bearer(&token)).is_err());
}

#[test]
fn rejects_tampered_rs256_signature() {
    let authenticator = RsaJwtDeviceAuthenticator::from_public_key_pem(RSA_PUBLIC_KEY).unwrap();
    let token =
        encode_token_rs256(RSA_PRIVATE_KEY, "tenant-rsa", "device-9", now() + 3600).unwrap();
    let (head, signature) = token.rsplit_once('.').unwrap();
    let mut chars: Vec<char> = signature.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    let tampered = format!("{head}.{}", chars.into_iter().collect::<String>());
    assert!(authenticator.authenticate(&bearer(&tampered)).is_err());
}

#[test]
fn rejects_algorithm_confusion_between_hs256_and_rs256() {
    let rsa = RsaJwtDeviceAuthenticator::from_public_key_pem(RSA_PUBLIC_KEY).unwrap();
    let hmac = JwtDeviceAuthenticator::new(SECRET);

    // An HS256 token must not be accepted by the RS256 verifier.
    let hs_token = encode_token(SECRET, "tenant-a", "device-1", now() + 3600);
    assert!(rsa.authenticate(&bearer(&hs_token)).is_err());

    // An RS256 token must not be accepted by the HS256 verifier.
    let rs_token =
        encode_token_rs256(RSA_PRIVATE_KEY, "tenant-rsa", "device-9", now() + 3600).unwrap();
    assert!(hmac.authenticate(&bearer(&rs_token)).is_err());
}

#[test]
fn enforces_audience_claim_when_required() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET).with_audience("loomabase-api");

    let good = encode_token_with_claims(
        SECRET,
        "tenant-a",
        "device-1",
        now() + 3600,
        None,
        Some("loomabase-api"),
        None,
    );
    assert!(authenticator.authenticate(&bearer(&good)).is_ok());

    // Wrong audience.
    let wrong = encode_token_with_claims(
        SECRET,
        "tenant-a",
        "device-1",
        now() + 3600,
        None,
        Some("other-api"),
        None,
    );
    assert!(authenticator.authenticate(&bearer(&wrong)).is_err());

    // Missing audience entirely.
    let missing = encode_token(SECRET, "tenant-a", "device-1", now() + 3600);
    assert!(authenticator.authenticate(&bearer(&missing)).is_err());
}

#[test]
fn enforces_issuer_claim_when_required() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET).with_issuer("https://issuer.example");

    let good = encode_token_with_claims(
        SECRET,
        "tenant-a",
        "device-1",
        now() + 3600,
        None,
        None,
        Some("https://issuer.example"),
    );
    assert!(authenticator.authenticate(&bearer(&good)).is_ok());

    let wrong = encode_token_with_claims(
        SECRET,
        "tenant-a",
        "device-1",
        now() + 3600,
        None,
        None,
        Some("https://evil.example"),
    );
    assert!(authenticator.authenticate(&bearer(&wrong)).is_err());
}

#[test]
fn rejects_a_token_used_before_its_nbf() {
    let authenticator = JwtDeviceAuthenticator::new(SECRET);
    let not_yet = encode_token_with_claims(
        SECRET,
        "tenant-a",
        "device-1",
        now() + 3600,
        Some(now() + 1000),
        None,
        None,
    );
    assert!(authenticator.authenticate(&bearer(&not_yet)).is_err());
}

#[test]
fn accepts_supabase_jwks_tokens_and_maps_tenant_device_and_authorization() {
    let issuer = "https://project.supabase.co/auth/v1";
    let authenticator = SupabaseJwtAuthenticator::from_jwks_json(&supabase_jwks(), issuer).unwrap();
    let token = supabase_token(&serde_json::json!({
        "sub": "user-123",
        "iss": issuer,
        "aud": "authenticated",
        "exp": now() + 3600,
        "app_metadata": {
            "tenant_id": "workspace-a",
            "loomabase_tables": ["todos"]
        }
    }));
    let mut headers = bearer(&token);
    headers.insert("x-device-id", "phone-1".parse().unwrap());
    let identity = authenticator.authenticate(&headers).unwrap();
    assert_eq!(identity.tenant_id, "workspace-a");
    assert_eq!(identity.device_id, "user-123/phone-1");
    assert!(identity.can_sync_table("todos"));
    assert!(!identity.can_sync_table("notes"));

    // A failed refresh must not discard the last trusted key set.
    assert!(authenticator.replace_jwks_json("{invalid").is_err());
    assert!(authenticator.authenticate(&headers).is_ok());
}

#[test]
fn supabase_auth_falls_back_to_per_user_tenant_and_requires_a_device() {
    let issuer = "https://project.supabase.co/auth/v1";
    let authenticator = SupabaseJwtAuthenticator::from_jwks_json(&supabase_jwks(), issuer).unwrap();
    let token = supabase_token(&serde_json::json!({
        "sub": "user-456",
        "iss": issuer,
        "aud": "authenticated",
        "exp": now() + 3600,
        "user_metadata": {
            "tenant_id": "attacker-tenant",
            "loomabase_tables": []
        }
    }));
    assert!(authenticator.authenticate(&bearer(&token)).is_err());

    let mut headers = bearer(&token);
    headers.insert("x-device-id", "laptop".parse().unwrap());
    let identity = authenticator.authenticate(&headers).unwrap();
    assert_eq!(identity.tenant_id, "user-456");
    assert_eq!(identity.device_id, "user-456/laptop");
    assert!(identity.can_sync_table("any_valid_table"));
}
