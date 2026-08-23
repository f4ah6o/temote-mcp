use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::{Child, Command};

use crate::profile::Profile;
use crate::provider::PublicEndpoint;

pub const TAILSCALE_HTTPS_PORTS: [u16; 3] = [443, 8443, 10000];

pub struct ManagedIngress {
    profile: Profile,
    child: Child,
}

impl ManagedIngress {
    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

pub async fn resolve_public_url(
    profile: Profile,
    explicit: Option<String>,
    managed_ingress: bool,
) -> Result<PublicEndpoint> {
    match profile {
        Profile::Cloudflare => {
            let value = explicit
                .or_else(|| std::env::var("TEMOTE_MCP_PUBLIC_URL").ok())
                .context(
                    "TEMOTE_MCP_PUBLIC_URL is required for the cloudflare profile; pass --public-url or create ~/.config/temote-mcp/public.env",
                )?;
            PublicEndpoint::parse(&value)
        }
        Profile::Openai => {
            anyhow::bail!("openai is a private connection profile, not a public ingress profile")
        }
        Profile::Tailscale => {
            // A CLI/shell value is an intentional override (for example, an
            // externally managed Funnel on 8443). Otherwise prefer the live
            // node DNS name so a legacy Cloudflare public.env does not leak
            // into the Tailscale profile.
            if let Some(value) = explicit {
                return PublicEndpoint::parse(&value);
            }
            match tailscale_dns_name().await {
                Ok(hostname) => {
                    let port = if managed_ingress {
                        preferred_funnel_https_port().await?
                    } else {
                        443
                    };
                    PublicEndpoint::parse(&tailscale_origin(&hostname, port))
                }
                Err(derive_error) => {
                    let value = std::env::var("TEMOTE_MCP_PUBLIC_URL").with_context(|| {
                        format!(
                            "could not derive the Tailscale public endpoint ({derive_error:#}); set TEMOTE_MCP_PUBLIC_URL explicitly"
                        )
                    })?;
                    PublicEndpoint::parse(&value)
                }
            }
        }
    }
}

pub async fn start(
    profile: Profile,
    addr: SocketAddr,
    tunnel_token_file: Option<&Path>,
    tailscale_https_port: Option<u16>,
) -> Result<ManagedIngress> {
    let child = match profile {
        Profile::Cloudflare => {
            let token_file = tunnel_token_file.context(
                "Cloudflare profile requires a tunnel token file when temote-mcp owns ingress",
            )?;
            Command::new("cloudflared")
                .args(["tunnel", "run", "--token-file"])
                .arg(token_file)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| {
                    format!("failed to start cloudflared with {}", token_file.display())
                })?
        }
        Profile::Openai => {
            anyhow::bail!("openai is a private connection profile, not a public ingress profile")
        }
        Profile::Tailscale => {
            anyhow::ensure!(
                addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST),
                "tailscale profile currently requires the origin to listen on 127.0.0.1"
            );
            let https_port =
                tailscale_https_port.context("missing managed Tailscale HTTPS port")?;
            anyhow::ensure!(
                TAILSCALE_HTTPS_PORTS.contains(&https_port),
                "Tailscale Funnel HTTPS port must be one of 443, 8443, or 10000"
            );
            anyhow::ensure!(
                !funnel_https_port_configured(https_port).await?,
                "Tailscale Funnel HTTPS port {https_port} is already configured; refusing to replace an existing Funnel owned outside this temote-mcp process"
            );
            let target = format!("http://127.0.0.1:{}", addr.port());
            let https_arg = format!("--https={https_port}");
            Command::new("tailscale")
                .args(["funnel", "--yes", &https_arg, &target])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("failed to start Tailscale Funnel for {target}"))?
        }
    };
    Ok(ManagedIngress { profile, child })
}

pub async fn configured_funnel_https_ports() -> Result<BTreeSet<u16>> {
    let output = Command::new("tailscale")
        .args(["funnel", "status", "--json"])
        .output()
        .await
        .context("failed to execute `tailscale funnel status --json`")?;
    anyhow::ensure!(
        output.status.success(),
        "`tailscale funnel status --json` failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    anyhow::ensure!(
        output.stdout.len() <= 4 * 1024 * 1024,
        "tailscale funnel status response is too large"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("invalid `tailscale funnel status --json` response")?;
    let mut ports = BTreeSet::new();
    if let Some(tcp) = value.get("TCP").and_then(serde_json::Value::as_object) {
        for key in tcp.keys() {
            if let Ok(port) = key.parse::<u16>() {
                ports.insert(port);
            }
        }
    }
    if let Some(web) = value.get("Web").and_then(serde_json::Value::as_object) {
        for key in web.keys() {
            if let Some(port) = key
                .rsplit(':')
                .next()
                .and_then(|value| value.parse::<u16>().ok())
            {
                ports.insert(port);
            }
        }
    }
    Ok(ports)
}

pub async fn funnel_https_port_configured(port: u16) -> Result<bool> {
    Ok(configured_funnel_https_ports().await?.contains(&port))
}

pub async fn preferred_funnel_https_port() -> Result<u16> {
    let configured = configured_funnel_https_ports().await?;
    TAILSCALE_HTTPS_PORTS
        .into_iter()
        .find(|port| !configured.contains(port))
        .context(
            "all supported Tailscale Funnel HTTPS ports (443, 8443, 10000) are already configured",
        )
}

pub fn tailscale_origin(hostname: &str, port: u16) -> String {
    if port == 443 {
        format!("https://{hostname}")
    } else {
        format!("https://{hostname}:{port}")
    }
}

pub async fn tailscale_dns_name() -> Result<String> {
    let output = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
        .context("failed to execute `tailscale status --json`")?;
    anyhow::ensure!(
        output.status.success(),
        "`tailscale status --json` failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    anyhow::ensure!(
        output.stdout.len() <= 4 * 1024 * 1024,
        "tailscale status response is too large"
    );
    let status: TailscaleStatus = serde_json::from_slice(&output.stdout)
        .context("invalid `tailscale status --json` response")?;
    let dns_name = status
        .self_node
        .and_then(|node| node.dns_name)
        .map(|value| value.trim().trim_end_matches('.').to_owned())
        .filter(|value| !value.is_empty())
        .context("tailscale status did not report Self.DNSName")?;
    anyhow::ensure!(
        dns_name.ends_with(".ts.net"),
        "tailscale Self.DNSName is not a *.ts.net hostname"
    );
    Ok(dns_name)
}

#[derive(Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Self")]
    self_node: Option<TailscaleSelf>,
}

#[derive(Deserialize)]
struct TailscaleSelf {
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_tailscale_origins() {
        assert_eq!(
            tailscale_origin("node.example.ts.net", 443),
            "https://node.example.ts.net"
        );
        assert_eq!(
            tailscale_origin("node.example.ts.net", 8443),
            "https://node.example.ts.net:8443"
        );
    }

    #[test]
    fn parses_tailscale_dns_name_shape() {
        let status: TailscaleStatus = serde_json::from_value(serde_json::json!({
            "Self": { "DNSName": "workstation.example.ts.net." }
        }))
        .unwrap();
        assert_eq!(
            status.self_node.unwrap().dns_name.as_deref(),
            Some("workstation.example.ts.net.")
        );
    }
}
