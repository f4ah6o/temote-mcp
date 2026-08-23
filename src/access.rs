//! Cloudflare Access JWT authentication for the public HTTP endpoint.
//!
//! Cloudflare Access terminates Managed OAuth at the edge and forwards a
//! signed Cf-Access-Jwt-Assertion header to the origin. The origin still
//! validates the signature, issuer, audience, and the configured email allow
//! list before dispatching any MCP operation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

const ACCESS_ASSERTION_HEADER: &str = "cf-access-jwt-assertion";
const JWKS_PATH: &str = "/cdn-cgi/access/certs";
const MAX_JWKS_BYTES: usize = 1024 * 1024;
const MAX_ACCESS_ASSERTION_BYTES: usize = 64 * 1024;
const MAX_ACCESS_HEADER_SEGMENT_BYTES: usize = 8 * 1024;
const MAX_ACCESS_CLAIMS_SEGMENT_BYTES: usize = 32 * 1024;
const MAX_ACCESS_SIGNATURE_SEGMENT_BYTES: usize = 8 * 1024;
const MAX_ACCESS_KID_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub struct AccessConfig {
    pub team_domain: String,
    pub audience: String,
    pub allowed_emails: HashSet<String>,
    jwks_url: String,
}

impl AccessConfig {
    pub fn from_env() -> Result<Self> {
        let team_domain = required_env("TEMOTE_MCP_ACCESS_TEAM_DOMAIN")?;
        let audience = required_env("TEMOTE_MCP_ACCESS_AUDIENCE")?
            .trim()
            .to_owned();
        anyhow::ensure!(
            !audience.is_empty(),
            "TEMOTE_MCP_ACCESS_AUDIENCE must not be empty"
        );
        let allowed_emails = required_env("TEMOTE_MCP_ACCESS_ALLOWED_EMAILS")?
            .split(',')
            .map(|email| email.trim().to_ascii_lowercase())
            .filter(|email| !email.is_empty())
            .collect::<HashSet<_>>();

        anyhow::ensure!(
            !allowed_emails.is_empty(),
            "TEMOTE_MCP_ACCESS_ALLOWED_EMAILS must contain at least one email"
        );

        let team_domain = normalize_team_domain(&team_domain)?;
        let jwks_url = format!("{team_domain}{JWKS_PATH}");
        Ok(Self {
            team_domain,
            audience: audience.trim().to_owned(),
            allowed_emails,
            jwks_url,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessIdentity {
    pub email: String,
    pub subject: String,
}

#[derive(Clone)]
pub struct AccessAuthenticator {
    config: Arc<AccessConfig>,
    client: Client,
    keys: Arc<RwLock<Option<JwkSet>>>,
    #[cfg(test)]
    test_token: Option<String>,
    #[cfg(test)]
    test_identity: Option<AccessIdentity>,
}

#[derive(Clone, Debug, Deserialize)]
struct JwkSet {
    keys: Vec<JsonWebKey>,
}

#[derive(Clone, Debug, Deserialize)]
struct JsonWebKey {
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    kty: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    n: String,
    e: String,
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

impl AccessAuthenticator {
    pub async fn from_env() -> Result<Self> {
        let config = Arc::new(AccessConfig::from_env()?);
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(format!("temote-mcp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create the Cloudflare Access HTTP client")?;
        let authenticator = Self {
            config,
            client,
            keys: Arc::new(RwLock::new(None)),
            #[cfg(test)]
            test_token: None,
            #[cfg(test)]
            test_identity: None,
        };
        authenticator.refresh_keys().await?;
        Ok(authenticator)
    }

    pub async fn authenticate(&self, headers: &axum::http::HeaderMap) -> Result<AccessIdentity> {
        let token = headers
            .get(ACCESS_ASSERTION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("missing Cf-Access-Jwt-Assertion header")?;

        #[cfg(test)]
        if self.test_token.as_deref() == Some(token) {
            return self
                .test_identity
                .clone()
                .context("test authenticator has no identity");
        }

        validate_access_assertion_shape(token)?;
        let (header_segment, claims_segment, signature_segment) = access_jwt_parts(token)?;
        let header_bytes = URL_SAFE_NO_PAD
            .decode(header_segment)
            .context("invalid Cloudflare Access JWT header encoding")?;
        let header: JwtHeader = serde_json::from_slice(&header_bytes)
            .context("invalid Cloudflare Access JWT header")?;
        anyhow::ensure!(
            header.alg == "RS256",
            "Cloudflare Access JWT must use RS256"
        );
        let kid = header.kid.context("Cloudflare Access JWT has no key ID")?;
        anyhow::ensure!(
            valid_access_kid(&kid),
            "Cloudflare Access JWT key ID is invalid"
        );
        let key = self.key_for(&kid).await?;
        let signing_input_len = header_segment.len() + 1 + claims_segment.len();
        verify_rs256(
            &key,
            &token.as_bytes()[..signing_input_len],
            signature_segment,
        )?;
        let claims_bytes = URL_SAFE_NO_PAD
            .decode(claims_segment)
            .context("invalid Cloudflare Access JWT claims encoding")?;
        let claims: Value = serde_json::from_slice(&claims_bytes)
            .context("invalid Cloudflare Access JWT claims")?;
        anyhow::ensure!(
            claims.is_object(),
            "Cloudflare Access JWT claims must be an object"
        );
        validate_access_times(&claims, current_unix_timestamp()?)?;

        let issuer = claims.get("iss").and_then(Value::as_str);
        anyhow::ensure!(
            issuer == Some(self.config.team_domain.as_str()),
            "Cloudflare Access JWT issuer is invalid"
        );
        anyhow::ensure!(
            audience_matches(claims.get("aud"), &self.config.audience),
            "Cloudflare Access JWT audience is invalid"
        );

        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("Cloudflare Access JWT has no subject")?;
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("Cloudflare Access JWT has no email")?
            .to_ascii_lowercase();
        anyhow::ensure!(
            self.config.allowed_emails.contains(&email),
            "Cloudflare Access email is not allowed"
        );

        Ok(AccessIdentity {
            email,
            subject: subject.to_owned(),
        })
    }

    async fn key_for(&self, kid: &str) -> Result<JsonWebKey> {
        if let Some(key) = self.find_key(kid).await {
            return Ok(key);
        }
        self.refresh_keys().await?;
        self.find_key(kid)
            .await
            .with_context(|| format!("Cloudflare Access key {kid} was not found"))
    }

    async fn find_key(&self, kid: &str) -> Option<JsonWebKey> {
        self.keys
            .read()
            .await
            .as_ref()?
            .keys
            .iter()
            .find(|key| key.kid.as_deref() == Some(kid))
            .cloned()
    }

    async fn refresh_keys(&self) -> Result<()> {
        let response = self
            .client
            .get(&self.config.jwks_url)
            .send()
            .await
            .context("failed to fetch Cloudflare Access signing keys")?
            .error_for_status()
            .context("Cloudflare Access signing-key endpoint returned an error")?;
        let bytes = read_bounded_jwks_response(response).await?;
        let set: JwkSet = serde_json::from_slice(&bytes)
            .context("invalid Cloudflare Access signing-key response")?;
        anyhow::ensure!(
            !set.keys.is_empty(),
            "Cloudflare Access signing-key response contained no keys"
        );
        *self.keys.write().await = Some(set);
        Ok(())
    }

    #[cfg(test)]
    pub fn test(token: &str, identity: AccessIdentity) -> Self {
        let config = AccessConfig {
            team_domain: "https://team.example.cloudflareaccess.com".to_owned(),
            audience: "test-audience".to_owned(),
            allowed_emails: [identity.email.clone()].into_iter().collect(),
            jwks_url: "https://team.example.cloudflareaccess.com/certs".to_owned(),
        };
        Self {
            config: Arc::new(config),
            client: Client::new(),
            keys: Arc::new(RwLock::new(None)),
            test_token: Some(token.to_owned()),
            test_identity: Some(identity),
        }
    }
}

async fn read_bounded_jwks_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_JWKS_BYTES as u64,
            "Cloudflare Access signing-key response is too large"
        );
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_JWKS_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read Cloudflare Access signing-key response")?
    {
        append_bounded_jwks_chunk(&mut bytes, &chunk)?;
    }
    Ok(bytes)
}

fn append_bounded_jwks_chunk(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    let next = bytes
        .len()
        .checked_add(chunk.len())
        .context("Cloudflare Access signing-key response size overflow")?;
    anyhow::ensure!(
        next <= MAX_JWKS_BYTES,
        "Cloudflare Access signing-key response is too large"
    );
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn verify_rs256(key: &JsonWebKey, signing_input: &[u8], signature_segment: &str) -> Result<()> {
    anyhow::ensure!(
        key.kty.as_deref() == Some("RSA"),
        "Access signing key is not RSA"
    );
    anyhow::ensure!(
        key.alg.as_deref().is_none_or(|alg| alg == "RS256"),
        "Access signing key algorithm is not RS256"
    );
    let modulus = URL_SAFE_NO_PAD
        .decode(&key.n)
        .context("invalid Cloudflare Access RSA modulus")?;
    let exponent = URL_SAFE_NO_PAD
        .decode(&key.e)
        .context("invalid Cloudflare Access RSA exponent")?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_segment)
        .context("invalid Cloudflare Access JWT signature encoding")?;
    anyhow::ensure!(
        !modulus.is_empty() && !exponent.is_empty(),
        "invalid Cloudflare Access RSA signing key"
    );
    RsaPublicKeyComponents {
        n: &modulus,
        e: &exponent,
    }
    .verify(&RSA_PKCS1_2048_8192_SHA256, signing_input, &signature)
    .map_err(|_| anyhow::anyhow!("Cloudflare Access JWT signature is invalid"))
}

fn access_jwt_parts(token: &str) -> Result<(&str, &str, &str)> {
    let mut parts = token.split('.');
    let header = parts.next().unwrap_or_default();
    let claims = parts.next().unwrap_or_default();
    let signature = parts.next().unwrap_or_default();
    anyhow::ensure!(parts.next().is_none(), "Cloudflare Access JWT is malformed");
    Ok((header, claims, signature))
}

fn numeric_date(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(integer) = value.as_u64() {
        return Some(integer);
    }
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0 && number < u64::MAX as f64).then(|| number.round() as u64)
}

fn validate_access_times(claims: &Value, now: u64) -> Result<()> {
    let expires =
        numeric_date(claims.get("exp")).context("Cloudflare Access JWT has no valid expiry")?;
    anyhow::ensure!(expires >= now, "Cloudflare Access JWT is expired");
    if let Some(not_before) = numeric_date(claims.get("nbf")) {
        anyhow::ensure!(not_before <= now, "Cloudflare Access JWT is not valid yet");
    }
    Ok(())
}

fn current_unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

fn valid_access_kid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ACCESS_KID_BYTES
        && value.chars().all(|character| !character.is_control())
}

fn valid_access_jwt_segment(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_access_assertion_shape(token: &str) -> Result<()> {
    anyhow::ensure!(
        token.len() <= MAX_ACCESS_ASSERTION_BYTES,
        "Cloudflare Access JWT is too large"
    );
    let mut parts = token.split('.');
    let header = parts.next().unwrap_or_default();
    let claims = parts.next().unwrap_or_default();
    let signature = parts.next().unwrap_or_default();
    anyhow::ensure!(parts.next().is_none(), "Cloudflare Access JWT is malformed");
    anyhow::ensure!(
        valid_access_jwt_segment(header, MAX_ACCESS_HEADER_SEGMENT_BYTES)
            && valid_access_jwt_segment(claims, MAX_ACCESS_CLAIMS_SEGMENT_BYTES)
            && valid_access_jwt_segment(signature, MAX_ACCESS_SIGNATURE_SEGMENT_BYTES),
        "Cloudflare Access JWT is malformed"
    );
    Ok(())
}

fn audience_matches(value: Option<&Value>, expected: &str) -> bool {
    value.is_some_and(|value| {
        value.as_str() == Some(expected)
            || value
                .as_array()
                .is_some_and(|audiences| audiences.iter().any(|aud| aud.as_str() == Some(expected)))
    })
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    anyhow::ensure!(!value.trim().is_empty(), "{name} must not be empty");
    Ok(value)
}

fn normalize_team_domain(value: &str) -> Result<String> {
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let parsed = Url::parse(&candidate).context("TEMOTE_MCP_ACCESS_TEAM_DOMAIN is invalid")?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().trim_matches('/').is_empty(),
        "TEMOTE_MCP_ACCESS_TEAM_DOMAIN must be an HTTPS host without a path"
    );
    anyhow::ensure!(
        parsed.host_str().is_some(),
        "Access team domain has no host"
    );
    Ok(parsed
        .origin()
        .ascii_serialization()
        .trim_end_matches('/')
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn normalizes_access_team_domain() {
        assert_eq!(
            normalize_team_domain("team.example.cloudflareaccess.com/").unwrap(),
            "https://team.example.cloudflareaccess.com"
        );
        assert!(normalize_team_domain("http://team.example").is_err());
        assert!(normalize_team_domain("https://team.example/path").is_err());
    }

    #[test]
    fn accepts_string_and_array_audiences() {
        assert!(audience_matches(
            Some(&Value::String("aud".to_owned())),
            "aud"
        ));
        assert!(audience_matches(
            Some(&serde_json::json!(["other", "aud"])),
            "aud"
        ));
        assert!(!audience_matches(
            Some(&serde_json::json!(["other"])),
            "aud"
        ));
    }

    #[test]
    fn rs256_fixture_accepts_valid_signature_and_rejects_mutation() {
        const MODULUS: &str = "rJPosGzIUEOuDHwYF2gvUO_Yt5CkpRApV_dHgfCztDhN5nMvF4v86tszOALCeeN-FnMO3Ys5XhYoaJ4B3mPdqfBH3S134yhU7vQhWphIMWdCd_KjHrVGHfrrk2DgkkNddLrzP6zjqQ3LNSepIlYE_TSolwIET927Kw7eFoLNikI7VB5pMYIGSo_CjaK4iz5tRF_eBiHpHAGH8j-UcJcFhvLayNB-NEB0qDuigfpf2YUmxq6XWqZ94JRzGXYwSKTRL_0kV-bvt339e_KShb5xAgnKDhfeDhgc6IQWS6v_wa2WngzNA_AaXRlOilFOWnnOEzuUjcCijVVn5ZSikAG-CQ";
        const HEADER: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3Qta2V5In0";
        const CLAIMS: &str = "eyJleHAiOjQxMDI0NDQ4MDAsInN1YiI6ImZpeHR1cmUifQ";
        const SIGNATURE: &str = "T1cpTaIQSjpDzd0HxEdjwOauRxRQx23rnt7MvozroXRNNJpF60uff0hK1L3wBC-wbNC0iv_dY0RIceuvqu8P9L60GOlh_-KpO7-n1pgRYyPKZm6G0E63Vldd46UTPrsfn9RtTPyN-rTFMY-8_BXx7kM05eo9pOsdHl0IPrBBZ1Q9p6SFPG9GrD-FLlnWg42ANw2vJljQzTVD6TmaqHGcSGVcaQf2hAwsCC-3ExlxKvPyH4ZyWraObOlDsZFMmSLlQtGTxR6fZPiDJRcsnVCgy3BY59i2QS8qAU0bNwsd5diz93cVGr7dIhZaLRq0NUglFHKi4XBlJFyOa6BvKlFcww";
        let key = JsonWebKey {
            kid: Some("test-key".to_owned()),
            kty: Some("RSA".to_owned()),
            alg: Some("RS256".to_owned()),
            n: MODULUS.to_owned(),
            e: "AQAB".to_owned(),
        };
        let signing_input = format!("{HEADER}.{CLAIMS}");
        assert!(verify_rs256(&key, signing_input.as_bytes(), SIGNATURE).is_ok());

        let mut mutated_input = signing_input.into_bytes();
        let last = mutated_input.last_mut().expect("fixture signing input");
        *last = if *last == b'A' { b'B' } else { b'A' };
        assert!(verify_rs256(&key, &mutated_input, SIGNATURE).is_err());

        let mut mutated_signature = SIGNATURE.as_bytes().to_vec();
        let first = mutated_signature.first_mut().expect("fixture signature");
        *first = if *first == b'A' { b'B' } else { b'A' };
        let mutated_signature = String::from_utf8(mutated_signature).unwrap();
        assert!(
            verify_rs256(
                &key,
                format!("{HEADER}.{CLAIMS}").as_bytes(),
                &mutated_signature
            )
            .is_err()
        );
    }

    #[test]
    fn generated_access_time_validation_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_5449, 1024, |ctx| {
            let now = noprop::sample_u64_in(ctx, 1..=4_000_000_000);
            let exp_case = noprop::sample_usize_in(ctx, 0..=5);
            let nbf_case = noprop::sample_usize_in(ctx, 0..=4);
            let mut claims = serde_json::Map::new();

            let exp_ok = match exp_case {
                0 => false,
                1 => {
                    claims.insert("exp".to_owned(), serde_json::json!(now - 1));
                    false
                }
                2 => {
                    claims.insert("exp".to_owned(), serde_json::json!(now));
                    true
                }
                3 => {
                    claims.insert("exp".to_owned(), serde_json::json!(now + 1));
                    true
                }
                4 => {
                    claims.insert("exp".to_owned(), Value::String(now.to_string()));
                    false
                }
                _ => {
                    claims.insert("exp".to_owned(), serde_json::json!(now as f64 + 0.6));
                    true
                }
            };

            let nbf_ok = match nbf_case {
                0 => true,
                1 => {
                    claims.insert("nbf".to_owned(), serde_json::json!(now - 1));
                    true
                }
                2 => {
                    claims.insert("nbf".to_owned(), serde_json::json!(now));
                    true
                }
                3 => {
                    claims.insert("nbf".to_owned(), serde_json::json!(now + 1));
                    false
                }
                _ => {
                    claims.insert("nbf".to_owned(), Value::String("invalid".to_owned()));
                    true
                }
            };

            let claims = Value::Object(claims);
            assert_eq!(
                validate_access_times(&claims, now).is_ok(),
                exp_ok && nbf_ok
            );
            Ok(())
        })
    }

    #[test]
    fn generated_audiences_match_reference_model() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_4155, test_support::DEFAULT_CASES, |ctx| {
            let expected = test_support::safe_component(ctx);
            let value = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => None,
                1 => Some(Value::String(expected.clone())),
                2 => Some(Value::String(test_support::safe_component(ctx))),
                3 => Some(serde_json::json!([
                    test_support::safe_component(ctx),
                    expected.clone(),
                    noprop::sample_u64(ctx)
                ])),
                _ => Some(serde_json::json!({"aud": expected.clone()})),
            };
            let model = value.as_ref().is_some_and(|value| {
                value.as_str() == Some(expected.as_str())
                    || value.as_array().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item.as_str() == Some(expected.as_str()))
                    })
            });
            assert_eq!(audience_matches(value.as_ref(), &expected), model);
            Ok(())
        })
    }

    #[test]
    fn generated_access_assertion_shape_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_4a54, 512, |ctx| {
            let header_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_ACCESS_HEADER_SEGMENT_BYTES - 1,
                3 => MAX_ACCESS_HEADER_SEGMENT_BYTES,
                _ => MAX_ACCESS_HEADER_SEGMENT_BYTES + 1,
            };
            let claims_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_ACCESS_CLAIMS_SEGMENT_BYTES - 1,
                3 => MAX_ACCESS_CLAIMS_SEGMENT_BYTES,
                _ => MAX_ACCESS_CLAIMS_SEGMENT_BYTES + 1,
            };
            let signature_len = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_ACCESS_SIGNATURE_SEGMENT_BYTES - 1,
                3 => MAX_ACCESS_SIGNATURE_SEGMENT_BYTES,
                _ => MAX_ACCESS_SIGNATURE_SEGMENT_BYTES + 1,
            };
            let mut header = "A".repeat(header_len);
            let mut claims = "B".repeat(claims_len);
            let mut signature = "C".repeat(signature_len);
            match noprop::sample_usize_in(ctx, 0..=4) {
                1 => header.push('='),
                2 => claims.push('+'),
                3 => signature.push('/'),
                _ => {}
            }
            let extra_part = noprop::sample_bool(ctx);
            let mut token = format!("{header}.{claims}.{signature}");
            if extra_part {
                token.push_str(".extra");
            }
            let valid_segment = |value: &str, max_bytes: usize| {
                !value.is_empty()
                    && value.len() <= max_bytes
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            };
            let model = token.len() <= MAX_ACCESS_ASSERTION_BYTES
                && !extra_part
                && valid_segment(&header, MAX_ACCESS_HEADER_SEGMENT_BYTES)
                && valid_segment(&claims, MAX_ACCESS_CLAIMS_SEGMENT_BYTES)
                && valid_segment(&signature, MAX_ACCESS_SIGNATURE_SEGMENT_BYTES);
            assert_eq!(
                validate_access_assertion_shape(&token).is_ok(),
                model,
                "header={} claims={} signature={} extra={extra_part}",
                header.len(),
                claims.len(),
                signature.len()
            );
            Ok(())
        })
    }

    #[test]
    fn oversized_access_assertions_fail_before_jwt_decode() {
        let token = format!("{}.e30.c2ln", "A".repeat(MAX_ACCESS_ASSERTION_BYTES));
        let error = validate_access_assertion_shape(&token)
            .unwrap_err()
            .to_string();
        assert!(error.contains("too large"));
    }

    #[test]
    fn generated_access_key_ids_match_length_policy() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_4b49, 512, |ctx| {
            let length = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => 0,
                1 => 1,
                2 => MAX_ACCESS_KID_BYTES - 1,
                3 => MAX_ACCESS_KID_BYTES,
                _ => MAX_ACCESS_KID_BYTES + 1,
            };
            let mut kid = "k".repeat(length);
            let has_control = noprop::sample_bool(ctx) && !kid.is_empty();
            if has_control {
                kid.replace_range(0..1, "\n");
            }
            assert_eq!(
                valid_access_kid(&kid),
                length > 0 && length <= MAX_ACCESS_KID_BYTES && !has_control
            );
            Ok(())
        })
    }

    #[test]
    fn generated_jwks_chunk_budget_never_overreads() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_4a57, 512, |ctx| {
            let start = if noprop::sample_bool(ctx) {
                noprop::sample_usize_in(ctx, 0..=2048)
            } else {
                MAX_JWKS_BYTES - noprop::sample_usize_in(ctx, 0..=2048)
            };
            let chunk_len = noprop::sample_usize_in(ctx, 0..=4096);
            let mut bytes = vec![0_u8; start];
            let chunk = vec![noprop::sample_u8(ctx); chunk_len];
            let expected = start
                .checked_add(chunk_len)
                .is_some_and(|next| next <= MAX_JWKS_BYTES);
            let result = append_bounded_jwks_chunk(&mut bytes, &chunk);
            assert_eq!(result.is_ok(), expected);
            assert_eq!(
                bytes.len(),
                if expected { start + chunk_len } else { start }
            );
            assert!(bytes.len() <= MAX_JWKS_BYTES);
            Ok(())
        })
    }

    #[test]
    fn generated_team_domains_accept_only_https_origins() -> noprop::TestResult {
        test_support::run(0x4143_4345_5353_5552, 512, |ctx| {
            let host = format!("{}.example.test", test_support::safe_component(ctx));
            let safe = match noprop::sample_usize_in(ctx, 0..=2) {
                0 => host.clone(),
                1 => format!("https://{host}"),
                _ => format!("https://{host}/"),
            };
            assert_eq!(
                normalize_team_domain(&safe).unwrap(),
                format!("https://{host}")
            );

            let unsafe_value = match noprop::sample_usize_in(ctx, 0..=4) {
                0 => format!("http://{host}"),
                1 => format!("https://{host}/path"),
                2 => format!("https://{host}?q=1"),
                3 => format!("https://{host}#fragment"),
                _ => format!("https://user@{host}"),
            };
            assert!(
                normalize_team_domain(&unsafe_value).is_err(),
                "accepted {unsafe_value:?}"
            );
            Ok(())
        })
    }
}
