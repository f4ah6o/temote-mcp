use std::ffi::OsString;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

const TUNNEL_ID_PREFIX: &str = "tunnel_";
const TUNNEL_ID_HEX_LEN: usize = 32;
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(15);

pub struct OpenAiTunnelConfig {
    pub tunnel_id: String,
    pub runtime_key_env: &'static str,
    pub binary: OsString,
}

pub fn config_from_env() -> Result<OpenAiTunnelConfig> {
    let tunnel_id = required_env("CONTROL_PLANE_TUNNEL_ID")?;
    anyhow::ensure!(
        valid_tunnel_id(&tunnel_id),
        "CONTROL_PLANE_TUNNEL_ID must match tunnel_<32 lowercase hexadecimal characters>"
    );

    let runtime_key_env = if nonempty_env("CONTROL_PLANE_API_KEY") {
        "CONTROL_PLANE_API_KEY"
    } else if nonempty_env("OPENAI_API_KEY") {
        "OPENAI_API_KEY"
    } else {
        anyhow::bail!(
            "CONTROL_PLANE_API_KEY is required for OpenAI Secure MCP Tunnel (OPENAI_API_KEY is accepted only as the official tunnel-client fallback)"
        );
    };

    let binary = std::env::var_os("TUNNEL_CLIENT_BIN")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("tunnel-client"));

    Ok(OpenAiTunnelConfig {
        tunnel_id,
        runtime_key_env,
        binary,
    })
}

pub async fn start(origin: SocketAddr) -> Result<Child> {
    ensure_loopback(origin)?;
    let config = config_from_env()?;
    let mcp_url = local_mcp_url(origin);
    Command::new(&config.binary)
        .args([
            "run",
            "--control-plane.tunnel-id",
            &config.tunnel_id,
            "--mcp.server-url",
            &mcp_url,
            "--health.listen-addr",
            "127.0.0.1:0",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "failed to start OpenAI Secure MCP tunnel-client; install the supported tunnel-client and ensure {} is configured",
                config.runtime_key_env
            )
        })
}

pub async fn doctor_control_plane() -> Result<String> {
    let config = config_from_env()?;
    let mut command = Command::new(&config.binary);
    command
        .args(["admin", "tunnels", "get", &config.tunnel_id])
        .env_remove("OPENAI_ADMIN_KEY")
        .stdin(Stdio::null());
    let output = tokio::time::timeout(DOCTOR_TIMEOUT, command.output())
        .await
        .context("timed out while validating OpenAI Secure MCP Tunnel runtime access")?
        .context("cannot execute tunnel-client")?;
    anyhow::ensure!(
        output.status.success(),
        "tunnel-client could not read the configured tunnel with the runtime credential (exit status {})",
        output.status
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
    let output = tokio::time::timeout(
        DOCTOR_TIMEOUT,
        Command::new(&binary)
            .arg("--version")
            .stdin(Stdio::null())
            .output(),
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

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    let value = value.trim().to_owned();
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
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
}
