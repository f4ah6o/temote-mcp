use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use zeroize::Zeroizing;

const TUNNEL_ID_PREFIX: &str = "tunnel_";
const TUNNEL_ID_HEX_LEN: usize = 32;
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(15);
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SETUP_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_TUNNEL_NAME_BYTES: usize = 256;
const MAX_TUNNEL_DESCRIPTION_BYTES: usize = 4096;
const MAX_SCOPE_IDS_PER_KIND: usize = 128;
const DEFAULT_CONTROL_PLANE_BASE_URL: &str = "https://api.openai.com";
const OPENAI_CONFIG_FILE: &str = "openai.env";

#[derive(Clone, Debug)]
pub struct SetupOptions {
    pub name: String,
    pub description: String,
    pub organization_ids: Vec<String>,
    pub workspace_ids: Vec<String>,
    pub config_file: Option<PathBuf>,
    pub force: bool,
}

#[derive(Debug)]
pub struct SetupResult {
    pub tunnel_id: String,
    pub config_file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct TunnelRecord {
    id: String,
}

#[derive(Debug, Serialize)]
struct TunnelCreateRequest {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    organization_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    workspace_ids: Vec<String>,
}

pub struct OpenAiTunnelConfig {
    pub tunnel_id: String,
    pub runtime_key_env: &'static str,
    pub binary: OsString,
}

enum RuntimeCredential {
    Inherited(&'static str),
    Prompted(Zeroizing<String>),
}

pub fn config_from_env() -> Result<OpenAiTunnelConfig> {
    let tunnel_id = configured_tunnel_id()?;
    anyhow::ensure!(
        valid_tunnel_id(&tunnel_id),
        "CONTROL_PLANE_TUNNEL_ID must match tunnel_<32 lowercase hexadecimal characters>"
    );

    let runtime_key_env = inherited_runtime_key_env().context(
        "CONTROL_PLANE_API_KEY is required for non-interactive OpenAI diagnostics (OPENAI_API_KEY is accepted only as the official tunnel-client fallback)",
    )?;

    let binary = tunnel_client_binary();

    Ok(OpenAiTunnelConfig {
        tunnel_id,
        runtime_key_env,
        binary,
    })
}

pub async fn setup(options: SetupOptions) -> Result<SetupResult> {
    anyhow::ensure!(
        !options.organization_ids.is_empty() || !options.workspace_ids.is_empty(),
        "at least one --organization-id or --workspace-id is required"
    );
    let name = options.name.trim();
    anyhow::ensure!(!name.is_empty(), "--name must not be empty");
    anyhow::ensure!(
        name.len() <= MAX_TUNNEL_NAME_BYTES,
        "--name exceeds {MAX_TUNNEL_NAME_BYTES} bytes"
    );
    let description = options.description.trim();
    anyhow::ensure!(!description.is_empty(), "--description must not be empty");
    anyhow::ensure!(
        description.len() <= MAX_TUNNEL_DESCRIPTION_BYTES,
        "--description exceeds {MAX_TUNNEL_DESCRIPTION_BYTES} bytes"
    );

    let config_file = options.config_file.unwrap_or(default_config_file()?);
    prepare_config_parent(&config_file)?;
    if !options.force
        && let Some(existing) = read_configured_tunnel_id(&config_file)?
    {
        anyhow::bail!(
            "OpenAI tunnel is already configured as {existing} in {}; use --force only when intentionally creating and selecting a new tunnel",
            config_file.display()
        );
    }

    let admin_key = secret_from_env_or_tty(
        "OPENAI_ADMIN_KEY",
        "OpenAI Admin API key: ",
        "OpenAI Admin API key",
    )?;
    let request = TunnelCreateRequest {
        name: name.to_owned(),
        description: description.to_owned(),
        organization_ids: normalized_scope_ids(options.organization_ids)?,
        workspace_ids: normalized_scope_ids(options.workspace_ids)?,
    };
    let client = reqwest::Client::builder()
        .timeout(SETUP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build OpenAI tunnel setup HTTP client")?;
    let base_url = control_plane_base_url()?;
    let tunnel = create_tunnel(&client, &base_url, admin_key.as_str(), &request).await?;
    drop(admin_key);
    anyhow::ensure!(
        valid_tunnel_id(&tunnel.id),
        "OpenAI Tunnel Management API returned an invalid tunnel id"
    );
    write_configured_tunnel_id(&config_file, &tunnel.id).with_context(|| {
        format!(
            "tunnel {} was created but its ID could not be saved; record this tunnel ID before retrying",
            tunnel.id
        )
    })?;
    Ok(SetupResult {
        tunnel_id: tunnel.id,
        config_file,
    })
}

pub fn configured_tunnel_id() -> Result<String> {
    if let Ok(value) = std::env::var("CONTROL_PLANE_TUNNEL_ID") {
        let value = value.trim().to_owned();
        anyhow::ensure!(
            valid_tunnel_id(&value),
            "CONTROL_PLANE_TUNNEL_ID must match tunnel_<32 lowercase hexadecimal characters>"
        );
        return Ok(value);
    }
    let path = std::env::var_os("TEMOTE_MCP_OPENAI_CONFIG_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_config_file()?);
    read_configured_tunnel_id(&path)?.with_context(|| {
        format!(
            "CONTROL_PLANE_TUNNEL_ID is required; set it in the environment or run `temote-mcp openai setup` to create {}",
            path.display()
        )
    })
}

fn control_plane_base_url() -> Result<String> {
    let value = std::env::var("CONTROL_PLANE_BASE_URL")
        .unwrap_or_else(|_| DEFAULT_CONTROL_PLANE_BASE_URL.to_owned());
    let parsed = url::Url::parse(value.trim())
        .map_err(|error| anyhow::anyhow!("CONTROL_PLANE_BASE_URL is invalid: {error}"))?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().trim_matches('/').is_empty()
            && parsed.host_str().is_some(),
        "CONTROL_PLANE_BASE_URL must be an HTTPS origin without credentials, path, query, or fragment"
    );
    Ok(parsed.origin().ascii_serialization())
}

fn default_config_file() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|home| {
            home.join(".config")
                .join("temote-mcp")
                .join(OPENAI_CONFIG_FILE)
        })
        .context("could not determine HOME for OpenAI tunnel state")
}

fn read_configured_tunnel_id(path: &Path) -> Result<Option<String>> {
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
        Err(error) => {
            return Err(error).with_context(|| format!("cannot open {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "OpenAI tunnel config must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(metadata.len() <= 4096, "OpenAI tunnel config is too large");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            mode & 0o077 == 0,
            "OpenAI tunnel config must not be accessible by group or other users (mode {mode:04o}): {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(4096));
    Read::take(&mut file, 4097)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(bytes.len() <= 4096, "OpenAI tunnel config is too large");
    let contents = String::from_utf8(bytes).with_context(|| {
        format!(
            "OpenAI tunnel config is not valid UTF-8: {}",
            path.display()
        )
    })?;
    parse_configured_tunnel_id(&contents)
        .with_context(|| format!("invalid OpenAI tunnel config {}", path.display()))
}

fn parse_configured_tunnel_id(contents: &str) -> Result<Option<String>> {
    let mut tunnel_id = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!("config line must contain '='");
        };
        anyhow::ensure!(
            key.trim() == "CONTROL_PLANE_TUNNEL_ID",
            "OpenAI tunnel config may contain only CONTROL_PLANE_TUNNEL_ID; keep API keys in environment/secret storage"
        );
        anyhow::ensure!(tunnel_id.is_none(), "duplicate CONTROL_PLANE_TUNNEL_ID");
        let value = parse_config_value(value)?;
        anyhow::ensure!(valid_tunnel_id(value), "invalid tunnel id");
        tunnel_id = Some(value.to_owned());
    }
    Ok(tunnel_id)
}

fn parse_config_value(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.starts_with('"') || value.ends_with('"') {
        anyhow::ensure!(
            value.len() >= 2 && value.starts_with('"') && value.ends_with('"'),
            "quoted config value must have matching double quotes"
        );
        let inner = &value[1..value.len() - 1];
        anyhow::ensure!(
            !inner.contains('"'),
            "quoted config value contains an extra quote"
        );
        Ok(inner)
    } else {
        Ok(value)
    }
}

fn prepare_config_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("OpenAI tunnel config has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    let metadata = std::fs::symlink_metadata(parent)
        .with_context(|| format!("failed to inspect {}", parent.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "OpenAI tunnel config parent must be a real directory: {}",
        parent.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", parent.display()))?;
    }
    Ok(())
}

fn write_configured_tunnel_id(path: &Path, tunnel_id: &str) -> Result<()> {
    anyhow::ensure!(valid_tunnel_id(tunnel_id), "invalid tunnel id");
    prepare_config_parent(path)?;
    let parent = path
        .parent()
        .context("OpenAI tunnel config has no parent directory")?;
    let temporary = parent.join(format!(
        ".{OPENAI_CONFIG_FILE}.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to protect {}", temporary.display()))?;
        }
        writeln!(file, "CONTROL_PLANE_TUNNEL_ID={tunnel_id}")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
            .with_context(|| format!("failed to install {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn normalized_scope_ids(values: Vec<String>) -> Result<Vec<String>> {
    anyhow::ensure!(
        values.len() <= MAX_SCOPE_IDS_PER_KIND,
        "at most {MAX_SCOPE_IDS_PER_KIND} scope IDs are allowed per kind"
    );
    let mut normalized = Vec::new();
    for value in values {
        let value = value.trim();
        anyhow::ensure!(!value.is_empty(), "scope IDs must not be empty");
        anyhow::ensure!(
            value.len() <= 256
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "scope ID contains unsupported characters"
        );
        if !normalized.iter().any(|existing| existing == value) {
            normalized.push(value.to_owned());
        }
    }
    Ok(normalized)
}

async fn create_tunnel(
    client: &reqwest::Client,
    base_url: &str,
    admin_key: &str,
    request: &TunnelCreateRequest,
) -> Result<TunnelRecord> {
    let url = format!("{}/v1/tunnels", base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .bearer_auth(admin_key)
        .json(request)
        .send()
        .await
        .context("OpenAI Tunnel Management API request failed")?;
    let status = response.status();
    let body = read_bounded_response(response).await?;
    anyhow::ensure!(
        status.is_success(),
        "OpenAI Tunnel Management API returned HTTP {status}: {}",
        safe_api_error(&body)
    );
    serde_json::from_slice(&body).context("OpenAI Tunnel Management API returned invalid JSON")
}

async fn read_bounded_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_SETUP_RESPONSE_BYTES as u64,
            "OpenAI Tunnel Management API response exceeds {MAX_SETUP_RESPONSE_BYTES} bytes"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_SETUP_RESPONSE_BYTES,
            "OpenAI Tunnel Management API response exceeds {MAX_SETUP_RESPONSE_BYTES} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn safe_api_error(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "request failed (response body omitted)".to_owned();
    };
    value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .map(|message| message.chars().take(512).collect())
        .unwrap_or_else(|| "request failed".to_owned())
}

pub async fn start(origin: SocketAddr) -> Result<Child> {
    ensure_loopback(origin)?;
    let tunnel_id = configured_tunnel_id()?;
    let binary = tunnel_client_binary();
    let runtime_credential = runtime_credential(true)?;
    let mcp_url = local_mcp_url(origin);
    let mut command = Command::new(&binary);
    command
        .args([
            "run",
            "--control-plane.tunnel-id",
            &tunnel_id,
            "--mcp.server-url",
            &mcp_url,
            "--health.listen-addr",
            "127.0.0.1:0",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    configure_runtime_command(&mut command, &runtime_credential);
    let child = command.spawn().with_context(|| {
        "failed to start OpenAI Secure MCP tunnel-client; install the supported tunnel-client and provide a runtime API key"
    })?;
    drop(command);
    drop(runtime_credential);
    Ok(child)
}

fn tunnel_client_binary() -> OsString {
    std::env::var_os("TUNNEL_CLIENT_BIN")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("tunnel-client"))
}

fn inherited_runtime_key_env() -> Option<&'static str> {
    if nonempty_env("CONTROL_PLANE_API_KEY") {
        Some("CONTROL_PLANE_API_KEY")
    } else if nonempty_env("OPENAI_API_KEY") {
        Some("OPENAI_API_KEY")
    } else {
        None
    }
}

fn runtime_credential(interactive: bool) -> Result<RuntimeCredential> {
    if let Some(name) = inherited_runtime_key_env() {
        return Ok(RuntimeCredential::Inherited(name));
    }
    anyhow::ensure!(
        interactive,
        "CONTROL_PLANE_API_KEY is required (OPENAI_API_KEY is accepted only as the official tunnel-client fallback)"
    );
    Ok(RuntimeCredential::Prompted(secret_from_tty(
        "OpenAI Runtime API key: ",
        "OpenAI Runtime API key",
    )?))
}

fn configure_runtime_command(command: &mut Command, credential: &RuntimeCredential) {
    match credential {
        RuntimeCredential::Inherited(name) => restrict_runtime_credentials(command, name),
        RuntimeCredential::Prompted(secret) => {
            scrub_openai_credentials(command);
            command.env("CONTROL_PLANE_API_KEY", secret.as_str());
        }
    }
}

fn secret_from_env_or_tty(env_name: &str, prompt: &str, label: &str) -> Result<Zeroizing<String>> {
    if let Some(value) = secret_from_env_value(std::env::var_os(env_name), env_name)? {
        return Ok(value);
    }
    secret_from_tty(prompt, label)
}

fn secret_from_env_value(
    value: Option<OsString>,
    env_name: &str,
) -> Result<Option<Zeroizing<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{env_name} must be valid UTF-8"))?;
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(Zeroizing::new(value)))
}

fn secret_from_tty(prompt: &str, label: &str) -> Result<Zeroizing<String>> {
    let value = rpassword::prompt_password(prompt).with_context(|| {
        format!(
            "cannot read {label} from the controlling terminal; set the corresponding environment variable for non-interactive use"
        )
    })?;
    anyhow::ensure!(!value.is_empty(), "{label} must not be empty");
    Ok(Zeroizing::new(value))
}

pub async fn doctor_control_plane() -> Result<String> {
    let config = config_from_env()?;
    let mut command = Command::new(&config.binary);
    command
        .args(["admin", "tunnels", "get", &config.tunnel_id])
        .stdin(Stdio::null());
    configure_runtime_command(
        &mut command,
        &RuntimeCredential::Inherited(config.runtime_key_env),
    );
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let status = tokio::time::timeout(DOCTOR_TIMEOUT, command.status())
        .await
        .context("timed out while validating OpenAI Secure MCP Tunnel runtime access")?
        .context("cannot execute tunnel-client")?;
    anyhow::ensure!(
        status.success(),
        "tunnel-client could not read the configured tunnel with the runtime credential (exit status {status})"
    );
    Ok(format!(
        "configured tunnel is readable with {}",
        config.runtime_key_env
    ))
}

pub async fn binary_version() -> Result<String> {
    let binary = std::env::var_os("TUNNEL_CLIENT_BIN")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("tunnel-client"));
    let mut command = Command::new(&binary);
    scrub_openai_credentials(&mut command);
    let output = tokio::time::timeout(
        DOCTOR_TIMEOUT,
        command.arg("--version").stdin(Stdio::null()).output(),
    )
    .await
    .context("timed out while checking tunnel-client version")?
    .context("cannot execute tunnel-client")?;
    anyhow::ensure!(
        output.status.success(),
        "tunnel-client --version failed with {}",
        output.status
    );
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if value.is_empty() {
        "available".to_owned()
    } else {
        value
    })
}

fn restrict_runtime_credentials(command: &mut Command, runtime_key_env: &str) {
    command.env_remove("OPENAI_ADMIN_KEY");
    match runtime_key_env {
        "CONTROL_PLANE_API_KEY" => {
            command.env_remove("OPENAI_API_KEY");
        }
        "OPENAI_API_KEY" => {
            command.env_remove("CONTROL_PLANE_API_KEY");
        }
        _ => {
            command.env_remove("CONTROL_PLANE_API_KEY");
            command.env_remove("OPENAI_API_KEY");
        }
    }
}

fn scrub_openai_credentials(command: &mut Command) {
    command.env_remove("OPENAI_ADMIN_KEY");
    command.env_remove("CONTROL_PLANE_API_KEY");
    command.env_remove("OPENAI_API_KEY");
}

pub fn ensure_loopback(origin: SocketAddr) -> Result<()> {
    anyhow::ensure!(
        origin.ip().is_loopback(),
        "OpenAI Secure MCP Tunnel profile requires a loopback-only MCP origin; public bind addresses are not allowed"
    );
    Ok(())
}

pub fn local_mcp_url(origin: SocketAddr) -> String {
    format!("http://{origin}/mcp")
}

fn nonempty_env(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn valid_tunnel_id(value: &str) -> bool {
    let Some(hex) = value.strip_prefix(TUNNEL_ID_PREFIX) else {
        return false;
    };
    hex.len() == TUNNEL_ID_HEX_LEN
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn tunnel_id_grammar_matches_official_shape() {
        assert!(valid_tunnel_id("tunnel_0123456789abcdef0123456789abcdef"));
        assert!(!valid_tunnel_id("tunnel_0123456789ABCDEF0123456789ABCDEF"));
        assert!(!valid_tunnel_id("tunnel_short"));
        assert!(!valid_tunnel_id("other_0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn generated_tunnel_ids_match_reference_grammar() -> noprop::TestResult {
        test_support::run(0x4f50_454e_4149_5455, 512, |ctx| {
            let candidate = match noprop::sample_usize_in(ctx, 0..=3) {
                0 => format!("tunnel_{:032x}", noprop::sample_u64(ctx)),
                1 => format!("tunnel_{}", test_support::safe_component(ctx)),
                2 => format!("other_{:032x}", noprop::sample_u64(ctx)),
                _ => format!(
                    "tunnel_{:016X}{:016X}",
                    noprop::sample_u64(ctx),
                    noprop::sample_u64(ctx)
                ),
            };
            let expected = candidate.strip_prefix("tunnel_").is_some_and(|hex| {
                hex.len() == 32
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
            assert_eq!(valid_tunnel_id(&candidate), expected, "{candidate:?}");
            Ok(())
        })
    }

    #[test]
    fn openai_origin_must_be_loopback_and_formats_mcp_url() {
        let local: SocketAddr = "127.0.0.1:8791".parse().unwrap();
        let ipv6: SocketAddr = "[::1]:8791".parse().unwrap();
        let public: SocketAddr = "0.0.0.0:8791".parse().unwrap();
        assert!(ensure_loopback(local).is_ok());
        assert!(ensure_loopback(ipv6).is_ok());
        assert!(ensure_loopback(public).is_err());
        assert_eq!(local_mcp_url(local), "http://127.0.0.1:8791/mcp");
        assert_eq!(local_mcp_url(ipv6), "http://[::1]:8791/mcp");
    }

    #[test]
    fn control_plane_base_url_policy_accepts_only_https_origins() {
        let valid = url::Url::parse("https://api.openai.com").unwrap();
        assert_eq!(
            valid.origin().ascii_serialization(),
            "https://api.openai.com"
        );

        for invalid in [
            "http://api.openai.com",
            "https://user@example.com",
            "https://example.com/v1",
            "https://example.com/?q=1",
        ] {
            let parsed = url::Url::parse(invalid).unwrap();
            let accepted = parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.path().trim_matches('/').is_empty()
                && parsed.host_str().is_some();
            assert!(!accepted, "{invalid}");
        }
    }

    #[test]
    fn generated_tunnel_config_value_quotes_are_unambiguous() -> noprop::TestResult {
        test_support::run(0x4f50_454e_4346_4751, 512, |ctx| {
            let id = format!(
                "tunnel_{:016x}{:016x}",
                noprop::sample_u64(ctx),
                noprop::sample_u64(ctx)
            );
            let mode = noprop::sample_usize_in(ctx, 0..=5);
            let contents = match mode {
                0 => format!("CONTROL_PLANE_TUNNEL_ID={id}\n"),
                1 => format!("CONTROL_PLANE_TUNNEL_ID=\"{id}\"\n"),
                2 => format!("CONTROL_PLANE_TUNNEL_ID=\"{id}\n"),
                3 => format!("CONTROL_PLANE_TUNNEL_ID={id}\"\n"),
                4 => format!("CONTROL_PLANE_TUNNEL_ID=\"\"{id}\"\"\n"),
                _ => format!("CONTROL_PLANE_TUNNEL_ID={id}\nCONTROL_PLANE_TUNNEL_ID={id}\n"),
            };
            let result = parse_configured_tunnel_id(&contents);
            assert_eq!(
                result.is_ok(),
                mode <= 1,
                "mode={mode} contents={contents:?}"
            );
            if let Ok(parsed) = result {
                assert_eq!(parsed.as_deref(), Some(id.as_str()));
            }
            Ok(())
        })
    }

    #[test]
    fn failed_config_install_removes_atomic_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("openai.env");
        std::fs::create_dir(&path).unwrap();
        let tunnel_id = "tunnel_0123456789abcdef0123456789abcdef";
        assert!(write_configured_tunnel_id(&path, tunnel_id).is_err());
        let leftovers = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with(".openai.env.") && name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "orphan temp files: {leftovers:?}");
    }

    #[test]
    fn config_reader_rejects_oversized_regular_files_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let oversized = root.path().join("oversized.env");
        std::fs::write(&oversized, vec![b'x'; 4097]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(read_configured_tunnel_id(&oversized).is_err());

        #[cfg(unix)]
        {
            let target = root.path().join("target.env");
            std::fs::write(
                &target,
                "CONTROL_PLANE_TUNNEL_ID=tunnel_0123456789abcdef0123456789abcdef\n",
            )
            .unwrap();
            use std::os::unix::fs::{PermissionsExt, symlink};
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = root.path().join("link.env");
            symlink(&target, &link).unwrap();
            assert!(read_configured_tunnel_id(&link).is_err());
        }
    }

    #[test]
    fn tunnel_id_config_is_private_and_contains_no_keys() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("openai.env");
        let tunnel_id = "tunnel_0123456789abcdef0123456789abcdef";
        write_configured_tunnel_id(&path, tunnel_id).unwrap();
        assert_eq!(
            read_configured_tunnel_id(&path).unwrap().as_deref(),
            Some(tunnel_id)
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, format!("CONTROL_PLANE_TUNNEL_ID={tunnel_id}\n"));
        assert!(!contents.contains("API_KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn tunnel_id_config_rejects_secrets_and_public_permissions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("openai.env");
        std::fs::write(
            &path,
            "CONTROL_PLANE_TUNNEL_ID=tunnel_0123456789abcdef0123456789abcdef\nCONTROL_PLANE_API_KEY=secret\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(read_configured_tunnel_id(&path).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &path,
                "CONTROL_PLANE_TUNNEL_ID=tunnel_0123456789abcdef0123456789abcdef\n",
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_configured_tunnel_id(&path).is_err());
        }
    }

    #[test]
    fn generated_scope_id_lists_are_bounded_and_deduplicated() -> noprop::TestResult {
        test_support::run(0x4f50_454e_5343_4f50, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=MAX_SCOPE_IDS_PER_KIND + 8);
            let mut values = Vec::with_capacity(count);
            for index in 0..count {
                let base = format!("scope_{}", index % 8);
                values.push(if noprop::sample_bool(ctx) {
                    format!(" {base} ")
                } else {
                    base
                });
            }
            let result = normalized_scope_ids(values.clone());
            if count > MAX_SCOPE_IDS_PER_KIND {
                assert!(result.is_err());
            } else {
                let normalized = result.unwrap();
                assert!(normalized.len() <= values.len().min(8));
                let mut seen = std::collections::HashSet::new();
                for value in normalized {
                    assert!(seen.insert(value.clone()), "duplicate scope id: {value}");
                    assert!(!value.starts_with(' ') && !value.ends_with(' '));
                }
            }
            Ok(())
        })
    }

    #[test]
    fn tunnel_client_commands_apply_least_privilege_credentials() {
        fn env_value(command: &Command, name: &str) -> Option<Option<String>> {
            command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
        }

        let mut control_plane = Command::new("tunnel-client");
        for name in [
            "OPENAI_ADMIN_KEY",
            "CONTROL_PLANE_API_KEY",
            "OPENAI_API_KEY",
        ] {
            control_plane.env(name, "sentinel");
        }
        restrict_runtime_credentials(&mut control_plane, "CONTROL_PLANE_API_KEY");
        assert_eq!(env_value(&control_plane, "OPENAI_ADMIN_KEY"), Some(None));
        assert_eq!(
            env_value(&control_plane, "CONTROL_PLANE_API_KEY"),
            Some(Some("sentinel".to_owned()))
        );
        assert_eq!(env_value(&control_plane, "OPENAI_API_KEY"), Some(None));

        let mut fallback = Command::new("tunnel-client");
        for name in [
            "OPENAI_ADMIN_KEY",
            "CONTROL_PLANE_API_KEY",
            "OPENAI_API_KEY",
        ] {
            fallback.env(name, "sentinel");
        }
        restrict_runtime_credentials(&mut fallback, "OPENAI_API_KEY");
        assert_eq!(env_value(&fallback, "OPENAI_ADMIN_KEY"), Some(None));
        assert_eq!(env_value(&fallback, "CONTROL_PLANE_API_KEY"), Some(None));
        assert_eq!(
            env_value(&fallback, "OPENAI_API_KEY"),
            Some(Some("sentinel".to_owned()))
        );

        let mut version = Command::new("tunnel-client");
        for name in [
            "OPENAI_ADMIN_KEY",
            "CONTROL_PLANE_API_KEY",
            "OPENAI_API_KEY",
        ] {
            version.env(name, "sentinel");
        }
        scrub_openai_credentials(&mut version);
        for name in [
            "OPENAI_ADMIN_KEY",
            "CONTROL_PLANE_API_KEY",
            "OPENAI_API_KEY",
        ] {
            assert_eq!(env_value(&version, name), Some(None));
        }
    }

    #[test]
    fn scope_ids_are_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            normalized_scope_ids(vec![" ws_123 ".into(), "ws_123".into(), "org-456".into()])
                .unwrap(),
            vec!["ws_123", "org-456"]
        );
        assert!(normalized_scope_ids(vec!["".into()]).is_err());
        assert!(normalized_scope_ids(vec!["bad/value".into()]).is_err());
    }

    #[test]
    fn api_errors_expose_only_bounded_message_field() {
        let body = br#"{"error":{"message":"permission denied","secret":"do-not-print"}}"#;
        assert_eq!(safe_api_error(body), "permission denied");
        assert!(!safe_api_error(body).contains("do-not-print"));
        assert_eq!(
            safe_api_error(b"not json"),
            "request failed (response body omitted)"
        );
    }

    #[test]
    fn generated_env_secret_selection_treats_only_empty_as_missing() -> noprop::TestResult {
        test_support::run(0x4f50_454e_5345_4352, 512, |ctx| {
            let value = test_support::ascii_string(ctx, 128);
            let selected =
                secret_from_env_value(Some(OsString::from(&value)), "TEST_SECRET").unwrap();
            assert_eq!(selected.is_some(), !value.is_empty(), "value={value:?}");
            if let Some(selected) = selected {
                assert_eq!(selected.as_str(), value);
            }
            assert!(
                secret_from_env_value(None, "TEST_SECRET")
                    .unwrap()
                    .is_none()
            );
            Ok(())
        })
    }

    #[test]
    fn prompted_runtime_secret_is_child_only_and_admin_is_removed() {
        use std::ffi::OsStr;

        let credential = RuntimeCredential::Prompted(Zeroizing::new("runtime-secret".to_owned()));
        let mut command = Command::new("tunnel-client");
        configure_runtime_command(&mut command, &credential);

        let envs: Vec<_> = command.as_std().get_envs().collect();
        let lookup = |name: &str| {
            envs.iter()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| *value)
        };
        assert_eq!(
            lookup("CONTROL_PLANE_API_KEY"),
            Some(Some(OsStr::new("runtime-secret")))
        );
        assert_eq!(lookup("OPENAI_API_KEY"), Some(None));
        assert_eq!(lookup("OPENAI_ADMIN_KEY"), Some(None));
    }

    #[test]
    fn inherited_runtime_key_removes_the_other_openai_key_and_admin_key() {
        use std::ffi::OsStr;

        let mut command = Command::new("tunnel-client");
        configure_runtime_command(
            &mut command,
            &RuntimeCredential::Inherited("CONTROL_PLANE_API_KEY"),
        );
        let envs: Vec<_> = command.as_std().get_envs().collect();
        let lookup = |name: &str| {
            envs.iter()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| *value)
        };
        assert_eq!(lookup("OPENAI_API_KEY"), Some(None));
        assert_eq!(lookup("OPENAI_ADMIN_KEY"), Some(None));
        assert_eq!(lookup("CONTROL_PLANE_API_KEY"), None);
    }

    #[tokio::test]
    async fn tunnel_create_posts_admin_auth_and_scope() {
        use axum::{Json, Router, http::HeaderMap, routing::post};
        use serde_json::{Value, json};

        let app = Router::new().route(
            "/v1/tunnels",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(
                    headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok()),
                    Some("Bearer admin-secret")
                );
                assert_eq!(body["name"], "Temote test");
                assert_eq!(body["workspace_ids"], json!(["ws_123"]));
                assert!(body.get("organization_ids").is_none());
                Json(json!({
                    "id": "tunnel_0123456789abcdef0123456789abcdef"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let request = TunnelCreateRequest {
            name: "Temote test".into(),
            description: "test tunnel".into(),
            organization_ids: vec![],
            workspace_ids: vec!["ws_123".into()],
        };
        let tunnel = create_tunnel(&client, &format!("http://{addr}"), "admin-secret", &request)
            .await
            .unwrap();
        assert_eq!(tunnel.id, "tunnel_0123456789abcdef0123456789abcdef");
        server.abort();
    }
}
