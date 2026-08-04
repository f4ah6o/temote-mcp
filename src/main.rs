mod access;
mod approvals;
mod config;
mod http;
mod mcp;
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
    /// Run the MCP server over HTTP behind Cloudflare Access.
    Serve {
        /// Public HTTPS base URL clients reach this server through. When
        /// omitted, LOCAL_MCP_PUBLIC_URL or ~/.config/local-mcp/public.env is
        /// used.
        #[arg(long, env = "LOCAL_MCP_PUBLIC_URL")]
        public_url: Option<String>,
        /// Local address to listen on.
        #[arg(long, default_value = "127.0.0.1:8791")]
        addr: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { session_id: None }) {
        Command::Start { session_id } => approvals::start(session_id.as_deref()).await,
        Command::Mcp => mcp::serve().await,
        Command::Serve { public_url, addr } => serve_http(public_url, addr).await,
    }
}

async fn serve_http(public_url: Option<String>, addr: SocketAddr) -> Result<()> {
    load_public_env()?;
    let public_url = public_url
        .or_else(|| std::env::var("LOCAL_MCP_PUBLIC_URL").ok())
        .context(
            "LOCAL_MCP_PUBLIC_URL is required; pass --public-url or create ~/.config/local-mcp/public.env",
        )?;
    let public_url = http::normalize_public_url(&public_url)?;
    let authenticator = access::AccessAuthenticator::from_env().await?;
    http::serve(addr, public_url, authenticator).await
}

fn load_public_env() -> Result<()> {
    let Some(config_dir) = dirs::config_dir() else {
        return Ok(());
    };
    let path = config_dir.join("local-mcp").join("public.env");
    if path.is_file() {
        dotenvy::from_path(&path).with_context(|| format!("failed to load {}", path.display()))?;
    }
    Ok(())
}
