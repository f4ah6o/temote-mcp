#[cfg(feature = "network")]
mod access;
mod approvals;
mod config;
mod doctor;
#[cfg(feature = "network")]
mod gateway;
#[cfg(feature = "network")]
mod http;
mod mcp;
mod onepassword_mcp;
mod sandbox;

#[cfg(feature = "network")]
use std::net::SocketAddr;
#[cfg(feature = "network")]
use std::time::Duration;

#[cfg(feature = "network")]
use anyhow::Context;
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Diagnose local-mcp and the host sandbox prerequisites.
    Doctor,
    /// Start a session in the current directory and show its permission UI.
    Start {
        /// Session ID to use instead of generating a UUID.
        session_id: Option<String>,
        /// Disable local approvals and run tools with the full permissions of this user.
        #[arg(long)]
        yolo: bool,
    },
    /// Run the session-independent MCP server over stdin/stdout.
    Mcp,
    #[cfg(feature = "network")]
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
    #[cfg(feature = "network")]
    /// Connect an active local session to a Cloudflare gateway using outbound long polling.
    GatewayAgent {
        /// Cloudflare Worker origin, without a path.
        #[arg(long, env = "LOCAL_MCP_GATEWAY_URL")]
        gateway_url: String,
        /// Active local-mcp session to publish through the gateway.
        #[arg(long)]
        session_id: String,
        /// Shared host credential stored as the Worker's HOST_TOKEN secret.
        #[arg(long, env = "LOCAL_MCP_GATEWAY_HOST_TOKEN", hide_env_values = true)]
        host_token: String,
        /// Optional Cloudflare Access service-token client ID.
        #[arg(long, env = "LOCAL_MCP_GATEWAY_ACCESS_CLIENT_ID")]
        access_client_id: Option<String>,
        /// Optional Cloudflare Access service-token client secret.
        #[arg(
            long,
            env = "LOCAL_MCP_GATEWAY_ACCESS_CLIENT_SECRET",
            hide_env_values = true
        )]
        access_client_secret: Option<String>,
        /// Host platform reported to the gateway. Auto detects macOS, Linux, or WSL2.
        #[arg(long, value_enum, default_value = "auto")]
        platform: gateway::Platform,
        /// Delay before reconnecting after a disconnect or generation replacement.
        #[arg(long, default_value_t = 2)]
        reconnect_delay_seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start {
        session_id: None,
        yolo: false,
    }) {
        Command::Doctor => doctor::run().await,
        Command::Start { session_id, yolo } => approvals::start(session_id.as_deref(), yolo).await,
        Command::Mcp => mcp::serve().await,
        #[cfg(feature = "network")]
        Command::Serve { public_url, addr } => serve_http(public_url, addr).await,
        #[cfg(feature = "network")]
        Command::GatewayAgent {
            gateway_url,
            session_id,
            host_token,
            access_client_id,
            access_client_secret,
            platform,
            reconnect_delay_seconds,
        } => {
            gateway::run_agent(gateway::AgentOptions {
                gateway_url,
                session_id,
                host_token,
                access_client_id,
                access_client_secret,
                platform,
                reconnect_delay: Duration::from_secs(reconnect_delay_seconds),
            })
            .await
        }
    }
}

#[cfg(feature = "network")]
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

#[cfg(feature = "network")]
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
