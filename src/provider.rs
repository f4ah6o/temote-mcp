use anyhow::Result;
use axum::http::HeaderMap;
use url::Url;

use crate::access::AccessAuthenticator;
use crate::local_oauth::LocalOAuth;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicEndpoint(String);

impl PublicEndpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let parsed = Url::parse(value.trim())
            .map_err(|error| anyhow::anyhow!("public endpoint is invalid: {error}"))?;
        anyhow::ensure!(
            parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.path().trim_matches('/').is_empty(),
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
