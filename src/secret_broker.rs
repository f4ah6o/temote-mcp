use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "linux")]
use tokio::net::{UnixListener, UnixStream};
#[cfg(target_os = "linux")]
use tokio::sync::{Semaphore, watch};
#[cfg(target_os = "linux")]
use tokio::task::JoinSet;
#[cfg(target_os = "linux")]
use uuid::Uuid;

pub const SOCKET_ENV: &str = "TEMOTE_MCP_SECRET_RESOLVER_SOCKET";
pub const TOKEN_ENV: &str = "TEMOTE_MCP_SECRET_RESOLVER_TOKEN";

#[cfg(target_os = "linux")]
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_LOCATOR_BYTES: usize = 4096;
#[cfg(target_os = "linux")]
const MAX_SECRET_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const MAX_IN_FLIGHT_CONNECTIONS: usize = 16;
#[cfg(target_os = "linux")]
const MAX_PROCESS_ANCESTORS: usize = 256;

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct ResolveRequest {
    token: String,
    locator: String,
}

#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct ResolveResponse<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct ProcessStat {
    parent_pid: u32,
    start_time: u64,
}

pub struct SecretBroker {
    socket_path: PathBuf,
    directory: PathBuf,
    capability_token: String,
    secrets: Vec<String>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<Result<()>>>,
    #[cfg(target_os = "linux")]
    target: watch::Sender<Option<ProcessIdentity>>,
}

impl SecretBroker {
    pub async fn start(resolved_locators: BTreeMap<String, String>) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = resolved_locators;
            anyhow::bail!(
                "nested 1Password secret resolution is currently supported only on Linux"
            );
        }

        #[cfg(target_os = "linux")]
        {
            let resolved = validate_policy(resolved_locators)?;
            let directory = create_capability_directory()?;
            let socket_path = directory.join("resolver.sock");
            let listener = UnixListener::bind(&socket_path).with_context(|| {
                format!(
                    "failed to create nested secret resolver at {}",
                    socket_path.display()
                )
            })?;
            set_mode(&socket_path, 0o600)?;
            let capability_token = Uuid::new_v4().simple().to_string();
            let mut secrets = Vec::new();
            for value in resolved.values() {
                if !value.is_empty() && !secrets.iter().any(|known| known == value) {
                    secrets.push(value.clone());
                }
            }
            let task_socket = socket_path.clone();
            let task_directory = directory.clone();
            let task_token = capability_token.clone();
            let (target, target_rx) = watch::channel(None);
            let (shutdown, shutdown_rx) = oneshot::channel();
            let join = tokio::spawn(async move {
                run_broker(
                    listener,
                    shutdown_rx,
                    Arc::new(resolved),
                    Arc::new(task_token),
                    target_rx,
                )
                .await;
                let _ = tokio::fs::remove_file(&task_socket).await;
                let _ = tokio::fs::remove_dir(&task_directory).await;
                Ok(())
            });
            Ok(Self {
                socket_path,
                directory,
                capability_token,
                secrets,
                shutdown: Some(shutdown),
                join: Some(join),
                target,
            })
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn capability_token(&self) -> &str {
        &self.capability_token
    }

    pub fn bind_target_pid(&self, pid: u32) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            anyhow::bail!(
                "nested 1Password secret resolution is currently supported only on Linux"
            );
        }

        #[cfg(target_os = "linux")]
        {
            let stat = read_process_stat(pid)?;
            self.target
                .send(Some(ProcessIdentity {
                    pid,
                    start_time: stat.start_time,
                }))
                .context("nested secret resolver closed before target binding")?;
            Ok(())
        }
    }

    pub async fn close(mut self) -> Vec<String> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
        self.secrets.clone()
    }
}

impl Drop for SecretBroker {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

pub fn validate_locator(locator: &str) -> Result<()> {
    anyhow::ensure!(
        !locator.is_empty() && locator.len() <= MAX_LOCATOR_BYTES,
        "1Password locator must contain between 1 and {MAX_LOCATOR_BYTES} bytes"
    );
    anyhow::ensure!(
        locator.starts_with("op://"),
        "nested secret resolver locators must start with op://"
    );
    anyhow::ensure!(
        locator.len() > "op://".len(),
        "nested secret resolver locator is incomplete"
    );
    anyhow::ensure!(
        locator.trim() == locator
            && !locator.chars().any(char::is_control)
            && !locator.chars().any(is_visual_format_character),
        "nested secret resolver locator contains invalid whitespace, control, or Unicode format characters"
    );
    Ok(())
}

fn is_visual_format_character(character: char) -> bool {
    matches!(
        character as u32,
        0x00ad
            | 0x061c
            | 0x180e
            | 0xfeff
            | 0xe0001
            | 0xfff9..=0xfffb
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0xe0020..=0xe007f
    )
}

#[cfg(target_os = "linux")]
fn validate_policy(
    resolved_locators: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    anyhow::ensure!(
        !resolved_locators.is_empty(),
        "nested secret resolver requires at least one allowed locator"
    );
    for (locator, value) in &resolved_locators {
        validate_locator(locator)?;
        anyhow::ensure!(
            value.len() <= MAX_SECRET_BYTES,
            "resolved secret exceeds size limit"
        );
    }
    Ok(resolved_locators)
}

#[cfg(target_os = "linux")]
fn create_capability_directory() -> Result<PathBuf> {
    for _ in 0..8 {
        let directory = std::env::temp_dir().join(format!(
            "temote-mcp-secret-resolver-{}",
            Uuid::new_v4().simple()
        ));
        match std::fs::create_dir(&directory) {
            Ok(()) => {
                set_mode(&directory, 0o700)?;
                return Ok(directory);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create nested secret resolver directory {}",
                        directory.display()
                    )
                });
            }
        }
    }
    anyhow::bail!("failed to allocate nested secret resolver capability directory")
}

#[cfg(target_os = "linux")]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(target_os = "linux")]
async fn run_broker(
    listener: UnixListener,
    mut shutdown: oneshot::Receiver<()>,
    resolved: Arc<BTreeMap<String, String>>,
    capability_token: Arc<String>,
    target: watch::Receiver<Option<ProcessIdentity>>,
) {
    let semaphore = Arc::new(Semaphore::new(MAX_IN_FLIGHT_CONNECTIONS));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let resolved = Arc::clone(&resolved);
                let capability_token = Arc::clone(&capability_token);
                let target = target.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = handle_connection(stream, &resolved, &capability_token, target).await;
                });
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                let _ = joined;
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

#[cfg(target_os = "linux")]
async fn handle_connection(
    stream: UnixStream,
    resolved: &BTreeMap<String, String>,
    capability_token: &str,
    mut target: watch::Receiver<Option<ProcessIdentity>>,
) -> Result<()> {
    let peer = stream
        .peer_cred()
        .context("failed to authenticate nested secret resolver peer")?;
    let current_uid = unsafe { libc::geteuid() };
    let peer_pid = peer
        .pid()
        .context("nested secret resolver peer PID is unavailable")? as u32;
    let target = wait_for_target_identity(&mut target).await?;
    let process_authorized = peer.uid() == current_uid && process_is_descendant(peer_pid, target)?;

    let (reader, mut writer) = stream.into_split();
    if !process_authorized {
        write_error(&mut writer, "unauthorized process").await?;
        return Ok(());
    }

    let mut line = String::new();
    let read = tokio::time::timeout(REQUEST_TIMEOUT, async {
        BufReader::new(reader)
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_line(&mut line)
            .await
    })
    .await
    .context("nested secret resolver request timed out")??;
    anyhow::ensure!(read > 0, "nested secret resolver request closed early");
    if read > MAX_REQUEST_BYTES {
        write_error(&mut writer, "invalid request").await?;
        return Ok(());
    }
    let request = match serde_json::from_str::<ResolveRequest>(line.trim_end()) {
        Ok(request) => request,
        Err(_) => {
            write_error(&mut writer, "invalid request").await?;
            return Ok(());
        }
    };
    if !constant_time_eq(request.token.as_bytes(), capability_token.as_bytes()) {
        write_error(&mut writer, "unauthorized capability").await?;
        return Ok(());
    }
    if validate_locator(&request.locator).is_err() {
        write_error(&mut writer, "locator is not authorized").await?;
        return Ok(());
    }
    let Some(value) = resolved.get(&request.locator) else {
        write_error(&mut writer, "locator is not authorized").await?;
        return Ok(());
    };
    write_response(
        &mut writer,
        &ResolveResponse {
            value: Some(value),
            error: None,
        },
    )
    .await
}

#[cfg(target_os = "linux")]
async fn wait_for_target_identity(
    target: &mut watch::Receiver<Option<ProcessIdentity>>,
) -> Result<ProcessIdentity> {
    if let Some(identity) = *target.borrow() {
        return Ok(identity);
    }
    let value = tokio::time::timeout(REQUEST_TIMEOUT, target.wait_for(Option::is_some))
        .await
        .context("nested secret resolver target binding timed out")?
        .context("nested secret resolver closed before target binding")?;
    value.context("nested secret resolver target binding is unavailable")
}

#[cfg(target_os = "linux")]
fn process_is_descendant(peer_pid: u32, root: ProcessIdentity) -> Result<bool> {
    let mut current = peer_pid;
    for _ in 0..MAX_PROCESS_ANCESTORS {
        let stat = match read_process_stat(current) {
            Ok(stat) => stat,
            Err(_) => return Ok(false),
        };
        if current == root.pid {
            return Ok(stat.start_time == root.start_time);
        }
        if stat.parent_pid == 0 || stat.parent_pid == current {
            return Ok(false);
        }
        current = stat.parent_pid;
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn read_process_stat(pid: u32) -> Result<ProcessStat> {
    let path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to inspect process identity for PID {pid}"))?;
    let close = stat
        .rfind(')')
        .context("process stat is missing command terminator")?;
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    anyhow::ensure!(fields.len() > 19, "process stat is incomplete");
    let parent_pid = fields[1]
        .parse::<u32>()
        .context("process stat has invalid parent PID")?;
    let start_time = fields[19]
        .parse::<u64>()
        .context("process stat has invalid start time")?;
    Ok(ProcessStat {
        parent_pid,
        start_time,
    })
}

#[cfg(target_os = "linux")]
async fn write_error(writer: &mut tokio::net::unix::OwnedWriteHalf, error: &str) -> Result<()> {
    write_response(
        writer,
        &ResolveResponse {
            value: None,
            error: Some(error),
        },
    )
    .await
}

#[cfg(target_os = "linux")]
async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ResolveResponse<'_>,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(response).context("failed to encode resolver response")?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.shutdown().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&left, &right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use serde_json::Value;

    use super::*;

    async fn broker(locator: &str, value: &str) -> SecretBroker {
        let broker = SecretBroker::start(BTreeMap::from([(locator.to_owned(), value.to_owned())]))
            .await
            .unwrap();
        broker.bind_target_pid(std::process::id()).unwrap();
        broker
    }

    async fn request(socket: &Path, token: &str, locator: &str) -> Result<Value> {
        let mut stream = UnixStream::connect(socket).await?;
        let request = serde_json::json!({"token": token, "locator": locator});
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        stream.shutdown().await?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).await?;
        Ok(serde_json::from_str(response.trim_end())?)
    }

    #[tokio::test]
    async fn allowed_locator_resolves_and_is_captured_for_output_redaction() {
        let locator = "op://vault/item/field";
        let broker = broker(locator, "resolved-secret").await;
        let response = request(broker.socket_path(), broker.capability_token(), locator)
            .await
            .unwrap();
        assert_eq!(response["value"], "resolved-secret");
        assert!(response["error"].is_null());
        let secrets = broker.close().await;
        assert_eq!(secrets, ["resolved-secret"]);
    }

    #[tokio::test]
    async fn denied_and_malformed_locators_fail_closed() {
        let allowed = "op://vault/item/allowed";
        let broker = broker(allowed, "secret").await;
        for locator in ["op://vault/item/denied", "plaintext", "op://bad\nlocator"] {
            let response = request(broker.socket_path(), broker.capability_token(), locator)
                .await
                .unwrap();
            assert!(response["value"].is_null());
            assert!(response["error"].is_string());
        }
        broker.close().await;
    }

    #[tokio::test]
    async fn capabilities_are_isolated_and_dead_after_close() {
        let locator = "op://vault/item/field";
        let broker_a = broker(locator, "secret-a").await;
        let broker_b = broker(locator, "secret-b").await;
        let response = request(broker_b.socket_path(), broker_a.capability_token(), locator)
            .await
            .unwrap();
        assert!(response["value"].is_null());

        let dead_socket = broker_a.socket_path().to_path_buf();
        let dead_token = broker_a.capability_token().to_owned();
        broker_a.close().await;
        assert!(request(&dead_socket, &dead_token, locator).await.is_err());
        broker_b.close().await;
    }

    #[tokio::test]
    async fn correct_token_is_denied_outside_bound_process_tree() {
        use std::process::Stdio;

        let locator = "op://vault/item/field";
        let broker = SecretBroker::start(BTreeMap::from([(
            locator.to_owned(),
            "resolved-secret".to_owned(),
        )]))
        .await
        .unwrap();
        let mut target = tokio::process::Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let target_pid = target.id().unwrap();
        broker.bind_target_pid(target_pid).unwrap();

        let response = request(broker.socket_path(), broker.capability_token(), locator)
            .await
            .unwrap();
        assert!(response["value"].is_null());
        assert_eq!(response["error"], "unauthorized process");

        let _ = target.kill().await;
        let _ = target.wait().await;
        broker.close().await;
    }

    #[test]
    fn locator_validation_is_exact_and_operator_safe() {
        assert!(validate_locator("op://vault/日本語 item/field").is_ok());
        for invalid in [
            "",
            "op://",
            " op://vault/item/field",
            "op://vault/item/field\n",
            "op://vault/item/\u{202e}field",
            "op://vault/item/\u{2066}field",
            "op://vault/item/\u{2067}field",
            "op://vault/item/\u{200b}field",
        ] {
            assert!(validate_locator(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn capability_comparison_checks_all_bytes() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"samf"));
        assert!(!constant_time_eq(b"same", b"short"));
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod non_linux_tests {
    use super::*;

    #[tokio::test]
    async fn nested_resolution_fails_closed_until_platform_support_exists() {
        let error = SecretBroker::start(BTreeMap::from([(
            "op://vault/item/field".to_owned(),
            "secret".to_owned(),
        )]))
        .await
        .err()
        .expect("non-Linux nested resolution fails closed");
        assert!(error.to_string().contains("supported only on Linux"));
    }
}
