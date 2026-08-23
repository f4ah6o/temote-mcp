use anyhow::Result;
use axum::http::HeaderMap;
use url::Url;

use crate::access::AccessAuthenticator;
use crate::local_oauth::LocalOAuth;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicEndpoint(String);

impl PublicEndpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let Some((scheme, authority_and_path)) = value.split_once("://") else {
            anyhow::bail!("public endpoint must be an HTTPS origin without a path");
        };
        anyhow::ensure!(
            scheme.eq_ignore_ascii_case("https") && !authority_and_path.starts_with('/'),
            "public endpoint must be an HTTPS origin without a path"
        );

        let parsed = Url::parse(value)
            .map_err(|error| anyhow::anyhow!("public endpoint is invalid: {error}"))?;
        anyhow::ensure!(
            parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.path() == "/",
            "public endpoint must be an HTTPS origin without a path"
        );
        anyhow::ensure!(parsed.host_str().is_some(), "public endpoint has no host");
        Ok(Self(
            parsed
                .origin()
                .ascii_serialization()
                .trim_end_matches('/')
                .to_owned(),
        ))
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub provider: &'static str,
    pub subject: Option<String>,
    pub display_principal: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone)]
pub enum AuthProvider {
    Cloudflare(AccessAuthenticator),
    Local(LocalOAuth),
    OpenAiTunnel,
}

impl AuthProvider {
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Identity> {
        match self {
            Self::Cloudflare(authenticator) => {
                let identity = authenticator.authenticate(headers).await?;
                Ok(Identity {
                    provider: "cloudflare-access",
                    subject: Some(identity.subject),
                    display_principal: Some(identity.email.clone()),
                    email: Some(identity.email),
                })
            }
            Self::Local(authenticator) => authenticator.authenticate(headers).await,
            Self::OpenAiTunnel => Ok(Identity {
                provider: "openai-secure-mcp-tunnel",
                subject: None,
                display_principal: None,
                email: None,
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Cloudflare(_) => "Cloudflare Access Managed OAuth",
            Self::Local(_) => "Temote local OAuth",
            Self::OpenAiTunnel => "OpenAI Secure MCP Tunnel",
        }
    }

    pub fn local_oauth(&self) -> Option<&LocalOAuth> {
        match self {
            Self::Cloudflare(_) | Self::OpenAiTunnel => None,
            Self::Local(local) => Some(local),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_public_endpoints_match_https_origin_policy() -> noprop::TestResult {
        test_support::run(0x5052_4f56_454e_4450, test_support::DEFAULT_CASES, |ctx| {
            let host = format!("{}.example.invalid", test_support::safe_component(ctx));
            let port = noprop::sample_usize_in(ctx, 1024..=65535);
            let mode = noprop::sample_usize_in(ctx, 0..=10);
            let (candidate, expected) = match mode {
                0 => (format!("https://{host}"), true),
                1 => (format!("  https://{host}/  "), true),
                2 => (format!("https://{host}:{port}"), true),
                3 => (format!("http://{host}"), false),
                4 => (format!("https://{host}/mcp"), false),
                5 => (format!("https://{host}//"), false),
                6 => (format!("https://{host}?query=1"), false),
                7 => (format!("https://{host}#fragment"), false),
                8 => (format!("https://user@{host}"), false),
                9 => (format!("https://user:password@{host}"), false),
                _ => ("https:///missing-host".to_owned(), false),
            };

            let result = PublicEndpoint::parse(&candidate);
            assert_eq!(
                result.is_ok(),
                expected,
                "candidate={candidate:?}, result={result:?}"
            );
            if let Ok(endpoint) = result {
                let canonical = endpoint.into_string();
                assert_eq!(
                    PublicEndpoint::parse(&canonical).unwrap().into_string(),
                    canonical
                );
            }
            Ok(())
        })
    }
}
