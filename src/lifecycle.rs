use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::host_identity;
use crate::profile::Profile;

const PID_FILE_NAME: &str = "up.pid";
const RUNTIME_STATE_FILE_NAME: &str = "up.state.json";
const LEGACY_PID_FILE_NAME: &str = "up.pids";
const MAX_LEGACY_PID_FILE_BYTES: usize = 64;
const PROCESS_NAME: &str = env!("CARGO_PKG_NAME");
const MAX_PID_FILE_BYTES: usize = 64;
const MAX_RUNTIME_STATE_BYTES: usize = 16 * 1024;
const RUNTIME_STATE_SCHEMA_VERSION: u64 = 1;
const RUNTIME_STATE_OWNERSHIP: &str = "temote_mcp_up";
const MAX_TUNNEL_TOKEN_BYTES: u64 = 64 * 1024;
const ORIGIN_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const INGRESS_RESTART_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ORIGIN_HEALTH_BYTES: usize = 16 * 1024;
#[cfg(target_os = "macos")]
const PS_COMMAND: &str = "/bin/ps";
#[cfg(target_os = "macos")]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";
#[cfg(target_os = "linux")]
const PS_COMMAND: &str = "/usr/bin/ps";
#[cfg(target_os = "linux")]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
const PS_COMMAND: &str = "/bin/ps";
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct DirectIngressRuntimeState {
    schema: u64,
    ownership: String,
    pid: i32,
    version: String,
    profile: String,
    host_id: String,
    addr: SocketAddr,
    public_url: Option<String>,
    tunnel_token_file: Option<PathBuf>,
    openai_tunnel_id: Option<String>,
    tunnel_client_bin: Option<PathBuf>,
    restart_context_keys: Vec<String>,
    restart_recipe_available: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DirectIngressUpgradePlan {
    pub active: bool,
    pub source_version: Option<String>,
    pub target_version: String,
    pub profile: Option<String>,
    pub addr: Option<SocketAddr>,
    pub action: String,
    pub health: String,
    pub reason: Option<String>,
}

pub struct PreparedDirectIngressUpgrade {
    plan: DirectIngressUpgradePlan,
    state: Option<DirectIngressRuntimeState>,
}

impl PreparedDirectIngressUpgrade {
    pub fn plan(&self) -> &DirectIngressUpgradePlan {
        &self.plan
    }

    pub fn blocker(&self) -> Option<&str> {
        (self.plan.action == "blocked")
            .then_some(self.plan.reason.as_deref())
            .flatten()
    }
}

pub async fn up(
    profile: Profile,
    public_url: Option<String>,
    addr: SocketAddr,
    tunnel_token_file: Option<PathBuf>,
) -> Result<()> {
    let legacy_pid_file = runtime_directory()?.join(LEGACY_PID_FILE_NAME);
    if read_legacy_up_pids(&legacy_pid_file)?.is_some() {
        anyhow::bail!(
            "legacy Temote runtime state exists at {}; run `temote-mcp migrate --dry-run` and `temote-mcp migrate` before `temote-mcp up`",
            legacy_pid_file.display()
        );
    }

    if profile == Profile::Cloudflare {
        crate::load_public_env()?;
    }

    let tunnel_token_file = match profile {
        Profile::Cloudflare => {
            let path = tunnel_token_file
                .or_else(|| env_path("TUNNEL_TOKEN_FILE"))
                .unwrap_or(default_tunnel_token_file()?);
            ensure_tunnel_token_file(&path)?;
            Some(path)
        }
        Profile::Tailscale | Profile::Openai => {
            anyhow::ensure!(
                tunnel_token_file.is_none(),
                "--tunnel-token-file is only valid for the cloudflare profile"
            );
            None
        }
    };
    let effective_public_url = match profile {
        Profile::Cloudflare | Profile::Tailscale => Some(
            crate::ingress::resolve_public_url(profile, public_url, true)
                .await?
                .into_string(),
        ),
        Profile::Openai => None,
    };
    let (openai_tunnel_id, tunnel_client_bin) = if profile == Profile::Openai {
        (
            Some(crate::openai_tunnel::configured_tunnel_id()?),
            env_path("TUNNEL_CLIENT_BIN"),
        )
    } else {
        (None, None)
    };
    let restart_context_keys = openai_restart_context_keys(profile);
    let restart_recipe_available = profile != Profile::Openai || !restart_context_keys.is_empty();
    let host_id = host_identity::resolve()?;
    let endpoint = effective_public_url
        .as_deref()
        .unwrap_or("<profile-managed/private>");
    eprintln!(
        "Temote direct ingress: topology=single-host host_id={host_id} profile={} endpoint={endpoint}",
        profile.name()
    );

    let pid_path = pid_file(true)?;
    let _pid_file = PidFile::create(&pid_path)?;
    let state = DirectIngressRuntimeState {
        schema: RUNTIME_STATE_SCHEMA_VERSION,
        ownership: RUNTIME_STATE_OWNERSHIP.to_owned(),
        pid: std::process::id() as i32,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        profile: profile.name().to_owned(),
        host_id,
        addr,
        public_url: effective_public_url.clone(),
        tunnel_token_file: tunnel_token_file.clone(),
        openai_tunnel_id,
        tunnel_client_bin,
        restart_context_keys,
        restart_recipe_available,
    };
    let state_path = runtime_state_file(true)?;
    let _runtime_state_file = RuntimeStateFile::create(&state_path, &state)?;
    crate::serve_http(
        profile,
        effective_public_url,
        addr,
        true,
        tunnel_token_file.as_deref(),
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LegacyUpPids {
    serve: i32,
    tunnel: i32,
}

pub async fn migrate(dry_run: bool) -> Result<()> {
    let path = runtime_directory()?.join(LEGACY_PID_FILE_NAME);
    let Some(pids) = read_legacy_up_pids(&path)? else {
        println!("no legacy temote-mcp runtime state found");
        return Ok(());
    };

    let serve_alive = process_exists(pids.serve);
    let tunnel_alive = process_exists(pids.tunnel);
    if serve_alive {
        ensure_process_name(pids.serve, PROCESS_NAME, "legacy Temote supervisor")?;
    }
    if tunnel_alive {
        ensure_process_name(pids.tunnel, "cloudflared", "legacy Cloudflare Tunnel")?;
    }

    if dry_run {
        println!(
            "legacy runtime migration required: {} (serve pid {}, tunnel pid {})",
            path.display(),
            pids.serve,
            pids.tunnel
        );
        println!("dry run: no processes were signaled and no state was removed");
        return Ok(());
    }

    if !serve_alive && !tunnel_alive {
        remove_legacy_pid_file(&path)?;
        println!("removed stale legacy runtime state {}", path.display());
        return Ok(());
    }

    if serve_alive {
        send_signal(pids.serve, libc::SIGTERM)?;
    }
    if tunnel_alive {
        send_signal(pids.tunnel, libc::SIGTERM)?;
    }

    for _ in 0..15 {
        if !process_exists(pids.serve) && !process_exists(pids.tunnel) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    if process_exists(pids.serve) {
        ensure_process_name(pids.serve, PROCESS_NAME, "legacy Temote supervisor")?;
        send_signal(pids.serve, libc::SIGKILL)?;
    }
    if process_exists(pids.tunnel) {
        ensure_process_name(pids.tunnel, "cloudflared", "legacy Cloudflare Tunnel")?;
        send_signal(pids.tunnel, libc::SIGKILL)?;
    }

    for _ in 0..10 {
        if !process_exists(pids.serve) && !process_exists(pids.tunnel) {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(
        !process_exists(pids.serve) && !process_exists(pids.tunnel),
        "legacy Temote runtime did not stop cleanly; refusing to remove {}",
        path.display()
    );

    remove_legacy_pid_file(&path)?;
    println!("migrated legacy Temote runtime state");
    println!("configuration and independently running local sessions were left unchanged");
    println!("next: run `temote-mcp up --profile cloudflare`");
    Ok(())
}

fn read_legacy_up_pids(path: &Path) -> Result<Option<LegacyUpPids>> {
    let file = match open_readonly_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open legacy runtime state {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect legacy runtime state {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "legacy runtime state must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_LEGACY_PID_FILE_BYTES as u64,
        "legacy runtime state exceeds {MAX_LEGACY_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_LEGACY_PID_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read legacy runtime state {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_LEGACY_PID_FILE_BYTES,
        "legacy runtime state exceeds {MAX_LEGACY_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let raw = std::str::from_utf8(&bytes).context("legacy runtime state is not valid UTF-8")?;
    Ok(Some(parse_legacy_up_pids(raw)?))
}

fn parse_legacy_up_pids(raw: &str) -> Result<LegacyUpPids> {
    let fields: Vec<_> = raw.split_whitespace().collect();
    anyhow::ensure!(
        fields.len() == 2,
        "legacy runtime state must contain exactly two positive PIDs"
    );
    Ok(LegacyUpPids {
        serve: parse_pid(fields[0]).context("invalid legacy Temote supervisor PID")?,
        tunnel: parse_pid(fields[1]).context("invalid legacy Cloudflare Tunnel PID")?,
    })
}

fn ensure_process_name(pid: i32, expected: &str, label: &str) -> Result<()> {
    let actual = process_name(pid)?;
    anyhow::ensure!(
        actual.as_deref() == Some(expected),
        "{label} PID {pid} belongs to an unexpected process ({actual:?}); refusing to signal it"
    );
    Ok(())
}

fn remove_legacy_pid_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub async fn down() -> Result<()> {
    let pid_file = pid_file(false)?;
    let mut pid_handle = match open_readonly_nofollow(&pid_file) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no temote-mcp up process is recorded");
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", pid_file.display()));
        }
    };
    let pid = read_pid_from_open_file(&mut pid_handle, &pid_file)
        .with_context(|| format!("failed to read {}", pid_file.display()))?;
    if try_acquire_pid_lock(&pid_handle)? {
        let _ = std::fs::remove_file(&pid_file);
        remove_runtime_state_if_safe()?;
        println!("recorded temote-mcp process is not running");
        return Ok(());
    }
    anyhow::ensure!(
        is_temote_process(pid)?,
        "PID file is locked by an unexpected process; refusing to signal PID {pid}"
    );

    let child_pids = child_processes(pid);
    for child_pid in &child_pids {
        if process_exists(*child_pid) {
            let _ = send_signal(*child_pid, libc::SIGTERM);
        }
    }
    send_signal(pid, libc::SIGTERM)?;

    for _ in 0..15 {
        if !process_exists(pid) && child_pids.iter().all(|child| !process_exists(*child)) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    for child_pid in child_pids {
        if process_exists(child_pid) {
            let _ = send_signal(child_pid, libc::SIGKILL);
        }
    }
    if process_exists(pid) {
        let _ = send_signal(pid, libc::SIGKILL);
    }

    let _ = std::fs::remove_file(&pid_file);
    remove_runtime_state_if_safe()?;
    Ok(())
}

struct PidFile {
    path: PathBuf,
    _file: std::fs::File,
}

impl PidFile {
    fn create(path: &Path) -> Result<Self> {
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(mut file) => {
                    acquire_pid_lock(&file)
                        .with_context(|| format!("failed to lock {}", path.display()))?;
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file.set_permissions(std::fs::Permissions::from_mode(0o600))
                            .with_context(|| format!("failed to protect {}", path.display()))?;
                    }
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("failed to sync {}", path.display()))?;
                    return Ok(Self {
                        path: path.to_owned(),
                        _file: file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let mut existing = open_readonly_nofollow(path).with_context(|| {
                        format!("cannot safely open existing PID file {}", path.display())
                    })?;
                    let _pid = read_pid_from_open_file(&mut existing, path).with_context(|| {
                        format!("cannot safely inspect existing PID file {}", path.display())
                    })?;
                    if !try_acquire_pid_lock(&existing)? {
                        anyhow::bail!("temote-mcp is already running; use temote-mcp down first");
                    }
                    std::fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale PID file {}", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }

        anyhow::bail!("failed to create {}", path.display())
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct RuntimeStateFile {
    path: PathBuf,
}

impl RuntimeStateFile {
    fn create(path: &Path, state: &DirectIngressRuntimeState) -> Result<Self> {
        let bytes = serde_json::to_vec_pretty(state)?;
        anyhow::ensure!(
            bytes.len() <= MAX_RUNTIME_STATE_BYTES,
            "direct ingress runtime state exceeds {MAX_RUNTIME_STATE_BYTES} bytes"
        );
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    validate_runtime_state_file(path)?;
                    std::fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale runtime state {}", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create direct ingress runtime state {}",
                            path.display()
                        )
                    });
                }
            }
        }
        anyhow::bail!(
            "failed to create direct ingress runtime state {}",
            path.display()
        )
    }
}

impl Drop for RuntimeStateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn remove_runtime_state_if_safe() -> Result<()> {
    let path = runtime_state_file(false)?;
    match validate_runtime_state_file(&path) {
        Ok(()) => std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display())),
        Err(error) => {
            if std::fs::symlink_metadata(&path)
                .is_err_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn openai_restart_context_keys(profile: Profile) -> Vec<String> {
    if profile != Profile::Openai {
        return Vec::new();
    }
    for name in ["CONTROL_PLANE_API_KEY", "OPENAI_API_KEY"] {
        if std::env::var_os(name).is_some_and(|value| !value.is_empty()) {
            return vec![name.to_owned()];
        }
    }
    Vec::new()
}

fn runtime_state_file(create_parent: bool) -> Result<PathBuf> {
    let directory = runtime_directory()?;
    if create_parent {
        ensure_runtime_directory(&directory)?;
    }
    Ok(directory.join(RUNTIME_STATE_FILE_NAME))
}

fn ensure_runtime_directory(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "runtime directory must be a real directory: {}",
        directory.display()
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", directory.display()))?;
    Ok(())
}

fn validate_runtime_state_file(path: &Path) -> Result<()> {
    let file = open_readonly_nofollow(path).with_context(|| {
        format!(
            "cannot open direct ingress runtime state {}",
            path.display()
        )
    })?;
    let metadata = file.metadata().with_context(|| {
        format!(
            "cannot inspect direct ingress runtime state {}",
            path.display()
        )
    })?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "direct ingress runtime state must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_RUNTIME_STATE_BYTES as u64,
        "direct ingress runtime state exceeds {MAX_RUNTIME_STATE_BYTES} bytes"
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "direct ingress runtime state must be owner-only (mode {mode:04o})"
    );
    Ok(())
}

fn read_runtime_state(path: &Path) -> Result<Option<DirectIngressRuntimeState>> {
    let file = match open_readonly_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "cannot open direct ingress runtime state {}",
                    path.display()
                )
            });
        }
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "direct ingress runtime state must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_RUNTIME_STATE_BYTES as u64,
        "direct ingress runtime state exceeds {MAX_RUNTIME_STATE_BYTES} bytes"
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "direct ingress runtime state must be owner-only (mode {mode:04o})"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_RUNTIME_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_RUNTIME_STATE_BYTES,
        "direct ingress runtime state exceeds {MAX_RUNTIME_STATE_BYTES} bytes"
    );
    let state: DirectIngressRuntimeState =
        serde_json::from_slice(&bytes).context("invalid direct ingress runtime state")?;
    anyhow::ensure!(
        state.schema == RUNTIME_STATE_SCHEMA_VERSION,
        "unsupported direct ingress runtime state schema {}",
        state.schema
    );
    anyhow::ensure!(
        state.ownership == RUNTIME_STATE_OWNERSHIP,
        "unsupported direct ingress runtime ownership {:?}",
        state.ownership
    );
    if let Some(public_url) = state.public_url.as_deref() {
        crate::provider::PublicEndpoint::parse(public_url)
            .context("direct ingress runtime state contains an invalid public endpoint")?;
    }
    Ok(Some(state))
}

fn probe_address(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
        }
        _ => addr,
    }
}

async fn probe_origin_health(addr: SocketAddr) -> Result<()> {
    let addr = probe_address(addr);
    let probe = async {
        let mut stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("cannot connect to direct ingress origin at {addr}"))?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut bytes = Vec::new();
        stream
            .take(MAX_ORIGIN_HEALTH_BYTES as u64)
            .read_to_end(&mut bytes)
            .await?;
        let response = String::from_utf8_lossy(&bytes);
        anyhow::ensure!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
            "direct ingress origin health endpoint did not return HTTP 200"
        );
        Ok(())
    };
    tokio::time::timeout(ORIGIN_HEALTH_TIMEOUT, probe)
        .await
        .context("direct ingress origin health probe timed out")??;
    Ok(())
}

fn parse_runtime_profile(state: &DirectIngressRuntimeState) -> Result<Profile> {
    state
        .profile
        .parse::<Profile>()
        .map_err(anyhow::Error::msg)
        .context("direct ingress runtime state has an invalid profile")
}

fn valid_openai_tunnel_id(value: &str) -> bool {
    value.strip_prefix("tunnel_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_restart_recipe(state: &DirectIngressRuntimeState) -> Result<()> {
    let profile = parse_runtime_profile(state)?;
    match profile {
        Profile::Cloudflare => {
            let public_url = state
                .public_url
                .as_deref()
                .context("Cloudflare direct ingress state has no public endpoint")?;
            crate::provider::PublicEndpoint::parse(public_url)?;
            let token_file = state
                .tunnel_token_file
                .as_deref()
                .context("Cloudflare direct ingress state has no tunnel-token file reference")?;
            ensure_tunnel_token_file(token_file)?;
        }
        Profile::Tailscale => {
            let public_url = state
                .public_url
                .as_deref()
                .context("Tailscale direct ingress state has no public endpoint")?;
            crate::provider::PublicEndpoint::parse(public_url)?;
            anyhow::ensure!(
                state.tunnel_token_file.is_none(),
                "Tailscale direct ingress state unexpectedly contains a tunnel-token reference"
            );
        }
        Profile::Openai => {
            anyhow::ensure!(
                state.public_url.is_none() && state.tunnel_token_file.is_none(),
                "OpenAI direct ingress state contains public-ingress-only fields"
            );
            let tunnel_id = state
                .openai_tunnel_id
                .as_deref()
                .context("OpenAI direct ingress state has no tunnel identity")?;
            anyhow::ensure!(
                valid_openai_tunnel_id(tunnel_id),
                "OpenAI direct ingress state has an invalid tunnel identity"
            );
            anyhow::ensure!(
                state.restart_recipe_available && !state.restart_context_keys.is_empty(),
                "OpenAI direct ingress was started with an interactive-only runtime credential; automatic restart would require persisting a secret"
            );
            for key in &state.restart_context_keys {
                anyhow::ensure!(
                    matches!(key.as_str(), "CONTROL_PLANE_API_KEY" | "OPENAI_API_KEY"),
                    "OpenAI direct ingress state contains an unsupported restart-context key"
                );
                anyhow::ensure!(
                    std::env::var_os(key).is_some_and(|value| !value.is_empty()),
                    "OpenAI direct ingress restart context is unavailable for key: {key}"
                );
            }
        }
    }
    Ok(())
}

fn direct_ingress_restart_reason(
    source_version: &str,
    target_version: &str,
    healthy: bool,
) -> Option<&'static str> {
    if source_version != target_version && !healthy {
        Some("binary version differs and origin health check failed")
    } else if source_version != target_version {
        Some("binary version differs")
    } else if !healthy {
        Some("origin health check failed")
    } else {
        None
    }
}

pub async fn prepare_direct_ingress_upgrade(
    target_version: &str,
) -> Result<PreparedDirectIngressUpgrade> {
    let inactive = |health: &str, reason: Option<String>| PreparedDirectIngressUpgrade {
        plan: DirectIngressUpgradePlan {
            active: false,
            source_version: None,
            target_version: target_version.to_owned(),
            profile: None,
            addr: None,
            action: "inactive".to_owned(),
            health: health.to_owned(),
            reason,
        },
        state: None,
    };
    let pid_path = pid_file(false)?;
    let mut pid_handle = match open_readonly_nofollow(&pid_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(inactive("inactive", None));
        }
        Err(error) => return Err(error).context("failed to inspect direct ingress PID file"),
    };
    let pid = read_pid_from_open_file(&mut pid_handle, &pid_path)?;
    if try_acquire_pid_lock(&pid_handle)? {
        return Ok(inactive(
            "stale",
            Some(format!(
                "stale direct ingress PID file records process {pid}"
            )),
        ));
    }
    anyhow::ensure!(
        is_temote_process(pid)?,
        "direct ingress PID file is owned by an unexpected process"
    );
    let Some(state) = read_runtime_state(&runtime_state_file(false)?)? else {
        return Ok(PreparedDirectIngressUpgrade {
            plan: DirectIngressUpgradePlan {
                active: true,
                source_version: None,
                target_version: target_version.to_owned(),
                profile: None,
                addr: None,
                action: "blocked".to_owned(),
                health: "unknown_legacy_runtime".to_owned(),
                reason: Some(
                    "running direct ingress predates durable restart metadata; run `temote-mcp down` and start `temote-mcp up` once before automatic upgrade"
                        .to_owned(),
                ),
            },
            state: None,
        });
    };
    anyhow::ensure!(
        state.pid == pid,
        "direct ingress runtime state PID does not match the live PID file"
    );
    let health_result = probe_origin_health(state.addr).await;
    let (healthy, health) = match health_result {
        Ok(()) => (true, "healthy".to_owned()),
        Err(error) => (false, format!("unhealthy: {error:#}")),
    };
    let restart_reason = direct_ingress_restart_reason(&state.version, target_version, healthy);
    let (action, reason) = if restart_reason.is_none() {
        ("untouched".to_owned(), None)
    } else {
        match validate_restart_recipe(&state) {
            Ok(()) => ("restart".to_owned(), restart_reason.map(str::to_owned)),
            Err(error) => ("blocked".to_owned(), Some(format!("{error:#}"))),
        }
    };
    Ok(PreparedDirectIngressUpgrade {
        plan: DirectIngressUpgradePlan {
            active: true,
            source_version: Some(state.version.clone()),
            target_version: target_version.to_owned(),
            profile: Some(state.profile.clone()),
            addr: Some(state.addr),
            action,
            health,
            reason,
        },
        state: Some(state),
    })
}

fn spawn_direct_ingress(executable: &Path, state: &DirectIngressRuntimeState) -> Result<()> {
    let profile = parse_runtime_profile(state)?;
    let mut command = Command::new(executable);
    command
        .arg("up")
        .arg("--profile")
        .arg(profile.name())
        .arg("--addr")
        .arg(state.addr.to_string());
    if let Some(public_url) = state.public_url.as_deref() {
        command.arg("--public-url").arg(public_url);
    }
    if let Some(token_file) = state.tunnel_token_file.as_deref() {
        command.arg("--tunnel-token-file").arg(token_file);
    }
    if let Some(tunnel_id) = state.openai_tunnel_id.as_deref() {
        command.env("CONTROL_PLANE_TUNNEL_ID", tunnel_id);
    }
    if let Some(tunnel_client_bin) = state.tunnel_client_bin.as_deref() {
        command.env("TUNNEL_CLIENT_BIN", tunnel_client_bin);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().with_context(|| {
        format!(
            "failed to restart direct ingress with {}",
            executable.display()
        )
    })?;
    Ok(())
}

pub async fn apply_direct_ingress_upgrade(
    prepared: PreparedDirectIngressUpgrade,
    executable: &Path,
) -> Result<DirectIngressUpgradePlan> {
    match prepared.plan.action.as_str() {
        "inactive" => return Ok(prepared.plan),
        "untouched" | "restart" => {}
        "blocked" => anyhow::bail!(
            "direct ingress upgrade is blocked: {}",
            prepared.plan.reason.as_deref().unwrap_or("unknown blocker")
        ),
        other => anyhow::bail!("unsupported direct ingress upgrade action {other:?}"),
    }
    let expected = prepared
        .state
        .as_ref()
        .context("direct ingress restart plan has no runtime state")?;
    let current = prepare_direct_ingress_upgrade(&prepared.plan.target_version).await?;
    if current.plan.action == "untouched" {
        return Ok(current.plan);
    }
    if let Some(blocker) = current.blocker() {
        anyhow::bail!("direct ingress became blocked after supervisor handoff: {blocker}");
    }
    anyhow::ensure!(
        current.plan.action == "restart" && current.state.as_ref() == Some(expected),
        "direct ingress state changed after upgrade preflight; refusing to restart a different process"
    );
    validate_restart_recipe(expected)?;
    down().await?;
    spawn_direct_ingress(executable, expected)?;

    let deadline = tokio::time::Instant::now() + INGRESS_RESTART_TIMEOUT;
    loop {
        let status = prepare_direct_ingress_upgrade(&prepared.plan.target_version).await?;
        if status.plan.action == "untouched"
            && status.plan.source_version.as_deref() == Some(prepared.plan.target_version.as_str())
            && status.plan.health == "healthy"
        {
            let mut result = status.plan;
            result.action = "restarted".to_owned();
            result.reason = Some("restart completed and /healthz returned HTTP 200".to_owned());
            return Ok(result);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "direct ingress restart did not become healthy within the bounded verification window"
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn pid_file(create_parent: bool) -> Result<PathBuf> {
    let directory = runtime_directory()?;
    if create_parent {
        ensure_runtime_directory(&directory)?;
    }
    Ok(directory.join(PID_FILE_NAME))
}

fn runtime_directory() -> Result<PathBuf> {
    if let Some(path) = env_path("TEMOTE_MCP_RUNTIME_DIR") {
        return Ok(path.join("temote-mcp"));
    }
    if let Some(path) = env_path("XDG_RUNTIME_DIR") {
        return Ok(path.join("temote-mcp"));
    }
    crate::platform_paths::home_dir()
        .map(|home| home.join(".cache").join("temote-mcp"))
        .context("could not determine a runtime directory")
}

fn default_tunnel_token_file() -> Result<PathBuf> {
    crate::platform_paths::home_dir()
        .map(|home| home.join(".config").join("temote-mcp").join("tunnel-token"))
        .context("could not determine HOME for the default tunnel token file")
}

fn ensure_tunnel_token_file(path: &Path) -> Result<()> {
    let mut file = open_readonly_nofollow(path)
        .with_context(|| format!("cannot open tunnel token file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect tunnel token file {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "tunnel token file must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_TUNNEL_TOKEN_BYTES,
        "tunnel token file must contain 1..={MAX_TUNNEL_TOKEN_BYTES} bytes: {}",
        path.display()
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            private_unix_mode(mode),
            "tunnel token file must not be accessible by group or other users (mode {mode:04o}): {}",
            path.display()
        );
    }
    let mut probe = [0u8; 1];
    let read = file
        .read(&mut probe)
        .with_context(|| format!("tunnel token file is not readable: {}", path.display()))?;
    anyhow::ensure!(read == 1, "tunnel token file is empty: {}", path.display());
    Ok(())
}

fn open_readonly_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

#[cfg(unix)]
fn private_unix_mode(mode: u32) -> bool {
    mode & 0o077 == 0
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
fn read_pid_file(path: &Path) -> Result<i32> {
    let mut file = open_readonly_nofollow(path)
        .with_context(|| format!("cannot open PID file {}", path.display()))?;
    read_pid_from_open_file(&mut file, path)
}

fn read_pid_from_open_file(file: &mut std::fs::File, path: &Path) -> Result<i32> {
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect PID file {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "PID file is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_PID_FILE_BYTES as u64,
        "PID file exceeds {MAX_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_PID_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read PID file {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_PID_FILE_BYTES,
        "PID file exceeds {MAX_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let raw = std::str::from_utf8(&bytes).context("PID file is not valid UTF-8")?;
    parse_pid(raw)
}

fn try_acquire_pid_lock(file: &std::fs::File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    let raw = error.raw_os_error();
    if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
        return Ok(false);
    }
    Err(error).context("failed to inspect PID file lock")
}

fn acquire_pid_lock(file: &std::fs::File) -> Result<()> {
    anyhow::ensure!(
        try_acquire_pid_lock(file)?,
        "PID file is already locked by another process"
    );
    Ok(())
}

fn parse_pid(raw: &str) -> Result<i32> {
    let pid = raw
        .trim()
        .parse::<i32>()
        .context("invalid temote-mcp PID file")?;
    anyhow::ensure!(pid > 0, "invalid temote-mcp PID file");
    Ok(pid)
}

fn is_temote_process(pid: i32) -> Result<bool> {
    Ok(process_name(pid)?.as_deref() == Some(PROCESS_NAME))
}

fn process_name(pid: i32) -> Result<Option<String>> {
    let output = Command::new(PS_COMMAND)
        .env_clear()
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .context("failed to inspect the temote-mcp process")?;
    if !output.status.success() {
        return Ok(None);
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let name = output.trim();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(executable_name(name).to_owned()))
}

fn executable_name(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(command)
}

fn child_processes(parent_pid: i32) -> Vec<i32> {
    let Ok(output) = Command::new(PGREP_COMMAND)
        .env_clear()
        .args(["-P", &parent_pid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .collect()
}

fn process_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_signal(pid: i32, signal: libc::c_int) -> Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to signal process {pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn parses_only_exact_legacy_pid_pairs() {
        assert_eq!(
            parse_legacy_up_pids("123 456\n").unwrap(),
            LegacyUpPids {
                serve: 123,
                tunnel: 456,
            }
        );
        for invalid in ["", "123", "123 456 789", "0 456", "123 -1", "abc 456"] {
            assert!(parse_legacy_up_pids(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn generated_legacy_pid_pairs_match_positive_i32_reference_model() -> noprop::TestResult {
        test_support::run(0x4c45_4741_4359_5049, test_support::DEFAULT_CASES, |ctx| {
            let field_count = noprop::sample_usize_in(ctx, 0..=4);
            let fields = (0..field_count)
                .map(|_| match noprop::sample_usize_in(ctx, 0..=4) {
                    0 => (1 + (noprop::sample_u32(ctx) % i32::MAX as u32)).to_string(),
                    1 => "0".to_owned(),
                    2 => format!("-{}", 1 + (noprop::sample_u32(ctx) % i32::MAX as u32)),
                    3 => (i32::MAX as u64 + 1 + (noprop::sample_u32(ctx) as u64)).to_string(),
                    _ => format!("bad{}", noprop::sample_u32(ctx)),
                })
                .collect::<Vec<_>>();
            let separator = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => " ",
                1 => "\t",
                2 => "\n",
                _ => "\r\n",
            };
            let leading = if noprop::sample_bool(ctx) {
                separator
            } else {
                ""
            };
            let trailing = if noprop::sample_bool(ctx) {
                separator
            } else {
                ""
            };
            let raw = format!("{leading}{}{trailing}", fields.join(separator));

            let parsed = raw.split_whitespace().collect::<Vec<_>>();
            let expected = (parsed.len() == 2)
                .then(|| {
                    let serve = parsed[0].parse::<i32>().ok().filter(|pid| *pid > 0)?;
                    let tunnel = parsed[1].parse::<i32>().ok().filter(|pid| *pid > 0)?;
                    Some(LegacyUpPids { serve, tunnel })
                })
                .flatten();
            assert_eq!(
                parse_legacy_up_pids(&raw).ok(),
                expected,
                "legacy PID pair mismatch for {raw:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn legacy_pid_reader_rejects_symlink_and_oversize() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pids");
        let target = root.path().join("target");
        std::fs::write(&target, b"123 456\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::create_dir(&path).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(&path, vec![b'1'; MAX_LEGACY_PID_FILE_BYTES + 1]).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::write(&path, b"123 456\n").unwrap();
        assert_eq!(
            read_legacy_up_pids(&path).unwrap(),
            Some(LegacyUpPids {
                serve: 123,
                tunnel: 456,
            })
        );
    }

    #[test]
    fn process_name_guard_rejects_unexpected_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let result = ensure_process_name(pid, "cloudflared", "legacy Cloudflare Tunnel");
        assert!(result.is_err());
        assert!(process_exists(pid));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reads_small_regular_pid_files_and_rejects_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        std::fs::write(&path, b"123\n").unwrap();
        assert_eq!(read_pid_file(&path).unwrap(), 123);

        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_PID_FILE_BYTES as u64 + 1).unwrap();
        assert!(
            read_pid_file(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("PID file exceeds")
        );
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_rejects_symlink_and_malformed_existing_state_without_deleting_it() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let path = root.path().join("up.pid");
        std::fs::write(&target, b"123\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_pid_file(&path).is_err());
        assert!(PidFile::create(&path).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, b"not-a-pid\n").unwrap();
        assert!(PidFile::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not-a-pid\n");
    }

    fn wait_for_pid_lock_release(file: &std::fs::File) -> Result<bool> {
        for _ in 0..50 {
            if try_acquire_pid_lock(file)? {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(false)
    }

    #[test]
    fn generated_pid_file_lock_tracks_owner_lifetime() -> noprop::TestResult {
        test_support::run(0x5049_444c_4f43_4b01, 128, |ctx| {
            let root = tempfile::tempdir().unwrap();
            let path = root
                .path()
                .join(format!("up-{:x}.pid", noprop::sample_u64(ctx)));
            let holder = PidFile::create(&path).unwrap();
            let probe = open_readonly_nofollow(&path).unwrap();
            let drop_before_probe = noprop::sample_bool(ctx);
            if drop_before_probe {
                drop(holder);
                assert!(
                    wait_for_pid_lock_release(&probe).unwrap(),
                    "released PID file lock remained busy"
                );
            } else {
                assert!(
                    !try_acquire_pid_lock(&probe).unwrap(),
                    "live PID file lock was unexpectedly acquirable"
                );
                drop(holder);
                assert!(
                    wait_for_pid_lock_release(&probe).unwrap(),
                    "PID file lock did not release after owner drop"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn unlocked_pid_file_is_stale_even_when_recorded_pid_is_live() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();

        let holder = PidFile::create(&path).unwrap();
        assert_eq!(read_pid_file(&path).unwrap(), std::process::id() as i32);
        drop(holder);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        {
            let _pid_file = PidFile::create(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert_eq!(read_pid_file(&path).unwrap(), std::process::id() as i32);
        }
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_token_file_rejects_symlink_public_mode_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        std::fs::write(&path, b"token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_ok());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_TUNNEL_TOKEN_BYTES + 1).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        let target = root.path().join("target-token");
        std::fs::write(&target, b"token").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
    }

    #[test]
    fn generated_pid_file_byte_limits_match_reference_model() -> noprop::TestResult {
        test_support::run(0x5049_4442_4f55_4e44, 256, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=MAX_PID_FILE_BYTES + 8);
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("up.pid");
            std::fs::write(&path, vec![b'1'; len]).unwrap();
            let result = read_pid_file(&path);
            let syntactically_valid = len > 0
                && len <= 10
                && std::str::from_utf8(&vec![b'1'; len])
                    .ok()
                    .and_then(|raw| raw.parse::<i32>().ok())
                    .is_some_and(|pid| pid > 0);
            assert_eq!(
                result.is_ok(),
                len <= MAX_PID_FILE_BYTES && syntactically_valid,
                "len={len} result={result:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_private_file_modes_match_owner_only_reference() -> noprop::TestResult {
        test_support::run(0x4c49_4645_4d4f_4445, test_support::DEFAULT_CASES, |ctx| {
            let mode = u32::from(noprop::sample_u16(ctx)) & 0o777;
            assert_eq!(private_unix_mode(mode), mode & 0o077 == 0);
            Ok(())
        })
    }

    #[test]
    fn parses_only_positive_pids() {
        assert_eq!(parse_pid("123\n").unwrap(), 123);
        assert!(parse_pid("").is_err());
        assert!(parse_pid("0").is_err());
        assert!(parse_pid("-1").is_err());
    }

    #[test]
    fn process_inspection_uses_absolute_system_utilities() {
        assert!(Path::new(PS_COMMAND).is_absolute());
        assert!(Path::new(PGREP_COMMAND).is_absolute());
        #[cfg(target_os = "macos")]
        {
            assert_eq!(PS_COMMAND, "/bin/ps");
            assert_eq!(PGREP_COMMAND, "/usr/bin/pgrep");
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(PS_COMMAND, "/usr/bin/ps");
            assert_eq!(PGREP_COMMAND, "/usr/bin/pgrep");
        }
    }

    #[test]
    fn extracts_the_executable_name() {
        assert_eq!(executable_name("temote-mcp"), "temote-mcp");
        assert_eq!(executable_name("/usr/local/bin/temote-mcp"), "temote-mcp");
    }

    #[test]
    fn generated_pid_strings_match_positive_i32_model() -> noprop::TestResult {
        test_support::run(0x5049_4446_494c_4501, test_support::DEFAULT_CASES, |ctx| {
            let raw = match noprop::sample_usize_in(ctx, 0..=5) {
                0 => noprop::sample_u32(ctx).to_string(),
                1 => format!("-{}", noprop::sample_u32(ctx)),
                2 => "0".to_owned(),
                3 => format!(" {} \n", 1 + noprop::sample_u16(ctx)),
                4 => test_support::safe_component(ctx),
                _ => format!(
                    "{}{}",
                    u64::from(i32::MAX as u32) + 1,
                    noprop::sample_u16(ctx)
                ),
            };
            let expected = raw.trim().parse::<i32>().ok().filter(|pid| *pid > 0);
            assert_eq!(parse_pid(&raw).ok(), expected, "raw={raw:?}");
            Ok(())
        })
    }

    #[test]
    fn generated_executable_paths_return_last_component() -> noprop::TestResult {
        test_support::run(0x5052_4f43_4e41_4d45, 512, |ctx| {
            let executable = test_support::safe_component(ctx);
            let path = format!(
                "/{}/{}/{}",
                test_support::safe_component(ctx),
                test_support::safe_component(ctx),
                executable
            );
            assert_eq!(executable_name(&path), executable);
            assert_eq!(executable_name(&executable), executable);
            Ok(())
        })
    }
    fn direct_ingress_state_fixture() -> DirectIngressRuntimeState {
        DirectIngressRuntimeState {
            schema: RUNTIME_STATE_SCHEMA_VERSION,
            ownership: RUNTIME_STATE_OWNERSHIP.to_owned(),
            pid: std::process::id() as i32,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            profile: "tailscale".to_owned(),
            host_id: "test-host".to_owned(),
            addr: "127.0.0.1:8791".parse().unwrap(),
            public_url: Some("https://example.test".to_owned()),
            tunnel_token_file: None,
            openai_tunnel_id: None,
            tunnel_client_bin: None,
            restart_context_keys: Vec::new(),
            restart_recipe_available: true,
        }
    }

    #[test]
    fn direct_ingress_restart_reason_only_restarts_when_needed() {
        assert_eq!(direct_ingress_restart_reason("1", "1", true), None);
        assert_eq!(
            direct_ingress_restart_reason("1", "2", true),
            Some("binary version differs")
        );
        assert_eq!(
            direct_ingress_restart_reason("1", "1", false),
            Some("origin health check failed")
        );
        assert_eq!(
            direct_ingress_restart_reason("1", "2", false),
            Some("binary version differs and origin health check failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_ingress_runtime_state_is_private_bounded_and_secret_free() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.state.json");
        let mut state = direct_ingress_state_fixture();
        state.profile = "openai".to_owned();
        state.public_url = None;
        state.openai_tunnel_id = Some("tunnel_0123456789abcdef0123456789abcdef".to_owned());
        state.restart_context_keys = vec!["CONTROL_PLANE_API_KEY".to_owned()];
        let secret = "credential-sentinel-must-not-persist";
        {
            let _holder = RuntimeStateFile::create(&path, &state).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.len() <= MAX_RUNTIME_STATE_BYTES);
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("CONTROL_PLANE_API_KEY"));
            assert!(!text.contains(secret));
            assert_eq!(read_runtime_state(&path).unwrap(), Some(state.clone()));
        }
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn direct_ingress_runtime_state_rejects_symlink_public_mode_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.state.json");
        let target = root.path().join("target.json");
        let state = direct_ingress_state_fixture();
        std::fs::write(&target, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_runtime_state(&path).is_err());
        assert!(RuntimeStateFile::create(&path, &state).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_runtime_state(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, vec![b'x'; MAX_RUNTIME_STATE_BYTES + 1]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_runtime_state(&path).is_err());
    }

    #[test]
    fn interactive_openai_restart_recipe_fails_closed_without_secret_storage() {
        let mut state = direct_ingress_state_fixture();
        state.profile = "openai".to_owned();
        state.public_url = None;
        state.openai_tunnel_id = Some("tunnel_0123456789abcdef0123456789abcdef".to_owned());
        state.restart_recipe_available = false;
        let error = validate_restart_recipe(&state).unwrap_err();
        assert!(format!("{error:#}").contains("interactive-only runtime credential"));
        let serialized = serde_json::to_string(&state).unwrap();
        assert!(!serialized.contains("API key:"));
    }

    #[test]
    fn health_probe_uses_loopback_for_unspecified_bind_addresses() {
        assert_eq!(
            probe_address("0.0.0.0:8791".parse().unwrap()),
            "127.0.0.1:8791".parse().unwrap()
        );
        assert_eq!(
            probe_address("[::]:8791".parse().unwrap()),
            "[::1]:8791".parse().unwrap()
        );
    }

    #[tokio::test]
    async fn direct_ingress_health_probe_requires_http_200() {
        async fn serve_once(status: &'static str) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await.unwrap();
                assert!(String::from_utf8_lossy(&request).contains("GET /healthz HTTP/1.1"));
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            addr
        }

        let healthy = serve_once("200 OK").await;
        probe_origin_health(healthy).await.unwrap();
        let unhealthy = serve_once("503 Service Unavailable").await;
        assert!(probe_origin_health(unhealthy).await.is_err());
    }
}
