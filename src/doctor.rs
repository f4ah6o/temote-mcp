use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(feature = "network")]
use serde::Deserialize;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::child_env;
use crate::host_identity;
use crate::profile::Profile;
use crate::sandbox;

#[cfg(target_os = "linux")]
const BWRAP_INSTALL_HINT: &str =
    "Install bubblewrap (for example: sudo apt install bubblewrap) and make sure it is in PATH.";
const APPARMOR_PROFILE_HINT: &str = "Ubuntu may be blocking unprivileged user namespaces. Try:\
sudo apt update\
sudo apt install apparmor-profiles apparmor-utils\
sudo install -m 0644 /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/bwrap-userns-restrict\
sudo apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict";
const MAX_TUNNEL_TOKEN_BYTES: u64 = 16 * 1024;
const DOCTOR_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DOCTOR_STREAM_BYTES: usize = 32 * 1024;
#[cfg(feature = "network")]
const MAX_CLOUDFLARE_API_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct Options {
    pub profile: Option<Profile>,
    pub cloudflare: bool,
    pub tunnel_token_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Pass,
    #[cfg(any(target_os = "linux", feature = "network"))]
    Warn,
    Fail,
}

struct Check {
    level: Level,
    name: String,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Pass,
            name: name.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    #[cfg(any(target_os = "linux", feature = "network"))]
    fn warn(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            name: name.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Fail,
            name: name.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn is_failure(&self) -> bool {
        matches!(self.level, Level::Fail)
    }

    fn print(&self) {
        let label = match self.level {
            Level::Pass => "PASS",
            #[cfg(any(target_os = "linux", feature = "network"))]
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        println!("[{label}] {}: {}", self.name, self.detail);
        if let Some(hint) = &self.hint {
            for line in hint.lines() {
                println!("       {line}");
            }
        }
    }
}

struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn add(&mut self, check: Check) {
        self.checks.push(check);
    }

    fn finish(self) -> Result<()> {
        for check in &self.checks {
            check.print();
        }

        let failures = self
            .checks
            .iter()
            .filter(|check| check.is_failure())
            .count();
        #[cfg(any(target_os = "linux", feature = "network"))]
        let warnings = self
            .checks
            .iter()
            .filter(|check| matches!(check.level, Level::Warn))
            .count();
        #[cfg(not(any(target_os = "linux", feature = "network")))]
        let warnings = 0;
        println!();
        println!(
            "doctor summary: {} failure(s), {} warning(s)",
            failures, warnings
        );
        if failures == 0 {
            Ok(())
        } else {
            anyhow::bail!("temote-mcp doctor found {failures} failing check(s)")
        }
    }
}

pub async fn run(options: Options) -> Result<()> {
    println!("temote-mcp doctor");
    println!("platform: {}", std::env::consts::OS);

    let mut report = Report::new();
    let host_id = host_identity::resolve()?;
    report.add(Check::pass("host identity", format!("host_id={host_id}")));
    check_platform(&mut report);
    #[cfg(target_os = "macos")]
    report.add(Check::pass("sandbox backend", "native macOS Seatbelt"));
    #[cfg(target_os = "linux")]
    report.add(Check::pass(
        "sandbox backend",
        "Temote Linux bubblewrap sandbox",
    ));

    #[cfg(target_os = "linux")]
    {
        let helper = check_linux_helper(&mut report)?;
        let network_namespace_ok = check_bwrap(&mut report).await;
        check_user_namespace_settings(&mut report, network_namespace_ok);
        if helper {
            check_sandbox_execution(&mut report).await;
            check_sandbox_runtime_environment(&mut report).await;
        }
    }

    #[cfg(target_os = "macos")]
    {
        check_sandbox_execution(&mut report).await;
        check_sandbox_runtime_environment(&mut report).await;
    }

    anyhow::ensure!(
        !(options
            .profile
            .is_some_and(|profile| profile != Profile::Cloudflare)
            && options.cloudflare),
        "--cloudflare can only be combined with --profile cloudflare"
    );

    match options.profile {
        Some(Profile::Cloudflare) => {
            let public_endpoint = std::env::var("TEMOTE_MCP_PUBLIC_URL")
                .unwrap_or_else(|_| "<not configured>".to_owned());
            let tunnel_id = std::env::var("TEMOTE_MCP_CLOUDFLARE_TUNNEL_ID")
                .or_else(|_| std::env::var("CLOUDFLARE_TUNNEL_ID"))
                .unwrap_or_else(|_| "<not configured>".to_owned());
            report.add(Check::pass(
                "direct ingress identity",
                format!(
                    "host_id={host_id}, public_endpoint={public_endpoint}, tunnel_id={tunnel_id}"
                ),
            ));
            check_cloudflare_local(&mut report, options.tunnel_token_file.as_deref(), true).await;
            #[cfg(feature = "network")]
            check_cloudflare_access_config(&mut report);
        }
        Some(Profile::Tailscale) => {
            #[cfg(feature = "network")]
            check_tailscale_local(&mut report).await;
            #[cfg(not(feature = "network"))]
            report.add(Check::fail(
                "tailscale profile",
                "this binary was built without the network feature",
                "Use the default temote-mcp build for remote connection profiles.",
            ));
        }
        Some(Profile::Openai) => {
            #[cfg(feature = "network")]
            check_openai_tunnel(&mut report).await;
            #[cfg(not(feature = "network"))]
            report.add(Check::fail(
                "openai profile",
                "this binary was built without the network feature",
                "Use the default temote-mcp build for remote connection profiles.",
            ));
        }
        None => {
            check_cloudflare_local(
                &mut report,
                options.tunnel_token_file.as_deref(),
                options.cloudflare,
            )
            .await;
        }
    }

    #[cfg(feature = "network")]
    if options.cloudflare {
        check_cloudflare_api(&mut report).await;
    }

    #[cfg(not(feature = "network"))]
    if options.cloudflare {
        report.add(Check::fail(
            "cloudflare API",
            "this binary was built without the network feature",
            "Use the default temote-mcp build to enable the authenticated Cloudflare check.",
        ));
    }

    report.finish()
}

fn check_platform(report: &mut Report) {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        report.add(Check::pass("platform", "supported local sandbox platform"));
    } else {
        report.add(Check::fail(
            "platform",
            "unsupported operating system",
            "temote-mcp sandbox execution currently supports Linux and macOS only.",
        ));
    }
}

#[cfg(feature = "network")]
fn check_cloudflare_access_config(report: &mut Report) {
    match crate::access::AccessConfig::from_env() {
        Ok(_) => report.add(Check::pass(
            "cloudflare access",
            "team domain, audience, and email allowlist are configured",
        )),
        Err(error) => report.add(Check::fail(
            "cloudflare access",
            format!("configuration is incomplete: {error}"),
            "Set TEMOTE_MCP_ACCESS_TEAM_DOMAIN, TEMOTE_MCP_ACCESS_AUDIENCE, and TEMOTE_MCP_ACCESS_ALLOWED_EMAILS for the cloudflare profile.",
        )),
    }
}

#[cfg(feature = "network")]
async fn check_tailscale_local(report: &mut Report) {
    match run_doctor_command("tailscale", &["version"]).await {
        Ok(output) if output.status.success() => report.add(Check::pass(
            "tailscale",
            display_output(&output).unwrap_or_else(|| "available".to_owned()),
        )),
        Ok(output) => {
            report.add(Check::fail(
                "tailscale",
                format!("tailscale version failed with {}", output.status),
                "Install and connect Tailscale before using --profile tailscale.",
            ));
            return;
        }
        Err(error) => {
            report.add(Check::fail(
                "tailscale",
                format!("cannot execute tailscale: {error}"),
                "Install Tailscale CLI and make sure it is available on PATH.",
            ));
            return;
        }
    }

    match crate::ingress::tailscale_dns_name().await {
        Ok(hostname) => {
            report.add(Check::pass(
                "tailscale node",
                format!("connected with canonical HTTPS origin https://{hostname}"),
            ));
        }
        Err(error) => report.add(Check::fail(
            "tailscale node",
            error.to_string(),
            "Run `tailscale up`, enable MagicDNS/HTTPS, and ensure Self.DNSName is available.",
        )),
    }

    match crate::ingress::configured_funnel_https_ports().await {
        Ok(configured) => match crate::ingress::TAILSCALE_HTTPS_PORTS
            .into_iter()
            .find(|port| !configured.contains(port))
        {
            Some(port) => report.add(Check::pass(
                "tailscale funnel",
                format!(
                    "Funnel is available; Temote will use HTTPS port {port} without replacing existing ports {:?}",
                    configured
                ),
            )),
            None => report.add(Check::fail(
                "tailscale funnel",
                "all supported HTTPS ports (443, 8443, 10000) already have Funnel configuration owned outside this temote-mcp process",
                "Free one supported Funnel port before `temote-mcp up --profile tailscale`; Temote will not replace existing Funnel configuration.",
            )),
        },
        Err(error) => report.add(Check::fail(
            "tailscale funnel",
            error.to_string(),
            "Enable Funnel for this node/tailnet and verify the funnel node attribute and HTTPS prerequisites.",
        )),
    }

    report.add(Check::pass(
        "local OAuth state",
        "authorization codes, registrations, and access tokens use bounded process-local memory; no Cloudflare credentials or persistent OAuth key file is required",
    ));
}

#[cfg(feature = "network")]
async fn check_openai_tunnel(report: &mut Report) {
    match crate::openai_tunnel::binary_version().await {
        Ok(version) => report.add(Check::pass("OpenAI tunnel-client", version)),
        Err(error) => {
            report.add(Check::fail(
                "OpenAI tunnel-client",
                error.to_string(),
                "Install the supported OpenAI tunnel-client from Platform Tunnels management or the openai/tunnel-client release, then put it on PATH (or set TUNNEL_CLIENT_BIN).",
            ));
            return;
        }
    }

    match crate::openai_tunnel::config_from_env() {
        Ok(config) => report.add(Check::pass(
            "OpenAI tunnel configuration",
            format!(
                "CONTROL_PLANE_TUNNEL_ID is valid and runtime credential is provided via {}",
                config.runtime_key_env
            ),
        )),
        Err(error) => {
            report.add(Check::fail(
                "OpenAI tunnel configuration",
                error.to_string(),
                "Set CONTROL_PLANE_TUNNEL_ID and a Restricted CONTROL_PLANE_API_KEY with Tunnels Read + Use. Do not use an admin key for the long-lived runtime.",
            ));
            return;
        }
    }

    match crate::openai_tunnel::doctor_control_plane().await {
        Ok(detail) => report.add(Check::pass("OpenAI tunnel control plane", detail)),
        Err(error) => report.add(Check::fail(
            "OpenAI tunnel control plane",
            error.to_string(),
            "Verify tunnel/workspace association and that the runtime-key principal has Tunnels Read + Use for the configured tunnel.",
        )),
    }

    report.add(Check::pass(
        "OpenAI local bind policy",
        "temote-mcp up --profile openai enforces a loopback-only HTTP origin and does not require TEMOTE_MCP_PUBLIC_URL, Cloudflare, or Tailscale",
    ));
}

async fn check_cloudflare_local(report: &mut Report, override_path: Option<&Path>, force: bool) {
    let (path, explicit) = resolve_tunnel_token_file(override_path);
    let should_check = force || explicit || path.as_deref().is_some_and(Path::is_file);
    if !should_check {
        report.add(Check::pass(
            "cloudflare tunnel",
            "not configured; local Tunnel checks skipped",
        ));
        return;
    }

    check_cloudflared(report).await;
    match path {
        Some(path) => check_tunnel_token_file(report, &path),
        None => report.add(Check::fail(
            "cloudflare tunnel token",
            "cannot determine the default token file because HOME is unavailable",
            "Set TUNNEL_TOKEN_FILE to an absolute readable token file path.",
        )),
    }
}

fn resolve_tunnel_token_file(override_path: Option<&Path>) -> (Option<PathBuf>, bool) {
    if let Some(path) = override_path {
        return (Some(path.to_owned()), true);
    }
    if let Some(path) = std::env::var_os("TUNNEL_TOKEN_FILE") {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return (Some(path), true);
        }
    }
    (
        crate::platform_paths::home_dir().map(|home| home.join(".config/temote-mcp/tunnel-token")),
        false,
    )
}

async fn check_cloudflared(report: &mut Report) {
    match run_doctor_command("cloudflared", &["--version"]).await {
        Ok(output) if output.status.success() => report.add(Check::pass(
            "cloudflared",
            display_output(&output).unwrap_or_else(|| "available".to_owned()),
        )),
        Ok(output) => report.add(Check::fail(
            "cloudflared",
            format!("--version exited with {}", output.status),
            "Install cloudflared and make sure it is available on PATH.",
        )),
        Err(error) => report.add(Check::fail(
            "cloudflared",
            format!("cannot execute cloudflared: {error}"),
            "Install cloudflared and make sure it is available on PATH.",
        )),
    }
}

fn check_tunnel_token_file(report: &mut Report, path: &Path) {
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
        Err(error) => {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!("cannot open {}: {error}", path.display()),
                format!(
                    "Create a readable regular Tunnel token file at {} and do not use a symlink.",
                    path.display()
                ),
            ));
            return;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!("cannot inspect {}: {error}", path.display()),
                "Replace the file with a readable regular Tunnel token file.",
            ));
            return;
        }
    };
    if !metadata.is_file() {
        report.add(Check::fail(
            "cloudflare tunnel token",
            format!("{} is not a regular file", path.display()),
            format!(
                "Set TUNNEL_TOKEN_FILE to a regular token file, not a directory: {}.",
                path.display()
            ),
        ));
        return;
    }

    if metadata.len() > MAX_TUNNEL_TOKEN_BYTES {
        report.add(Check::fail(
            "cloudflare tunnel token",
            format!(
                "{} exceeds the {MAX_TUNNEL_TOKEN_BYTES}-byte token-file limit",
                path.display()
            ),
            "Replace the file with the remotely managed Tunnel bearer token only.",
        ));
        return;
    }

    let mut bytes =
        Vec::with_capacity((metadata.len() as usize).min(MAX_TUNNEL_TOKEN_BYTES as usize));
    match file
        .by_ref()
        .take(MAX_TUNNEL_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        Ok(_) if bytes.len() > MAX_TUNNEL_TOKEN_BYTES as usize => {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!(
                    "{} exceeds the {MAX_TUNNEL_TOKEN_BYTES}-byte token-file limit",
                    path.display()
                ),
                "Replace the file with the remotely managed Tunnel bearer token only.",
            ));
            return;
        }
        Ok(_) => {}
        Err(error) => {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!("cannot read {}: {error}", path.display()),
                "Make the Tunnel token file readable by the user running temote-mcp.",
            ));
            return;
        }
    }
    let value = match std::str::from_utf8(&bytes) {
        Ok(value) => value,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!("{} is not valid UTF-8: {error}", path.display()),
                "Replace the file with the remotely managed Tunnel bearer token only.",
            ));
            return;
        }
    };
    if value.trim().is_empty() {
        report.add(Check::fail(
            "cloudflare tunnel token",
            format!("{} is empty", path.display()),
            format!(
                "Store the remotely managed Tunnel token in {}.",
                path.display()
            ),
        ));
        return;
    }

    #[cfg(unix)]
    {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            report.add(Check::fail(
                "cloudflare tunnel token",
                format!(
                    "{} is readable by group or other users (mode {mode:04o})",
                    path.display()
                ),
                format!(
                    "Protect the bearer token with: chmod 600 {}.",
                    path.display()
                ),
            ));
            return;
        }
        report.add(Check::pass(
            "cloudflare tunnel token",
            format!(
                "{} is readable and protected (mode {mode:04o})",
                path.display()
            ),
        ));
    }

    #[cfg(not(unix))]
    report.add(Check::pass(
        "cloudflare tunnel token",
        format!("{} is readable", path.display()),
    ));
}

#[cfg(feature = "network")]
#[derive(Deserialize)]
struct CloudflareApiResponse<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<CloudflareApiError>,
    result: Option<T>,
}

#[cfg(feature = "network")]
#[derive(Deserialize)]
struct CloudflareApiError {
    message: Option<String>,
}

#[cfg(feature = "network")]
#[derive(Deserialize)]
struct CloudflareTunnel {
    id: Option<String>,
    name: Option<String>,
    status: Option<String>,
}

#[cfg(feature = "network")]
async fn check_cloudflare_api(report: &mut Report) {
    let account_id =
        first_nonempty_env(&["TEMOTE_MCP_CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ACCOUNT_ID"]);
    let tunnel_id =
        first_nonempty_env(&["TEMOTE_MCP_CLOUDFLARE_TUNNEL_ID", "CLOUDFLARE_TUNNEL_ID"]);
    let api_token =
        first_nonempty_env(&["TEMOTE_MCP_CLOUDFLARE_API_TOKEN", "CLOUDFLARE_API_TOKEN"]);

    let mut missing = Vec::new();
    if account_id.is_none() {
        missing.push("TEMOTE_MCP_CLOUDFLARE_ACCOUNT_ID or CLOUDFLARE_ACCOUNT_ID");
    }
    if tunnel_id.is_none() {
        missing.push("TEMOTE_MCP_CLOUDFLARE_TUNNEL_ID or CLOUDFLARE_TUNNEL_ID");
    }
    if api_token.is_none() {
        missing.push("TEMOTE_MCP_CLOUDFLARE_API_TOKEN or CLOUDFLARE_API_TOKEN");
    }
    if !missing.is_empty() {
        report.add(Check::fail(
            "cloudflare API",
            format!("missing {}", missing.join(", ")),
            "Provide the account ID, Tunnel ID, and API token before using doctor --cloudflare. Values are never printed.",
        ));
        return;
    }

    let account_id = account_id.expect("account ID checked above");
    let tunnel_id = tunnel_id.expect("Tunnel ID checked above");
    let api_token = api_token.expect("API token checked above");

    if !is_cloudflare_account_id(&account_id) {
        report.add(Check::fail(
            "cloudflare API",
            "Cloudflare account ID must be a 32-character hexadecimal value",
            "Set TEMOTE_MCP_CLOUDFLARE_ACCOUNT_ID to the account ID from Cloudflare.",
        ));
        return;
    }
    if uuid::Uuid::parse_str(&tunnel_id).is_err() {
        report.add(Check::fail(
            "cloudflare API",
            "Cloudflare Tunnel ID must be a UUID",
            "Set TEMOTE_MCP_CLOUDFLARE_TUNNEL_ID to the Tunnel UUID from Cloudflare.",
        ));
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare API",
                format!("cannot create the HTTPS client: {error}"),
                "Check the temote-mcp network build and TLS runtime.",
            ));
            return;
        }
    };

    let endpoint = format!(
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/cfd_tunnel/{tunnel_id}"
    );
    let response = match client.get(endpoint).bearer_auth(api_token).send().await {
        Ok(response) => response,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare API",
                format!("Tunnel status request failed: {error}"),
                "Check outbound HTTPS access and the Cloudflare API token permissions.",
            ));
            return;
        }
    };

    let http_status = response.status();
    let bytes = match read_bounded_cloudflare_response(response).await {
        Ok(bytes) => bytes,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare API",
                format!("Cloudflare returned an unreadable response ({http_status}): {error}"),
                "Check the Cloudflare API endpoint and token permissions.",
            ));
            return;
        }
    };
    let body: CloudflareApiResponse<CloudflareTunnel> = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => {
            report.add(Check::fail(
                "cloudflare API",
                format!("Cloudflare returned invalid JSON ({http_status}): {error}"),
                "Check the Cloudflare API endpoint and token permissions.",
            ));
            return;
        }
    };

    if !http_status.is_success() || !body.success {
        report.add(Check::fail(
            "cloudflare API",
            format!(
                "Tunnel status request returned {http_status}: {}",
                cloudflare_error_detail(&body.errors)
            ),
            "Check the account ID, Tunnel ID, and API token permissions.",
        ));
        return;
    }

    let Some(tunnel) = body.result else {
        report.add(Check::fail(
            "cloudflare API",
            "Cloudflare returned no Tunnel result",
            "Check the account ID and Tunnel ID.",
        ));
        return;
    };

    let status = tunnel.status.as_deref().unwrap_or("unknown");
    let name = tunnel.name.as_deref().unwrap_or("unnamed");
    let detail = format!("{name} ({tunnel_id}): status={status}");
    match cloudflare_status_level(Some(status)) {
        Level::Pass => report.add(Check::pass("cloudflare tunnel", detail)),
        Level::Warn => report.add(Check::warn(
            "cloudflare tunnel",
            detail,
            "Inspect the Tunnel connector and Cloudflare dashboard before relying on the public endpoint.",
        )),
        Level::Fail => report.add(Check::fail(
            "cloudflare tunnel",
            detail,
            "Start cloudflared for this Tunnel and verify its connector and public hostname configuration.",
        )),
    }

    if let Some(id) = tunnel.id
        && id != tunnel_id
    {
        report.add(Check::fail(
            "cloudflare API",
            "Cloudflare returned a different Tunnel ID than requested",
            "Check the configured Tunnel ID and account ID.",
        ));
    }
}

#[cfg(feature = "network")]
fn first_nonempty_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_owned())
    })
}

#[cfg(feature = "network")]
fn is_cloudflare_account_id(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|character| character.is_ascii_hexdigit())
}

#[cfg(feature = "network")]
fn cloudflare_status_level(status: Option<&str>) -> Level {
    match status {
        Some(value) if value.eq_ignore_ascii_case("healthy") => Level::Pass,
        Some(value) if value.eq_ignore_ascii_case("degraded") => Level::Warn,
        Some(value)
            if value.eq_ignore_ascii_case("inactive") || value.eq_ignore_ascii_case("down") =>
        {
            Level::Fail
        }
        _ => Level::Fail,
    }
}

#[cfg(feature = "network")]
async fn read_bounded_cloudflare_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_CLOUDFLARE_API_RESPONSE_BYTES as u64,
            "Cloudflare API response exceeds {MAX_CLOUDFLARE_API_RESPONSE_BYTES} bytes"
        );
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_CLOUDFLARE_API_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read Cloudflare API response")?
    {
        append_bounded_cloudflare_chunk(&mut bytes, &chunk)?;
    }
    Ok(bytes)
}

#[cfg(feature = "network")]
fn append_bounded_cloudflare_chunk(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    let next = bytes
        .len()
        .checked_add(chunk.len())
        .context("Cloudflare API response size overflow")?;
    anyhow::ensure!(
        next <= MAX_CLOUDFLARE_API_RESPONSE_BYTES,
        "Cloudflare API response exceeds {MAX_CLOUDFLARE_API_RESPONSE_BYTES} bytes"
    );
    bytes.extend_from_slice(chunk);
    Ok(())
}

#[cfg(feature = "network")]
fn cloudflare_error_detail(errors: &[CloudflareApiError]) -> String {
    let messages = errors
        .iter()
        .filter_map(|error| error.message.as_deref())
        .filter(|message| !message.trim().is_empty())
        .take(2)
        .collect::<Vec<_>>();
    if messages.is_empty() {
        "Cloudflare reported an unsuccessful API response".to_owned()
    } else {
        messages.join("; ")
    }
}

#[cfg(target_os = "linux")]
fn check_linux_helper(report: &mut Report) -> Result<bool> {
    let executable =
        std::env::current_exe().context("could not determine temote-mcp executable")?;
    let directory = executable
        .parent()
        .context("temote-mcp executable has no parent directory")?;
    let candidates = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .map(|profile| {
                vec![
                    directory.join("temote-linux-sandbox"),
                    profile.join("temote-linux-sandbox"),
                ]
            })
            .unwrap_or_else(|| vec![directory.join("temote-linux-sandbox")])
    } else {
        vec![directory.join("temote-linux-sandbox")]
    };
    if let Some(helper) = candidates.into_iter().find(|candidate| candidate.is_file()) {
        report.add(Check::pass(
            "sandbox helper",
            format!("{}", helper.display()),
        ));
        Ok(true)
    } else {
        let helper = directory.join("temote-linux-sandbox");
        report.add(Check::fail(
            "sandbox helper",
            format!("missing {}", helper.display()),
            "Install temote-mcp with cargo install --path . --locked so the helper is installed beside it.",
        ));
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
async fn check_bwrap(report: &mut Report) -> bool {
    let version = run_doctor_command("bwrap", &["--version"]).await;
    match version {
        Ok(output) if output.status.success() => {
            report.add(Check::pass(
                "bubblewrap",
                display_output(&output).unwrap_or_else(|| "available".to_owned()),
            ));
        }
        Ok(output) => {
            report.add(Check::fail(
                "bubblewrap",
                format!("--version exited with {}", output.status),
                BWRAP_INSTALL_HINT,
            ));
            return false;
        }
        Err(error) => {
            report.add(Check::fail(
                "bubblewrap",
                format!("cannot execute bwrap: {error}"),
                BWRAP_INSTALL_HINT,
            ));
            return false;
        }
    }

    match run_doctor_command(
        "bwrap",
        &[
            "--unshare-user",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/bin/true",
        ],
    )
    .await
    {
        Ok(output) if output.status.success() => {
            report.add(Check::pass(
                "network namespace",
                "bwrap can create the isolated loopback namespace",
            ));
            true
        }
        Ok(output) => {
            let detail =
                display_output(&output).unwrap_or_else(|| format!("exited with {}", output.status));
            let hint = if contains_loopback_permission_error(&output) {
                APPARMOR_PROFILE_HINT
            } else {
                "Run the bwrap namespace probe manually and check the host's user-namespace and network-namespace policy."
            };
            report.add(Check::fail("network namespace", detail, hint));
            false
        }
        Err(error) => {
            report.add(Check::fail(
                "network namespace",
                format!("cannot execute bwrap: {error}"),
                BWRAP_INSTALL_HINT,
            ));
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn check_user_namespace_settings(report: &mut Report, network_namespace_ok: bool) {
    check_positive_sysctl(
        report,
        "/proc/sys/user/max_user_namespaces",
        "user namespaces",
        "Enable unprivileged user namespaces or run temote-mcp on a host that permits them.",
    );
    check_positive_sysctl(
        report,
        "/proc/sys/kernel/unprivileged_userns_clone",
        "unprivileged user namespaces",
        "Set kernel.unprivileged_userns_clone to 1 or use the distribution's supported user-namespace configuration.",
    );

    let path = Path::new("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    match std::fs::read_to_string(path) {
        Ok(value) if value.trim() == "1" && network_namespace_ok => report.add(Check::pass(
            "AppArmor userns policy",
            "restriction value is 1; bwrap compatibility check passed",
        )),
        Ok(value) if value.trim() == "1" => report.add(Check::warn(
            "AppArmor userns policy",
            "unprivileged user namespaces are restricted (1)",
            APPARMOR_PROFILE_HINT,
        )),
        Ok(value) => report.add(Check::pass(
            "AppArmor userns policy",
            format!("restriction value is {}", value.trim()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.add(Check::pass(
            "AppArmor userns policy",
            "kernel setting is not present",
        )),
        Err(error) => report.add(Check::warn(
            "AppArmor userns policy",
            format!("could not read {path:?}: {error}"),
            "Check the host's AppArmor and user-namespace policy if bwrap fails.",
        )),
    }
}

#[cfg(target_os = "linux")]
fn check_positive_sysctl(report: &mut Report, path: &str, name: &str, hint: &str) {
    match std::fs::read_to_string(path) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(value) if value > 0 => report.add(Check::pass(name, format!("{path}={value}"))),
            Ok(value) => report.add(Check::fail(name, format!("{path}={value}"), hint)),
            Err(error) => report.add(Check::warn(
                name,
                format!("{path} is not numeric: {error}"),
                hint,
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.add(Check::pass(name, format!("{path} is not present")))
        }
        Err(error) => report.add(Check::warn(
            name,
            format!("could not read {path}: {error}"),
            hint,
        )),
    }
}

async fn check_sandbox_execution(report: &mut Report) {
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => {
            report.add(Check::fail(
                "sandbox execution",
                format!("cannot resolve current directory: {error}"),
                "Run doctor from an existing directory that the current user can read.",
            ));
            return;
        }
    };

    let roots = vec![cwd.clone()];
    #[cfg(target_os = "macos")]
    let true_executable = "/usr/bin/true";
    #[cfg(not(target_os = "macos"))]
    let true_executable = "/bin/true";
    match sandbox::run(&[true_executable.to_owned()], &cwd, &roots, None).await {
        Ok(output) if output.status == 0 => report.add(Check::pass(
            "sandbox execution",
            "a temote-mcp sandboxed command completed successfully",
        )),
        Ok(output) => {
            let detail = if output.stderr.trim().is_empty() {
                format!("sandboxed command exited with status {}", output.status)
            } else {
                output.stderr.trim().to_owned()
            };
            let hint = if contains_loopback_permission_error_text(&detail) {
                APPARMOR_PROFILE_HINT
            } else {
                "Fix the lower-level sandbox check above, then restart temote-mcp."
            };
            report.add(Check::fail("sandbox execution", detail, hint));
        }
        Err(error) => {
            let detail = format!("{error:#}");
            let hint = if contains_loopback_permission_error_text(&detail) {
                APPARMOR_PROFILE_HINT
            } else {
                "Fix the lower-level sandbox check above, then restart temote-mcp."
            };
            report.add(Check::fail("sandbox execution", detail, hint));
        }
    }
}

async fn check_sandbox_runtime_environment(report: &mut Report) {
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => {
            report.add(Check::fail(
                "sandbox runtime environment",
                format!("cannot resolve current directory: {error}"),
                "Run doctor from an existing directory that the current user can read.",
            ));
            return;
        }
    };

    let command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "test -n \"$HOME\" || { echo 'HOME is unset' >&2; exit 10; }; test -d \"$HOME\" || { echo 'HOME is not a directory' >&2; exit 11; }; temp=$(mktemp /tmp/temote-mcp-doctor.XXXXXX) || { echo '/tmp is not writable' >&2; exit 12; }; rm -f \"$temp\" || { echo 'cannot remove temporary file from /tmp' >&2; exit 13; }".to_owned(),
    ];
    match sandbox::run(&command, &cwd, std::slice::from_ref(&cwd), None).await {
        Ok(output) if output.status == 0 => report.add(Check::pass(
            "sandbox runtime environment",
            "HOME and /tmp are available to sandboxed commands",
        )),
        Ok(output) => {
            let detail = command_output_detail(&output);
            report.add(Check::fail(
                "sandbox runtime environment",
                detail,
                "Run just install to update temote-mcp, then restart it; shell commands need HOME and a writable temporary directory.",
            ));
        }
        Err(error) => report.add(Check::fail(
            "sandbox runtime environment",
            format!("{error:#}"),
            "Run just install to update temote-mcp, then restart it; shell commands need HOME and a writable temporary directory.",
        )),
    }
}

fn command_output_detail(output: &sandbox::Output) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        stderr.to_owned()
    } else if output.stdout.trim().is_empty() {
        format!("sandboxed command exited with status {}", output.status)
    } else {
        output.stdout.trim().to_owned()
    }
}

struct DoctorOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

async fn run_doctor_command(program: &str, args: &[&str]) -> Result<DoctorOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    child_env::scrub_sensitive(&mut command, &[]);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot execute {program}"))?;
    let stdout = child
        .stdout
        .take()
        .context("doctor command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("doctor command stderr was not captured")?;
    let captured = tokio::time::timeout(DOCTOR_COMMAND_TIMEOUT, async {
        let (stdout, stderr, status) = tokio::join!(
            read_doctor_stream(stdout, MAX_DOCTOR_STREAM_BYTES),
            read_doctor_stream(stderr, MAX_DOCTOR_STREAM_BYTES),
            child.wait(),
        );
        Result::<_>::Ok((stdout?, stderr?, status?))
    })
    .await;
    let (stdout, stderr, status) = match captured {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!(
                "{program} timed out after {}s",
                DOCTOR_COMMAND_TIMEOUT.as_secs()
            );
        }
    };
    Ok(DoctorOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        truncated: stdout.truncated || stderr.truncated,
    })
}

struct DoctorStream {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_doctor_stream<R>(mut reader: R, limit: usize) -> std::io::Result<DoctorStream>
where
    R: AsyncRead + Unpin,
{
    const CHUNK: usize = 8192;
    let mut bytes = Vec::with_capacity(limit.min(CHUNK));
    let mut truncated = false;
    let mut buffer = [0_u8; CHUNK];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let kept = remaining.min(read);
        bytes.extend_from_slice(&buffer[..kept]);
        if kept < read {
            truncated = true;
        }
    }
    Ok(DoctorStream { bytes, truncated })
}

fn display_output(output: &DoctorOutput) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let value = if !stderr.is_empty() {
        Some(stderr)
    } else if !stdout.is_empty() {
        Some(stdout)
    } else {
        None
    };
    value.map(|value| {
        if output.truncated {
            format!("{value} [output truncated]")
        } else {
            value
        }
    })
}

#[cfg(target_os = "linux")]
fn contains_loopback_permission_error(output: &DoctorOutput) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    contains_loopback_permission_error_text(&text)
}

fn contains_loopback_permission_error_text(text: &str) -> bool {
    text.contains("RTM_NEWADDR")
        || (text.contains("loopback") && text.contains("Operation not permitted"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_doctor_stream_capture_matches_prefix_model() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x444f_4354_4341_5001, 512, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 0..=128);
            let len = noprop::sample_usize_in(ctx, 0..=256);
            let input = (0..len).map(|_| noprop::sample_u8(ctx)).collect::<Vec<_>>();
            runtime.block_on(async {
                use tokio::io::AsyncWriteExt as _;
                let (mut writer, reader) = tokio::io::duplex(input.len().max(1));
                writer.write_all(&input).await.unwrap();
                writer.shutdown().await.unwrap();
                let captured = read_doctor_stream(reader, limit).await.unwrap();
                assert_eq!(captured.bytes, input[..input.len().min(limit)]);
                assert_eq!(captured.truncated, input.len() > limit);
            });
            Ok(())
        })
    }

    #[test]
    fn recognizes_the_bwrap_loopback_failure() {
        assert!(contains_loopback_permission_error_text(
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n"
        ));
    }

    #[test]
    fn ignores_unrelated_bwrap_failures() {
        assert!(!contains_loopback_permission_error_text(
            "bwrap: Can't find source path /missing\n"
        ));
    }

    #[cfg(feature = "network")]
    #[test]
    fn generated_cloudflare_account_ids_match_hex_grammar() -> noprop::TestResult {
        test_support::run(0x4346_4143_4354_0001, test_support::DEFAULT_CASES, |ctx| {
            let value = test_support::ascii_string(ctx, 40);
            let expected =
                value.len() == 32 && value.chars().all(|character| character.is_ascii_hexdigit());
            assert_eq!(
                is_cloudflare_account_id(&value),
                expected,
                "account id={value:?}"
            );
            Ok(())
        })
    }

    #[cfg(feature = "network")]
    #[test]
    fn generated_cloudflare_response_budget_never_overreads() -> noprop::TestResult {
        test_support::run(0x4346_4150_4942_4f44, 512, |ctx| {
            let start = if noprop::sample_bool(ctx) {
                noprop::sample_usize_in(ctx, 0..=2048)
            } else {
                MAX_CLOUDFLARE_API_RESPONSE_BYTES - noprop::sample_usize_in(ctx, 0..=2048)
            };
            let chunk_len = noprop::sample_usize_in(ctx, 0..=4096);
            let mut bytes = vec![0_u8; start];
            let chunk = vec![noprop::sample_u8(ctx); chunk_len];
            let expected = start
                .checked_add(chunk_len)
                .is_some_and(|next| next <= MAX_CLOUDFLARE_API_RESPONSE_BYTES);
            let result = append_bounded_cloudflare_chunk(&mut bytes, &chunk);
            assert_eq!(result.is_ok(), expected);
            assert_eq!(
                bytes.len(),
                if expected { start + chunk_len } else { start }
            );
            assert!(bytes.len() <= MAX_CLOUDFLARE_API_RESPONSE_BYTES);
            Ok(())
        })
    }

    #[cfg(feature = "network")]
    #[test]
    fn generated_cloudflare_statuses_match_reference_model() -> noprop::TestResult {
        test_support::run(0x4346_5354_4154_0001, 512, |ctx| {
            let known = ["healthy", "degraded", "inactive", "down"];
            let status = if noprop::sample_bool(ctx) {
                let value = known[noprop::sample_usize_in(ctx, 0..known.len())];
                if noprop::sample_bool(ctx) {
                    value.to_ascii_uppercase()
                } else {
                    value.to_owned()
                }
            } else {
                test_support::safe_component(ctx)
            };
            let expected = if status.eq_ignore_ascii_case("healthy") {
                Level::Pass
            } else if status.eq_ignore_ascii_case("degraded") {
                Level::Warn
            } else {
                Level::Fail
            };
            assert_eq!(
                cloudflare_status_level(Some(&status)),
                expected,
                "status={status:?}"
            );
            Ok(())
        })
    }

    #[cfg(feature = "network")]
    #[test]
    fn classifies_cloudflare_tunnel_statuses() {
        assert_eq!(cloudflare_status_level(Some("healthy")), Level::Pass);
        assert_eq!(cloudflare_status_level(Some("degraded")), Level::Warn);
        assert_eq!(cloudflare_status_level(Some("down")), Level::Fail);
        assert_eq!(cloudflare_status_level(Some("inactive")), Level::Fail);
        assert_eq!(cloudflare_status_level(Some("future-status")), Level::Fail);
        assert_eq!(cloudflare_status_level(None), Level::Fail);
    }

    #[cfg(unix)]
    #[test]
    fn generated_tunnel_token_permissions_match_private_file_policy() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("token");
        std::fs::write(&path, "opaque-token").unwrap();

        test_support::run(0x4346_544f_4b45_4e01, 512, |ctx| {
            let exposure = noprop::sample_u8(ctx) & 0o77;
            let mode = 0o600 | u32::from(exposure);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let mut report = Report::new();
            check_tunnel_token_file(&mut report, &path);
            let level = report.checks.last().unwrap().level;
            let expected = if exposure == 0 {
                Level::Pass
            } else {
                Level::Fail
            };
            assert_eq!(level, expected, "mode={mode:04o}");
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_token_rejects_symlinks_and_oversized_files() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        let link = fixture.path().join("link");
        std::fs::write(&target, "opaque-token").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();
        let mut report = Report::new();
        check_tunnel_token_file(&mut report, &link);
        assert_eq!(report.checks.last().unwrap().level, Level::Fail);

        let oversized = fixture.path().join("oversized");
        std::fs::write(&oversized, vec![b'x'; MAX_TUNNEL_TOKEN_BYTES as usize + 1]).unwrap();
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut report = Report::new();
        check_tunnel_token_file(&mut report, &oversized);
        assert_eq!(report.checks.last().unwrap().level, Level::Fail);
    }

    #[cfg(feature = "network")]
    #[test]
    fn validates_cloudflare_account_ids() {
        assert!(is_cloudflare_account_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_cloudflare_account_id("not-an-account-id"));
        assert!(!is_cloudflare_account_id("0123456789abcdef0123456789abcde"));
    }
}
