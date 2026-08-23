use clap::ValueEnum;

/// Production-supported ingress/auth combinations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    /// Cloudflare Tunnel + Cloudflare Access Managed OAuth.
    #[default]
    Cloudflare,
    /// Tailscale Funnel + Temote local OAuth.
    Tailscale,
    /// OpenAI Secure MCP Tunnel over an outbound-only private connection.
    Openai,
}

impl Profile {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cloudflare => "cloudflare",
            Self::Tailscale => "tailscale",
            Self::Openai => "openai",
        }
    }

    pub const fn ingress_name(self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare Tunnel",
            Self::Tailscale => "Tailscale Funnel",
            Self::Openai => "OpenAI Secure MCP Tunnel",
        }
    }

    pub const fn auth_name(self) -> &'static str {
        match self {
            Self::Cloudflare => "Cloudflare Access Managed OAuth",
            Self::Tailscale => "Temote local OAuth",
            Self::Openai => "OpenAI tunnel connection",
        }
    }
}
