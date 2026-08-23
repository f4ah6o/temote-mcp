#[cfg(feature = "network")]
mod access;
mod approvals;
mod child_env;
mod cli;
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
mod line_diff;
mod line_protocol;
#[cfg(feature = "network")]
mod local_oauth;
mod mcp;
mod named_roots;
mod onepassword_mcp;
#[cfg(feature = "network")]
mod openai_tunnel;
mod platform_paths;
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
use std::path::{Path, PathBuf};
#[cfg(feature = "network")]
use std::sync::Arc;
#[cfg(feature = "network")]
use std::time::Duration;

#[cfg(feature = "network")]
use anyhow::Context;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = match cli::parse_env() {
        Ok(cli::ParseOutcome::Run(cli)) => cli,
        Ok(cli::ParseOutcome::Print(output)) => {
            print!("{output}");
            return Ok(());
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    match cli.command.unwrap_or(cli::Command::Start {
        session_id: None,
        yolo: false,
    }) {
        cli::Command::Doctor {
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
        cli::Command::Start { session_id, yolo } => {
            approvals::start(session_id.as_deref(), yolo).await
        }
        cli::Command::Mcp => mcp::serve().await,
        #[cfg(feature = "network")]
        cli::Command::Serve {
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
        cli::Command::Up {
            profile,
            public_url,
            addr,
            tunnel_token_file,
        } => {
            if profile == profile::Profile::Cloudflare {
                migrate_legacy_cloudflare_config(false)?;
            }
            lifecycle::up(profile, public_url, addr, tunnel_token_file).await
        }
        #[cfg(all(feature = "network", unix))]
        cli::Command::Down => lifecycle::down().await,
        #[cfg(all(feature = "network", unix))]
        cli::Command::Migrate { dry_run } => {
            migrate_legacy_cloudflare_config(dry_run)?;
            lifecycle::migrate(dry_run).await
        }
        #[cfg(feature = "network")]
        cli::Command::Openai { command } => match command {
            cli::OpenaiCommand::Setup {
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
        cli::Command::GatewayAgent {
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
const REQUIRED_CLOUDFLARE_ENV_KEYS: [&str; 4] = [
    "TEMOTE_MCP_PUBLIC_URL",
    "TEMOTE_MCP_ACCESS_TEAM_DOMAIN",
    "TEMOTE_MCP_ACCESS_AUDIENCE",
    "TEMOTE_MCP_ACCESS_ALLOWED_EMAILS",
];
#[cfg(feature = "network")]
const MIGRATED_CLOUDFLARE_ENV_KEYS: [&str; 6] = [
    "TEMOTE_MCP_PUBLIC_URL",
    "TEMOTE_MCP_ACCESS_TEAM_DOMAIN",
    "TEMOTE_MCP_ACCESS_AUDIENCE",
    "TEMOTE_MCP_ACCESS_ALLOWED_EMAILS",
    "TEMOTE_MCP_ROOTS",
    "TUNNEL_TOKEN_FILE",
];
#[cfg(feature = "network")]
const LEGACY_TUNNEL_TOKEN_KEY: &str = "TEMOTE_MCP_TUNNEL_TOKEN";

#[cfg(feature = "network")]
fn default_public_env_file() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        platform_paths::home_dir()
            .map(|home| home.join(".config").join("temote-mcp").join("public.env"))
            .context("could not determine HOME for the default public environment file")
    } else {
        platform_paths::config_dir()
            .map(|config| config.join("temote-mcp").join("public.env"))
            .context("could not determine the default public environment directory")
    }
}

#[cfg(feature = "network")]
fn public_env_file() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("TEMOTE_MCP_ENV_FILE") {
        return Ok(PathBuf::from(path));
    }
    default_public_env_file()
}

#[cfg(feature = "network")]
fn default_tunnel_token_file() -> Result<PathBuf> {
    platform_paths::home_dir()
        .map(|home| home.join(".config").join("temote-mcp").join("tunnel-token"))
        .context("could not determine HOME for the default tunnel token file")
}

#[cfg(feature = "network")]
fn read_bounded_env_file(
    path: &Path,
    require_private: bool,
    label: &str,
) -> Result<Option<Vec<u8>>> {
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
        "{label} must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_PUBLIC_ENV_BYTES as u64,
        "{label} exceeds {MAX_PUBLIC_ENV_BYTES} bytes: {}",
        path.display()
    );
    #[cfg(unix)]
    if require_private {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "{label} must not be accessible by group or other users (mode {mode:04o}): {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_PUBLIC_ENV_BYTES));
    std::io::Read::take(&mut file, (MAX_PUBLIC_ENV_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_PUBLIC_ENV_BYTES,
        "{label} exceeds {MAX_PUBLIC_ENV_BYTES} bytes: {}",
        path.display()
    );
    Ok(Some(bytes))
}

#[cfg(feature = "network")]
fn read_private_public_env(path: &Path) -> Result<Option<Vec<u8>>> {
    read_bounded_env_file(path, true, "public env")
}

#[cfg(feature = "network")]
fn quote_dotenv_value(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value
            .chars()
            .any(|character| matches!(character, '\0' | '\n' | '\r' | '\'')),
        "legacy Temote env value contains characters that cannot be migrated safely"
    );

    // dotenvy's line reader treats a backslash before the closing single quote as
    // escaping that quote, even though its value parser treats backslashes inside
    // single quotes literally. Keep trailing backslashes outside the quoted segment
    // and encode each as an escaped pair so both parser layers round-trip exactly.
    let prefix = value.trim_end_matches('\\');
    let trailing_backslashes = value.len() - prefix.len();
    let mut encoded = format!("'{prefix}'");
    for _ in 0..trailing_backslashes {
        encoded.push_str("\\\\");
    }
    Ok(encoded)
}

#[cfg(feature = "network")]
#[derive(Debug)]
struct LegacyCloudflareConfig {
    public_env: Vec<u8>,
    tunnel_token: Option<String>,
}

#[cfg(feature = "network")]
fn parse_legacy_cloudflare_env(
    path: &Path,
    bytes: &[u8],
) -> Result<Option<LegacyCloudflareConfig>> {
    use std::collections::BTreeMap;

    let mut values = BTreeMap::<String, String>::new();
    for item in dotenvy::from_read_iter(std::io::Cursor::new(bytes)) {
        let (key, value) =
            item.with_context(|| format!("failed to parse legacy Temote env {}", path.display()))?;
        if MIGRATED_CLOUDFLARE_ENV_KEYS.contains(&key.as_str()) || key == LEGACY_TUNNEL_TOKEN_KEY {
            values.entry(key).or_insert(value);
        }
    }

    if !values.contains_key("TEMOTE_MCP_PUBLIC_URL") {
        return Ok(None);
    }

    let missing = REQUIRED_CLOUDFLARE_ENV_KEYS
        .iter()
        .filter(|key| values.get(**key).is_none_or(String::is_empty))
        .copied()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "legacy Temote env {} is incomplete; missing {}",
        path.display(),
        missing.join(", ")
    );

    let mut public_env =
        String::from("# Migrated by temote-mcp from a legacy checkout-local .env.\n");
    for key in MIGRATED_CLOUDFLARE_ENV_KEYS {
        if let Some(value) = values.get(key) {
            public_env.push_str(key);
            public_env.push('=');
            public_env.push_str(&quote_dotenv_value(value)?);
            public_env.push('\n');
        }
    }

    let has_explicit_token_file = values
        .get("TUNNEL_TOKEN_FILE")
        .is_some_and(|value| !value.is_empty());
    let tunnel_token = if has_explicit_token_file {
        None
    } else {
        values
            .remove(LEGACY_TUNNEL_TOKEN_KEY)
            .filter(|value| !value.is_empty())
    };

    Ok(Some(LegacyCloudflareConfig {
        public_env: public_env.into_bytes(),
        tunnel_token,
    }))
}

#[cfg(feature = "network")]
fn legacy_checkout_env_file() -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().context("could not determine the current directory")?;
    let cargo_toml = cwd.join("Cargo.toml");
    let Some(cargo_bytes) = read_bounded_env_file(&cargo_toml, false, "Cargo.toml")? else {
        return Ok(None);
    };
    let cargo = String::from_utf8_lossy(&cargo_bytes);
    let is_temote = cargo
        .lines()
        .any(|line| line.trim() == "name = \"temote-mcp\"");
    if !is_temote {
        return Ok(None);
    }
    let env_path = cwd.join(".env");
    Ok(read_bounded_env_file(&env_path, false, "legacy Temote env")?.map(|_| env_path))
}

#[cfg(feature = "network")]
fn write_private_file_new(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .context("configuration destination has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {label} {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    Ok(())
}

#[cfg(feature = "network")]
fn legacy_platform_public_env_file(target: &Path) -> Option<PathBuf> {
    platform_paths::config_dir()
        .map(|config| config.join("temote-mcp").join("public.env"))
        .filter(|path| path != target)
}

#[cfg(feature = "network")]
fn migrate_legacy_cloudflare_config(dry_run: bool) -> Result<()> {
    let target = public_env_file()?;
    if read_private_public_env(&target)?.is_some() {
        return Ok(());
    }

    if let Some(source) = legacy_platform_public_env_file(&target)
        && let Some(bytes) = read_private_public_env(&source)?
    {
        if dry_run {
            println!(
                "legacy Cloudflare config migration required: {} -> {}",
                source.display(),
                target.display()
            );
            return Ok(());
        }
        write_private_file_new(&target, &bytes, "public env")?;
        println!(
            "migrated legacy Cloudflare public env: {} -> {}",
            source.display(),
            target.display()
        );
        return Ok(());
    }

    let Some(source) = legacy_checkout_env_file()? else {
        return Ok(());
    };
    let Some(bytes) = read_bounded_env_file(&source, false, "legacy Temote env")? else {
        return Ok(());
    };
    let Some(config) = parse_legacy_cloudflare_env(&source, &bytes)? else {
        return Ok(());
    };

    let token_target = if config.tunnel_token.is_some() {
        Some(default_tunnel_token_file()?)
    } else {
        None
    };

    if dry_run {
        println!(
            "legacy Cloudflare config migration required: {} -> {}",
            source.display(),
            target.display()
        );
        if let Some(token_target) = &token_target
            && !token_target.exists()
        {
            println!(
                "legacy Cloudflare tunnel token migration required: {} -> {}",
                LEGACY_TUNNEL_TOKEN_KEY,
                token_target.display()
            );
        }
        return Ok(());
    }

    if let (Some(token), Some(token_target)) =
        (config.tunnel_token.as_deref(), token_target.as_ref())
    {
        match read_bounded_env_file(token_target, true, "tunnel token")? {
            Some(existing) => {
                let existing = std::str::from_utf8(&existing)
                    .context("existing tunnel token is not valid UTF-8")?;
                anyhow::ensure!(
                    existing.trim() == token,
                    "existing tunnel token at {} differs from legacy {}; refusing to overwrite it",
                    token_target.display(),
                    LEGACY_TUNNEL_TOKEN_KEY
                );
            }
            None => {
                write_private_file_new(token_target, token.as_bytes(), "tunnel token")?;
                println!(
                    "migrated legacy Cloudflare tunnel token to {}",
                    token_target.display()
                );
            }
        }
    }

    write_private_file_new(&target, &config.public_env, "public env")?;
    println!(
        "migrated legacy Cloudflare public env: {} -> {}",
        source.display(),
        target.display()
    );
    Ok(())
}

#[cfg(feature = "network")]
pub(crate) fn load_public_env() -> Result<()> {
    let path = public_env_file()?;
    if let Some(bytes) = read_private_public_env(&path)? {
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

    #[test]
    fn legacy_cloudflare_env_migrates_only_supported_values() {
        let path = Path::new(".env");
        let bytes = br#"
TEMOTE_MCP_PUBLIC_URL=https://example.com
TEMOTE_MCP_ACCESS_TEAM_DOMAIN=https://team.cloudflareaccess.com
TEMOTE_MCP_ACCESS_AUDIENCE=audience
TEMOTE_MCP_ACCESS_ALLOWED_EMAILS=user@example.com
TEMOTE_MCP_ROOTS=src=~/src
TUNNEL_TOKEN_FILE=/tmp/tunnel-token
TEMOTE_MCP_TUNNEL_TOKEN=legacy-secret
TEMOTE_MCP_GATEWAY_HOST_TOKEN=must-not-copy
UNRELATED=value
"#;
        let migrated = parse_legacy_cloudflare_env(path, bytes).unwrap().unwrap();
        let public_env = String::from_utf8(migrated.public_env).unwrap();
        for expected in [
            "TEMOTE_MCP_PUBLIC_URL=",
            "TEMOTE_MCP_ACCESS_TEAM_DOMAIN=",
            "TEMOTE_MCP_ACCESS_AUDIENCE=",
            "TEMOTE_MCP_ACCESS_ALLOWED_EMAILS=",
            "TEMOTE_MCP_ROOTS=",
            "TUNNEL_TOKEN_FILE=",
        ] {
            assert!(public_env.contains(expected), "{public_env}");
        }
        assert!(!public_env.contains("TEMOTE_MCP_TUNNEL_TOKEN"));
        assert!(!public_env.contains("TEMOTE_MCP_GATEWAY_HOST_TOKEN"));
        assert!(!public_env.contains("UNRELATED"));
        assert!(migrated.tunnel_token.is_none());
    }

    #[test]
    fn legacy_cloudflare_env_extracts_raw_tunnel_token_without_token_file() {
        let bytes = br#"
TEMOTE_MCP_PUBLIC_URL=https://example.com
TEMOTE_MCP_ACCESS_TEAM_DOMAIN=https://team.cloudflareaccess.com
TEMOTE_MCP_ACCESS_AUDIENCE=audience
TEMOTE_MCP_ACCESS_ALLOWED_EMAILS=user@example.com
TEMOTE_MCP_TUNNEL_TOKEN=legacy-secret
"#;
        let migrated = parse_legacy_cloudflare_env(Path::new(".env"), bytes)
            .unwrap()
            .unwrap();
        assert_eq!(migrated.tunnel_token.as_deref(), Some("legacy-secret"));
    }

    #[test]
    fn legacy_cloudflare_env_requires_complete_access_config() {
        let bytes = br#"
TEMOTE_MCP_PUBLIC_URL=https://example.com
TEMOTE_MCP_ACCESS_TEAM_DOMAIN=https://team.cloudflareaccess.com
"#;
        let error = parse_legacy_cloudflare_env(Path::new(".env"), bytes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("TEMOTE_MCP_ACCESS_AUDIENCE"), "{error}");
        assert!(
            error.contains("TEMOTE_MCP_ACCESS_ALLOWED_EMAILS"),
            "{error}"
        );
    }

    #[test]
    fn legacy_cloudflare_env_ignores_unrelated_dotenv_files() {
        assert!(
            parse_legacy_cloudflare_env(Path::new(".env"), b"FOO=bar\n")
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn migrated_private_file_is_owner_only_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("public.env");
        write_private_file_new(&path, b"first\n", "public env").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"first\n");
        assert!(write_private_file_new(&path, b"second\n", "public env").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"first\n");
    }

    #[test]
    fn quoted_dotenv_values_are_literal_and_round_trip() {
        let original = "a b$c\\d\"e#f";
        let line = format!("VALUE={}\n", quote_dotenv_value(original).unwrap());
        let parsed = dotenvy::from_read_iter(std::io::Cursor::new(line))
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(parsed, ("VALUE".to_owned(), original.to_owned()));
    }

    #[test]
    fn quoted_dotenv_values_fail_closed_on_ambiguous_characters() {
        for value in [
            "line\nbreak",
            "carriage\rreturn",
            "single'quote",
            "nul\0byte",
        ] {
            assert!(quote_dotenv_value(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn generated_safe_dotenv_values_round_trip_literal() -> noprop::TestResult {
        test_support::run(0x444f_5445_4e56_5341, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=256);
            let value = (0..len)
                .map(|_| match noprop::sample_u8(ctx) & 0x7f {
                    b'\0' | b'\n' | b'\r' | b'\'' => 'x',
                    byte => char::from(byte),
                })
                .collect::<String>();
            let line = format!("VALUE={}\n", quote_dotenv_value(&value).unwrap());
            let parsed = dotenvy::from_read_iter(std::io::Cursor::new(line))
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(parsed, ("VALUE".to_owned(), value));
            Ok(())
        })
    }

    #[test]
    fn generated_unicode_dotenv_values_round_trip_literal() -> noprop::TestResult {
        test_support::run(0x444f_5445_4e56_554e, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=128);
            let value = (0..len)
                .map(|_| {
                    let scalar = noprop::sample_u32(ctx) % 0x11_0000;
                    let character = char::from_u32(scalar).unwrap_or('x');
                    if matches!(character, '\0' | '\n' | '\r' | '\'') {
                        'x'
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let line = format!("VALUE={}\n", quote_dotenv_value(&value).unwrap());
            let parsed = dotenvy::from_read_iter(std::io::Cursor::new(line))
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(parsed, ("VALUE".to_owned(), value));
            Ok(())
        })
    }

    #[test]
    fn generated_ambiguous_dotenv_values_fail_closed() -> noprop::TestResult {
        test_support::run(0x444f_5445_4e56_4241, test_support::DEFAULT_CASES, |ctx| {
            let mut value = test_support::ascii_string(ctx, 128)
                .chars()
                .filter(|character| !matches!(character, '\0' | '\n' | '\r' | '\''))
                .collect::<String>();
            let forbidden = match noprop::sample_usize_in(ctx, 0..4) {
                0 => '\0',
                1 => '\n',
                2 => '\r',
                _ => '\'',
            };
            let index = noprop::sample_usize_in(ctx, 0..=value.len());
            value.insert(index, forbidden);
            assert!(quote_dotenv_value(&value).is_err(), "{value:?}");
            Ok(())
        })
    }

    #[test]
    fn generated_legacy_required_key_sets_match_reference_model() -> noprop::TestResult {
        test_support::run(0x4c45_4741_4359_5245, test_support::DEFAULT_CASES, |ctx| {
            let mask = noprop::sample_u8(ctx) & 0x0f;
            let nonce = noprop::sample_u64(ctx);
            let mut input = String::new();
            for (index, key) in REQUIRED_CLOUDFLARE_ENV_KEYS.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    input.push_str(&format!("{key}=value-{index}-{nonce:x}\n"));
                }
            }

            let result = parse_legacy_cloudflare_env(Path::new(".env"), input.as_bytes());
            let public_url_present = mask & 0x01 != 0;
            let all_required_present = mask == 0x0f;
            match (public_url_present, all_required_present, result) {
                (false, _, Ok(None)) => {}
                (true, false, Err(_)) => {}
                (true, true, Ok(Some(_))) => {}
                (_, _, other) => {
                    panic!("unexpected migration result for mask {mask:04b}: {other:?}")
                }
            }
            Ok(())
        })
    }

    #[test]
    fn generated_legacy_migration_never_copies_secret_values() -> noprop::TestResult {
        use std::collections::BTreeMap;

        test_support::run(0x4c45_4741_4359_5345, test_support::DEFAULT_CASES, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let explicit_token_file = noprop::sample_bool(ctx);
            let raw_token = format!("raw-secret-{nonce:016x}");
            let gateway_secret = format!("gateway-secret-{nonce:016x}");
            let unrelated_secret = format!("unrelated-secret-{nonce:016x}");
            let mut input = format!(
                "TEMOTE_MCP_PUBLIC_URL=https://public-{nonce:016x}.example.invalid\n\
TEMOTE_MCP_ACCESS_TEAM_DOMAIN=https://team-{nonce:016x}.example.invalid\n\
TEMOTE_MCP_ACCESS_AUDIENCE=aud-{nonce:016x}\n\
TEMOTE_MCP_ACCESS_ALLOWED_EMAILS=user-{nonce:016x}@example.invalid\n\
{LEGACY_TUNNEL_TOKEN_KEY}={raw_token}\n\
TEMOTE_MCP_GATEWAY_HOST_TOKEN={gateway_secret}\n\
UNRELATED={unrelated_secret}\n"
            );
            if explicit_token_file {
                input.push_str(&format!("TUNNEL_TOKEN_FILE=/tmp/token-{nonce:016x}\n"));
            }

            let migrated = parse_legacy_cloudflare_env(Path::new(".env"), input.as_bytes())
                .unwrap()
                .unwrap();
            let public_env = String::from_utf8(migrated.public_env).unwrap();
            for secret in [&raw_token, &gateway_secret, &unrelated_secret] {
                assert!(
                    !public_env.contains(secret),
                    "secret leaked into public env"
                );
            }
            assert!(!public_env.contains(LEGACY_TUNNEL_TOKEN_KEY));
            assert!(!public_env.contains("TEMOTE_MCP_GATEWAY_HOST_TOKEN"));
            assert!(!public_env.contains("UNRELATED"));

            let parsed = dotenvy::from_read_iter(std::io::Cursor::new(public_env))
                .map(|entry| entry.unwrap())
                .collect::<BTreeMap<_, _>>();
            let expected_len =
                REQUIRED_CLOUDFLARE_ENV_KEYS.len() + usize::from(explicit_token_file);
            assert_eq!(parsed.len(), expected_len);
            assert_eq!(
                migrated.tunnel_token.as_deref(),
                (!explicit_token_file).then_some(raw_token.as_str())
            );
            Ok(())
        })
    }
}
