mod approvals;
mod config;
mod http;
mod mcp;
mod oauth;
mod sandbox;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session in the current directory and show its permission UI.
    Start {
        /// Session ID to use instead of generating a UUID.
        session_id: Option<String>,
    },
    /// Run the session-independent MCP server over stdin/stdout.
    Mcp,
    /// Run the session-independent MCP server over HTTP with OAuth.
    Serve {
        /// Public HTTPS base URL clients reach this server through.
        #[arg(long)]
        public_url: String,
        /// Local address to listen on.
        #[arg(long, default_value = "127.0.0.1:8791")]
        addr: SocketAddr,
        /// Owner-approval token for the authorization page. Defaults to
        /// LOCAL_MCP_OAUTH_ADMIN_TOKEN, or a generated token kept in the state
        /// directory.
        #[arg(long, env = "LOCAL_MCP_OAUTH_ADMIN_TOKEN")]
        admin_token: Option<String>,
        /// Additional redirect URI accepted from dynamic client registration.
        /// A trailing "/" makes it a prefix. Repeatable.
        #[arg(long = "allow-redirect-prefix")]
        allow_redirect_prefixes: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { session_id: None }) {
        Command::Start { session_id } => approvals::start(session_id.as_deref()).await,
        Command::Mcp => mcp::serve().await,
        Command::Serve {
            public_url,
            addr,
            admin_token,
            allow_redirect_prefixes,
        } => serve_http(public_url, addr, admin_token, allow_redirect_prefixes).await,
    }
}

async fn serve_http(
    public_url: String,
    addr: SocketAddr,
    admin_token: Option<String>,
    allow_redirect_prefixes: Vec<String>,
) -> Result<()> {
    let admin_token = match admin_token.filter(|token| !token.trim().is_empty()) {
        Some(token) => token,
        None => stored_admin_token().await?,
    };
    let mut allowed_redirect_prefixes: Vec<String> = oauth::DEFAULT_REDIRECT_PREFIXES
        .iter()
        .map(|prefix| (*prefix).to_owned())
        .collect();
    allowed_redirect_prefixes.extend(allow_redirect_prefixes);
    let config = oauth::OAuthConfig {
        public_url: oauth::OAuthConfig::normalize_public_url(&public_url),
        admin_token,
        allowed_redirect_prefixes,
    };
    let store = oauth::OAuthStore::open(config::state_dir()?.join("oauth.json")).await?;

    eprintln!("Admin token for the authorization page: {}", config.admin_token);
    eprintln!("Allowed redirect URIs:");
    for prefix in &config.allowed_redirect_prefixes {
        eprintln!("  {prefix}{}", if prefix.ends_with('/') { "*" } else { "" });
    }
    http::serve(addr, config, store).await
}

/// Reads the generated admin token, creating it on first use.
async fn stored_admin_token() -> Result<String> {
    let path = config::state_dir()?.join("oauth-admin-token");
    if let Ok(token) = tokio::fs::read_to_string(&path).await {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    tokio::fs::create_dir_all(path.parent().context("state directory has no parent")?).await?;
    tokio::fs::write(&path, format!("{token}\n")).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    eprintln!("Generated a new admin token at {}", path.display());
    Ok(token)
}
