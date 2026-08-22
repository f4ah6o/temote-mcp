//! Cloudflare Access JWT authentication for the public HTTP endpoint.
//!
//! Cloudflare Access terminates Managed OAuth at the edge and forwards a
//! signed Cf-Access-Jwt-Assertion header to the origin. The origin still
//! validates the signature, issuer, audience, and the configured email allow
//! list before dispatching any MCP operation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use url::Url;

const ACCESS_ASSERTION_HEADER: &str = "cf-access-jwt-assertion";
const JWKS_PATH: &str = "/cdn-cgi/access/certs";

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

fn jwt_validation(audience: &str) -> Validation {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = 0;
    validation.set_audience(&[audience]);
    validation
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

        let header = decode_header(token).context("invalid Cloudflare Access JWT header")?;
        anyhow::ensure!(
            header.alg == Algorithm::RS256,
            "Cloudflare Access JWT must use RS256"
        );
        let kid = header.kid.context("Cloudflare Access JWT has no key ID")?;
        let key = self.key_for(&kid).await?;
        let validation = jwt_validation(&self.config.audience);
        let claims = decode::<Value>(token, &key, &validation)
            .context("Cloudflare Access JWT signature or expiry is invalid")?
            .claims;

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

    async fn key_for(&self, kid: &str) -> Result<DecodingKey> {
        if let Some(key) = self.find_key(kid).await {
            return rsa_key(&key);
        }
        self.refresh_keys().await?;
        let key = self
            .find_key(kid)
            .await
            .with_context(|| format!("Cloudflare Access key {kid} was not found"))?;
        rsa_key(&key)
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
        let bytes = response
            .bytes()
            .await
            .context("failed to read Cloudflare Access signing-key response")?;
        anyhow::ensure!(
            bytes.len() <= 1024 * 1024,
            "Cloudflare Access signing-key response is too large"
        );
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

fn rsa_key(key: &JsonWebKey) -> Result<DecodingKey> {
    anyhow::ensure!(
        key.kty.as_deref() == Some("RSA"),
        "Access signing key is not RSA"
    );
    anyhow::ensure!(
        key.alg.as_deref().is_none_or(|alg| alg == "RS256"),
        "Access signing key algorithm is not RS256"
    );
    DecodingKey::from_rsa_components(&key.n, &key.e)
        .context("invalid Cloudflare Access RSA signing key")
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
    fn configures_the_expected_audience_for_jwt_validation() {
        let validation = jwt_validation("self-hosted-audience");
        assert!(validation.validate_aud);
        assert_eq!(
            validation.aud,
            Some(["self-hosted-audience".to_owned()].into_iter().collect())
        );
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
