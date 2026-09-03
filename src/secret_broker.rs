#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "linux")]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
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

pub type ResolveFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
pub type Resolver = Arc<dyn Fn(String) -> ResolveFuture + Send + Sync>;

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

pub struct SecretBroker {
    socket_path: PathBuf,
    directory: PathBuf,
    capability_token: String,
    secrets: Arc<Mutex<Vec<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<Result<()>>>,
}

impl SecretBroker {
    pub async fn start(allowed_locators: &[String], resolver: Resolver) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (allowed_locators, resolver);
            anyhow::bail!(
                "nested 1Password secret resolution is currently supported only on Linux"
            );
        }

        #[cfg(target_os = "linux")]
        {
            let allowed = validate_policy(allowed_locators)?;
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
            let secrets = Arc::new(Mutex::new(Vec::new()));
            let task_secrets = Arc::clone(&secrets);
            let task_socket = socket_path.clone();
            let task_directory = directory.clone();
            let task_token = capability_token.clone();
            let (shutdown, shutdown_rx) = oneshot::channel();
            let join = tokio::spawn(async move {
                run_broker(
                    listener,
                    shutdown_rx,
                    allowed,
                    task_token,
                    resolver,
                    task_secrets,
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
            })
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn capability_token(&self) -> &str {
        &self.capability_token
    }

    pub async fn close(mut self) -> Vec<String> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
        self.secrets.lock().await.clone()
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
        locator.trim() == locator && !locator.chars().any(|character| character.is_control()),
        "nested secret resolver locator contains invalid whitespace or control characters"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_policy(allowed_locators: &[String]) -> Result<BTreeSet<String>> {
    anyhow::ensure!(
        !allowed_locators.is_empty(),
        "nested secret resolver requires at least one allowed locator"
    );
    allowed_locators
        .iter()
        .try_fold(BTreeSet::new(), |mut allowed, locator| {
            validate_locator(locator)?;
            allowed.insert(locator.clone());
            Result::<_>::Ok(allowed)
        })
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
    allowed: BTreeSet<String>,
    capability_token: String,
    resolver: Resolver,
    secrets: Arc<Mutex<Vec<String>>>,
) {
    let allowed = Arc::new(allowed);
    let capability_token = Arc::new(capability_token);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let allowed = Arc::clone(&allowed);
                let capability_token = Arc::clone(&capability_token);
                let resolver = Arc::clone(&resolver);
                let secrets = Arc::clone(&secrets);
                connections.spawn(async move {
                    let _ = handle_connection(stream, &allowed, &capability_token, resolver, secrets).await;
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
    allowed: &BTreeSet<String>,
    capability_token: &str,
    resolver: Resolver,
    secrets: Arc<Mutex<Vec<String>>>,
) -> Result<()> {
    let peer = stream
        .peer_cred()
        .context("failed to authenticate nested secret resolver peer")?;
    let current_uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        peer.uid() == current_uid,
        "nested secret resolver peer is owned by a different user"
    );

    let (reader, mut writer) = stream.into_split();
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
    if validate_locator(&request.locator).is_err() || !allowed.contains(&request.locator) {
        write_error(&mut writer, "locator is not authorized").await?;
        return Ok(());
    }

    let value = match resolver(request.locator).await {
        Ok(value) if value.len() <= MAX_SECRET_BYTES => value,
        Ok(_) => {
            write_error(&mut writer, "resolved secret exceeds size limit").await?;
            return Ok(());
        }
        Err(_) => {
            write_error(&mut writer, "1Password secret resolution failed").await?;
            return Ok(());
        }
    };
    {
        let mut known = secrets.lock().await;
        if !value.is_empty() && !known.iter().any(|secret| secret == &value) {
            known.push(value.clone());
        }
    }
    write_response(
        &mut writer,
        &ResolveResponse {
            value: Some(&value),
            error: None,
        },
    )
    .await
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::Value;

    use super::*;

    fn resolver(
        counter: Arc<AtomicUsize>,
        expected: &'static str,
        value: &'static str,
    ) -> Resolver {
        Arc::new(move |locator| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                anyhow::ensure!(locator == expected, "unexpected locator");
                Ok(value.to_owned())
            })
        })
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
        let calls = Arc::new(AtomicUsize::new(0));
        let locator = "op://vault/item/field";
        let broker = SecretBroker::start(
            &[locator.to_owned()],
            resolver(Arc::clone(&calls), locator, "resolved-secret"),
        )
        .await
        .unwrap();
        let response = request(broker.socket_path(), broker.capability_token(), locator)
            .await
            .unwrap();
        assert_eq!(response["value"], "resolved-secret");
        assert!(response["error"].is_null());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let secrets = broker.close().await;
        assert_eq!(secrets, ["resolved-secret"]);
    }

    #[tokio::test]
    async fn denied_and_malformed_locators_do_not_reach_resolver() {
        let calls = Arc::new(AtomicUsize::new(0));
        let allowed = "op://vault/item/allowed";
        let broker = SecretBroker::start(
            &[allowed.to_owned()],
            resolver(Arc::clone(&calls), allowed, "secret"),
        )
        .await
        .unwrap();
        for locator in ["op://vault/item/denied", "plaintext", "op://bad\nlocator"] {
            let response = request(broker.socket_path(), broker.capability_token(), locator)
                .await
                .unwrap();
            assert!(response["value"].is_null());
            assert!(response["error"].is_string());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        broker.close().await;
    }

    #[tokio::test]
    async fn capabilities_are_isolated_and_dead_after_close() {
        let locator = "op://vault/item/field";
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let broker_a = SecretBroker::start(
            &[locator.to_owned()],
            resolver(Arc::clone(&calls_a), locator, "secret-a"),
        )
        .await
        .unwrap();
        let broker_b = SecretBroker::start(
            &[locator.to_owned()],
            resolver(Arc::clone(&calls_b), locator, "secret-b"),
        )
        .await
        .unwrap();
        let response = request(broker_b.socket_path(), broker_a.capability_token(), locator)
            .await
            .unwrap();
        assert!(response["value"].is_null());
        assert_eq!(calls_b.load(Ordering::SeqCst), 0);

        let dead_socket = broker_a.socket_path().to_path_buf();
        let dead_token = broker_a.capability_token().to_owned();
        broker_a.close().await;
        assert!(request(&dead_socket, &dead_token, locator).await.is_err());
        broker_b.close().await;
    }

    #[test]
    fn locator_validation_is_exact_and_fail_closed() {
        assert!(validate_locator("op://vault/item/field").is_ok());
        for invalid in [
            "",
            "op://",
            " op://vault/item/field",
            "op://vault/item/field\n",
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
        let resolver: Resolver = Arc::new(|_| Box::pin(async { Ok("secret".to_owned()) }));
        let error = SecretBroker::start(&["op://vault/item/field".to_owned()], resolver)
            .await
            .err()
            .expect("non-Linux nested resolution fails closed");
        assert!(error.to_string().contains("supported only on Linux"));
    }
}
