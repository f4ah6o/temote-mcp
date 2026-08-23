#[cfg(feature = "network")]
mod access;
mod approvals;
mod child_env;
mod config;
mod doctor;
#[cfg(feature = "network")]
mod gateway;
#[cfg(feature = "network")]
mod http;
#[cfg(feature = "network")]
mod ingress;
mod kintone_cli;
mod kintone_mcp;
#[cfg(all(feature = "network", unix))]
mod lifecycle;
mod line_protocol;
#[cfg(feature = "network")]
mod local_oauth;
mod mcp;
mod named_roots;
mod onepassword_mcp;
#[cfg(feature = "network")]
mod openai_tunnel;
mod profile;
#[cfg(feature = "network")]
mod provider;
mod supervisor;
#[cfg(test)]
mod test_support;

use temote_mcp::sandbox;

#[cfg(feature = "network")]
use std::net::SocketAddr;
#[cfg(feature = "network")]
use std::path::Path;
use std::path::PathBuf;
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
        /// Production ingress/auth profile to diagnose. Bare doctor preserves legacy auto-detection.
        #[arg(long, value_enum)]
        profile: Option<profile::Profile>,
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
    /// Run the MCP server over HTTP using the selected authentication profile.
    Serve {
        /// Production ingress/auth profile. Existing installations default to cloudflare.
        #[arg(long, value_enum, default_value_t = profile::Profile::Cloudflare)]
        profile: profile::Profile,
        /// Public HTTPS base URL clients reach this server through. When
        /// omitted, TEMOTE_MCP_PUBLIC_URL or ~/.config/temote-mcp/public.env is
        /// used.
        #[arg(long)]
        public_url: Option<String>,
        /// Local address to listen on.
        #[arg(long, default_value = "127.0.0.1:8791")]
        addr: SocketAddr,
        /// Run cloudflared as a child of this supervisor using this token file.
        #[arg(long)]
        tunnel_token_file: Option<PathBuf>,
    },
    #[cfg(all(feature = "network", unix))]
    /// Start the HTTP server and selected ingress as one foreground supervisor.
    Up {
        /// Production ingress/auth profile. Existing installations default to cloudflare.
        #[arg(long, value_enum, default_value_t = profile::Profile::Cloudflare)]
        profile: profile::Profile,
        /// Public HTTPS base URL clients reach this server through. When
        /// omitted, TEMOTE_MCP_PUBLIC_URL or ~/.config/temote-mcp/public.env is
        /// used.
        #[arg(long)]
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
    #[cfg(all(feature = "network", unix))]
    /// Migrate legacy pre-profile runtime ownership without changing configuration or local sessions.
    Migrate {
        /// Report legacy runtime state without signaling processes or deleting files.
        #[arg(long)]
        dry_run: bool,
    },
    #[cfg(feature = "network")]
    /// Manage OpenAI Secure MCP Tunnel setup.
    Openai {
        #[command(subcommand)]
        command: OpenaiCommand,
    },
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

#[cfg(feature = "network")]
#[derive(Subcommand)]
enum OpenaiCommand {
    /// Create an OpenAI Secure MCP Tunnel through the Tunnel Management API.
    Setup {
        /// Operator-visible tunnel name.
        #[arg(long, default_value = "Temote MCP")]
        name: String,
        /// Operator-visible tunnel description.
        #[arg(
            long,
            default_value = "Routes OpenAI Secure MCP Tunnel traffic to Temote MCP"
        )]
        description: String,
        /// Organization scope to attach. May be repeated.
        #[arg(long = "organization-id")]
        organization_ids: Vec<String>,
        /// ChatGPT workspace scope to attach. May be repeated.
        #[arg(long = "workspace-id")]
        workspace_ids: Vec<String>,
        /// Override the local file that stores only CONTROL_PLANE_TUNNEL_ID.
        #[arg(long)]
        config_file: Option<PathBuf>,
        /// Intentionally create a new tunnel and replace an existing saved tunnel ID.
        #[arg(long)]
        force: bool,
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
            profile,
            cloudflare,
            tunnel_token_file,
        } => {
            #[cfg(feature = "network")]
            if profile.is_none() || profile == Some(profile::Profile::Cloudflare) {
                load_public_env()?;
            }
            doctor::run(doctor::Options {
                profile,
                cloudflare,
                tunnel_token_file,
            })
            .await
        }
        Command::Start { session_id, yolo } => approvals::start(session_id.as_deref(), yolo).await,
        Command::Mcp => mcp::serve().await,
        #[cfg(feature = "network")]
        Command::Serve {
            profile,
            public_url,
            addr,
            tunnel_token_file,
        } => {
            if profile == profile::Profile::Cloudflare {
                load_public_env()?;
            }
            if profile != profile::Profile::Cloudflare && tunnel_token_file.is_some() {
                anyhow::bail!("--tunnel-token-file is only valid for the cloudflare profile");
            }
            if profile == profile::Profile::Openai && public_url.is_some() {
                anyhow::bail!("--public-url is not used by the openai profile");
            }
            serve_http(
                profile,
                public_url,
                addr,
                false,
                tunnel_token_file.as_deref(),
            )
            .await
        }
        #[cfg(all(feature = "network", unix))]
        Command::Up {
            profile,
            public_url,
            addr,
            tunnel_token_file,
        } => lifecycle::up(profile, public_url, addr, tunnel_token_file).await,
        #[cfg(all(feature = "network", unix))]
        Command::Down => lifecycle::down().await,
        #[cfg(all(feature = "network", unix))]
        Command::Migrate { dry_run } => lifecycle::migrate(dry_run).await,
        #[cfg(feature = "network")]
        Command::Openai { command } => match command {
            OpenaiCommand::Setup {
                name,
                description,
                organization_ids,
                workspace_ids,
                config_file,
                force,
            } => {
                let result = openai_tunnel::setup(openai_tunnel::SetupOptions {
                    name,
                    description,
                    organization_ids,
                    workspace_ids,
                    config_file,
                    force,
                })
                .await?;
                println!("Created OpenAI Secure MCP Tunnel {}", result.tunnel_id);
                println!(
                    "Saved CONTROL_PLANE_TUNNEL_ID to {}",
                    result.config_file.display()
                );
                if std::env::var_os("CONTROL_PLANE_API_KEY").is_some()
                    || std::env::var_os("OPENAI_API_KEY").is_some()
                {
                    println!(
                        "Runtime key detected; run `temote-mcp doctor --profile openai` before `temote-mcp up --profile openai`."
                    );
                } else {
                    println!(
                        "Next: create a Restricted Runtime API key with Tunnels Read + Use and expose it as CONTROL_PLANE_API_KEY."
                    );
                }
                Ok(())
            }
        },
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
    profile: profile::Profile,
    public_url: Option<String>,
    addr: SocketAddr,
    manage_ingress: bool,
    tunnel_token_file: Option<&Path>,
) -> Result<()> {
    if profile == profile::Profile::Openai {
        anyhow::ensure!(
            public_url.is_none(),
            "OpenAI Secure MCP Tunnel does not use a public URL"
        );
        anyhow::ensure!(
            tunnel_token_file.is_none(),
            "OpenAI Secure MCP Tunnel does not use a Cloudflare tunnel token"
        );
        return serve_openai(addr, manage_ingress).await;
    }

    let public_url = ingress::resolve_public_url(profile, public_url, manage_ingress)
        .await?
        .into_string();
    let tailscale_https_port = if manage_ingress && profile == profile::Profile::Tailscale {
        let hostname = ingress::tailscale_dns_name().await?;
        let parsed =
            url::Url::parse(&public_url).context("managed Tailscale public URL is invalid")?;
        anyhow::ensure!(
            parsed.host_str() == Some(hostname.as_str()),
            "managed Tailscale Funnel public URL must use the node hostname {hostname}"
        );
        let port = parsed
            .port_or_known_default()
            .context("managed Tailscale public URL has no HTTPS port")?;
        anyhow::ensure!(
            ingress::TAILSCALE_HTTPS_PORTS.contains(&port),
            "managed Tailscale Funnel HTTPS port must be one of 443, 8443, or 10000"
        );
        Some(port)
    } else {
        None
    };
    let roots = named_roots::NamedRoots::from_env()?;
    let (supervisor, approvals) = supervisor::SessionSupervisor::new(roots);
    let authenticator = match profile {
        profile::Profile::Cloudflare => {
            provider::AuthProvider::Cloudflare(access::AccessAuthenticator::from_env().await?)
        }
        profile::Profile::Tailscale => provider::AuthProvider::Local(local_oauth::LocalOAuth::new(
            public_url.clone(),
            supervisor.approval_sender(),
        )),
        profile::Profile::Openai => unreachable!("handled before public profile setup"),
    };
    eprintln!("Profile: {}", profile.name());
    eprintln!("Ingress: {}", profile.ingress_name());
    eprintln!("Auth: {}", profile.auth_name());
    eprintln!(
        "Managed session roots: {}",
        if supervisor.roots_configured() {
            "configured"
        } else {
            "not configured (session_start fails closed)"
        }
    );
    let console = tokio::spawn(approvals::run_supervisor_console(approvals));
    let should_start_ingress = manage_ingress || tunnel_token_file.is_some();
    let mut managed_ingress = if should_start_ingress {
        Some(ingress::start(profile, addr, tunnel_token_file, tailscale_https_port).await?)
    } else {
        None
    };

    let serve = http::serve(addr, public_url, authenticator, Arc::clone(&supervisor));
    tokio::pin!(serve);
    let serve_result = if let Some(ingress) = managed_ingress.as_mut() {
        let ingress_name = ingress.profile().ingress_name();
        tokio::select! {
            result = &mut serve => result,
            status = ingress.child_mut().wait() => {
                let status = status.with_context(|| format!("failed while waiting for {ingress_name}"))?;
                Err(anyhow::anyhow!("{ingress_name} exited while temote-mcp serve was active: {status}"))
            }
        }
    } else {
        serve.await
    };

    if let Some(ingress) = managed_ingress.as_mut() {
        stop_child(ingress.child_mut()).await;
    }
    let shutdown_result = supervisor.shutdown().await;
    console.abort();
    serve_result?;
    shutdown_result
}

#[cfg(feature = "network")]
async fn serve_openai(addr: SocketAddr, manage_tunnel: bool) -> Result<()> {
    openai_tunnel::ensure_loopback(addr)?;
    let roots = named_roots::NamedRoots::from_env()?;
    let (supervisor, approvals) = supervisor::SessionSupervisor::new(roots);
    let authenticator = provider::AuthProvider::OpenAiTunnel;
    eprintln!("Profile: openai");
    eprintln!("Connection: OpenAI Secure MCP Tunnel");
    eprintln!("Local MCP origin: {}", openai_tunnel::local_mcp_url(addr));
    eprintln!(
        "Managed session roots: {}",
        if supervisor.roots_configured() {
            "configured"
        } else {
            "not configured (session_start fails closed)"
        }
    );
    // Acquire/start the OpenAI tunnel before starting the approval console. Both may use the
    // controlling terminal, and the secret prompt must be the only terminal reader while active.
    let mut tunnel = if manage_tunnel {
        Some(openai_tunnel::start(addr).await?)
    } else {
        None
    };
    let console = tokio::spawn(approvals::run_supervisor_console(approvals));
    let serve = http::serve(
        addr,
        format!("http://{addr}"),
        authenticator,
        Arc::clone(&supervisor),
    );
    tokio::pin!(serve);
    let serve_result = if let Some(child) = tunnel.as_mut() {
        tokio::select! {
            result = &mut serve => result,
            status = child.wait() => {
                let status = status.context("failed while waiting for OpenAI tunnel-client")?;
                Err(anyhow::anyhow!("OpenAI tunnel-client exited while temote-mcp serve was active: {status}"))
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
const MAX_PUBLIC_ENV_BYTES: usize = 64 * 1024;

#[cfg(feature = "network")]
fn read_private_public_env(path: &Path) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot open {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "public env must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_PUBLIC_ENV_BYTES as u64,
        "public env exceeds {MAX_PUBLIC_ENV_BYTES} bytes: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "public env must not be accessible by group or other users (mode {mode:04o}): {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_PUBLIC_ENV_BYTES));
    std::io::Read::take(&mut file, (MAX_PUBLIC_ENV_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_PUBLIC_ENV_BYTES,
        "public env exceeds {MAX_PUBLIC_ENV_BYTES} bytes: {}",
        path.display()
    );
    Ok(Some(bytes))
}

#[cfg(feature = "network")]
pub(crate) fn load_public_env() -> Result<()> {
    let path = std::env::var_os("TEMOTE_MCP_ENV_FILE")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|config| config.join("temote-mcp").join("public.env")));
    if let Some(path) = path
        && let Some(bytes) = read_private_public_env(&path)?
    {
        dotenvy::from_read(std::io::Cursor::new(bytes))
            .with_context(|| format!("failed to load {}", path.display()))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "network"))]
mod public_env_tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_public_env_size_bound_is_exact() -> noprop::TestResult {
        test_support::run(0x5055_4245_4e56_5349, 64, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=MAX_PUBLIC_ENV_BYTES + 1024);
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("public.env");
            std::fs::write(&path, vec![b'x'; len]).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let result = read_private_public_env(&path);
            assert_eq!(result.is_ok(), len <= MAX_PUBLIC_ENV_BYTES, "len={len}");
            if let Ok(Some(bytes)) = result {
                assert_eq!(bytes.len(), len);
            }
            Ok(())
        })
    }

    #[test]
    fn public_env_rejects_symlinks_and_public_permissions() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.env");
        std::fs::write(&target, b"TEMOTE_MCP_PUBLIC_URL=https://example.com\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = root.path().join("link.env");
            symlink(&target, &link).unwrap();
            assert!(read_private_public_env(&link).is_err());

            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_private_public_env(&target).is_err());
        }
    }
}
