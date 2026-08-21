#[cfg(feature = "network")]
mod access;
mod approvals;
mod config;
mod doctor;
#[cfg(feature = "network")]
mod gateway;
#[cfg(feature = "network")]
mod http;
mod kintone_cli;
mod kintone_mcp;
#[cfg(all(feature = "network", unix))]
mod lifecycle;
mod mcp;
mod named_roots;
mod onepassword_mcp;
mod sandbox;
mod supervisor;

#[cfg(feature = "network")]
use std::net::SocketAddr;
#[cfg(feature = "network")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "network")]
use std::process::Stdio;
#[cfg(feature = "network")]
use std::sync::Arc;
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
    /// Diagnose temote-mcp, local Tunnel prerequisites, and the host sandbox.
    Doctor {
        /// Also query the Cloudflare API for the configured Tunnel status.
        #[arg(long)]
        cloudflare: bool,
        /// Cloudflare Tunnel token file. Defaults to TUNNEL_TOKEN_FILE or
        /// ~/.config/temote-mcp/tunnel-token.
        #[arg(long, env = "TUNNEL_TOKEN_FILE")]
        tunnel_token_file: Option<PathBuf>,
    },
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
        /// omitted, TEMOTE_MCP_PUBLIC_URL or ~/.config/temote-mcp/public.env is
        /// used.
        #[arg(long, env = "TEMOTE_MCP_PUBLIC_URL")]
        public_url: Option<String>,
        /// Local address to listen on.
        #[arg(long, default_value = "127.0.0.1:8791")]
        addr: SocketAddr,
        /// Run cloudflared as a child of this supervisor using this token file.
        #[arg(long)]
        tunnel_token_file: Option<PathBuf>,
    },
    #[cfg(all(feature = "network", unix))]
    /// Start the HTTP server and Cloudflare Tunnel as one foreground supervisor.
    Up {
        /// Public HTTPS base URL clients reach this server through. When
        /// omitted, TEMOTE_MCP_PUBLIC_URL or ~/.config/temote-mcp/public.env is
        /// used.
        #[arg(long, env = "TEMOTE_MCP_PUBLIC_URL")]
        public_url: Option<String>,
        /// Local address to listen on.
        #[arg(long, default_value = "127.0.0.1:8791")]
        addr: SocketAddr,
        /// Cloudflare Tunnel token file. Defaults to TUNNEL_TOKEN_FILE or
        /// ~/.config/temote-mcp/tunnel-token.
        #[arg(long, env = "TUNNEL_TOKEN_FILE")]
        tunnel_token_file: Option<PathBuf>,
    },
    #[cfg(all(feature = "network", unix))]
    /// Stop the foreground supervisor started by temote-mcp up.
    Down,
    #[cfg(feature = "network")]
    /// Connect an active local session to a Cloudflare gateway using outbound long polling.
    GatewayAgent {
        /// Cloudflare Worker origin, without a path.
        #[arg(long, env = "TEMOTE_MCP_GATEWAY_URL")]
        gateway_url: String,
        /// Active temote-mcp session to publish through the gateway.
        #[arg(long)]
        session_id: String,
        /// Shared host credential stored as the Worker's HOST_TOKEN secret.
        #[arg(long, env = "TEMOTE_MCP_GATEWAY_HOST_TOKEN", hide_env_values = true)]
        host_token: String,
        /// Optional Cloudflare Access service-token client ID.
        #[arg(long, env = "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID")]
        access_client_id: Option<String>,
        /// Optional Cloudflare Access service-token client secret.
        #[arg(
            long,
            env = "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET",
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
        Command::Doctor {
            cloudflare,
            tunnel_token_file,
        } => {
            #[cfg(feature = "network")]
            load_public_env()?;
            doctor::run(doctor::Options {
                cloudflare,
                tunnel_token_file,
            })
            .await
        }
        Command::Start { session_id, yolo } => approvals::start(session_id.as_deref(), yolo).await,
        Command::Mcp => mcp::serve().await,
        #[cfg(feature = "network")]
        Command::Serve {
            public_url,
            addr,
            tunnel_token_file,
        } => {
            load_public_env()?;
            serve_http(public_url, addr, tunnel_token_file.as_deref()).await
        }
        #[cfg(all(feature = "network", unix))]
        Command::Up {
            public_url,
            addr,
            tunnel_token_file,
        } => lifecycle::up(public_url, addr, tunnel_token_file).await,
        #[cfg(all(feature = "network", unix))]
        Command::Down => lifecycle::down().await,
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
async fn serve_http(
    public_url: Option<String>,
    addr: SocketAddr,
    tunnel_token_file: Option<&Path>,
) -> Result<()> {
    let public_url = public_url
        .or_else(|| std::env::var("TEMOTE_MCP_PUBLIC_URL").ok())
        .context(
            "TEMOTE_MCP_PUBLIC_URL is required; pass --public-url or create ~/.config/temote-mcp/public.env",
        )?;
    let public_url = http::normalize_public_url(&public_url)?;
    let authenticator = access::AccessAuthenticator::from_env().await?;
    let roots = named_roots::NamedRoots::from_env()?;
    let (supervisor, approvals) = supervisor::SessionSupervisor::new(roots);
    eprintln!(
        "Managed session roots: {}",
        if supervisor.roots_configured() {
            "configured"
        } else {
            "not configured (session_start fails closed)"
        }
    );
    let console = tokio::spawn(approvals::run_supervisor_console(approvals));
    let mut tunnel = if let Some(token_file) = tunnel_token_file {
        let child = tokio::process::Command::new("cloudflared")
            .args(["tunnel", "run", "--token-file"])
            .arg(token_file)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("failed to start cloudflared with {}", token_file.display())
            })?;
        Some(child)
    } else {
        None
    };

    let serve = http::serve(addr, public_url, authenticator, Arc::clone(&supervisor));
    tokio::pin!(serve);
    let serve_result = if let Some(child) = tunnel.as_mut() {
        tokio::select! {
            result = &mut serve => result,
            status = child.wait() => {
                let status = status.context("failed while waiting for cloudflared")?;
                Err(anyhow::anyhow!("cloudflared exited while temote-mcp serve was active: {status}"))
            }
        }
    } else {
        serve.await
    };

    if let Some(child) = tunnel.as_mut() {
        stop_child(child).await;
    }
    let shutdown_result = supervisor.shutdown().await;
    console.abort();
    serve_result?;
    shutdown_result
}

#[cfg(feature = "network")]
async fn stop_child(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(feature = "network")]
pub(crate) fn load_public_env() -> Result<()> {
    let path = std::env::var_os("TEMOTE_MCP_ENV_FILE")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|config| config.join("temote-mcp").join("public.env")));
    if let Some(path) = path.filter(|path| path.is_file()) {
        dotenvy::from_path(&path).with_context(|| format!("failed to load {}", path.display()))?;
    }
    Ok(())
}
