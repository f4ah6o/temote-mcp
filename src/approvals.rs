use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::config::{self, Session};
use crate::sandbox;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Probe,
    Approval {
        request: Request,
    },
    Activity {
        title: String,
        detail: Option<String>,
    },
    OnePasswordServiceAccount {
        request: ServiceAccountRequest,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ServiceAccountRequest {
    Status,
    Run {
        cwd: PathBuf,
        command: Vec<String>,
        env_files: Vec<PathBuf>,
        environment: BTreeMap<String, String>,
    },
}

#[derive(Serialize, Deserialize)]
struct ServiceAccountResponse {
    result: Option<Value>,
    error: Option<String>,
}

pub async fn request(
    session_id: &str,
    operation: &str,
    detail: String,
    cwd: PathBuf,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::Approval { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
}

pub async fn onepassword_service_account_status(session_id: &str) -> Result<Value> {
    service_account_request(session_id, ServiceAccountRequest::Status).await
}

pub async fn onepassword_service_account_run(
    session_id: &str,
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment: BTreeMap<String, String>,
) -> Result<Value> {
    service_account_request(
        session_id,
        ServiceAccountRequest::Run {
            cwd,
            command,
            env_files,
            environment,
        },
    )
    .await
}

async fn service_account_request(
    session_id: &str,
    request: ServiceAccountRequest,
) -> Result<Value> {
    let path = config::socket_path(session_id)?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::OnePasswordServiceAccount {
            request,
        })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    let response: ServiceAccountResponse = serde_json::from_str(response.trim())
        .context("invalid 1Password service-account response")?;
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }
    response
        .result
        .context("1Password service-account response is missing result")
}

/// Sends a one-way activity update to the `start` screen. Activity reporting is
/// deliberately best-effort: an MCP operation must not fail just because its UI
/// was closed between loading the session and completing the operation.
pub async fn activity(session_id: &str, title: impl Into<String>, detail: Option<String>) {
    let Ok(path) = config::socket_path(session_id) else {
        return;
    };
    let Ok(mut stream) = UnixStream::connect(path).await else {
        return;
    };
    let message = Message::Activity {
        title: title.into(),
        detail,
    };
    let Ok(bytes) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.shutdown().await;
}

pub async fn start(session_id: Option<&str>, yolo: bool) -> Result<()> {
    let service_account_token = std::env::var("OP_SERVICE_ACCOUNT_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let id = config::session_id(session_id)?;
    config::remove_inactive_socket(&id).await?;
    let mut session = config::new_session(&std::env::current_dir()?, Some(&id), yolo)?;
    let path = config::socket_path(&session.id)?;
    let state_dir = path.parent().context("session socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    session.process_id = std::process::id();
    if let Err(error) = config::save_session(&session).await {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    eprintln!(
        "local-mcp session: {}\ncwd: {}\nmode: {}\n\
         Give this session ID to the agent so it can include it in local-mcp tool calls.\n\
         Commands: /permission ask|yolo|allow <directory>|revoke <directory>|list|status\n\
         Press Ctrl-C to stop.",
        session.id,
        session.cwd.display(),
        if session.yolo { "yolo" } else { "ask" }
    );
    if session.yolo {
        eprintln!(
            "WARNING: YOLO mode grants MCP tools this user's full filesystem, process, environment, and network permissions without local approval."
        );
    }
    eprintln!(
        "1Password service account: {}",
        if service_account_token.is_some() {
            "configured (token kept only by this session process)"
        } else {
            "not configured"
        }
    );

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<(Request, UnixStream)>::new();
    let result = run_session(
        &listener,
        &mut input,
        &mut session,
        &mut pending,
        service_account_token.as_deref(),
    )
    .await;
    session.process_id = 0;
    if let Err(error) = config::save_session(&session).await {
        eprintln!("failed to mark session stopped: {error:#}");
    }
    let _ = tokio::fs::remove_file(&path).await;
    result
}

async fn run_session(
    listener: &UnixListener,
    input: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    session: &mut Session,
    pending: &mut VecDeque<(Request, UnixStream)>,
    service_account_token: Option<&str>,
) -> Result<()> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (mut stream, _) = connection?;
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await?;
                let message: Message = serde_json::from_str(&line).context("invalid session message")?;
                match message {
                    Message::Probe => {
                        stream.write_all(b"active\n").await?;
                    }
                    Message::Activity { title, detail } => show_activity(&title, detail.as_deref()),
                    Message::OnePasswordServiceAccount { request } => {
                        let session = session.clone();
                        let token = service_account_token.map(str::to_owned);
                        tokio::spawn(async move {
                            let response = handle_service_account_request(&session, token.as_deref(), request).await;
                            let response = match response {
                                Ok(result) => ServiceAccountResponse { result: Some(result), error: None },
                                Err(error) => ServiceAccountResponse { result: None, error: Some(format!("{error:#}")) },
                            };
                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = stream.write_all(&bytes).await;
                                let _ = stream.write_all(b"\n").await;
                                let _ = stream.shutdown().await;
                            }
                        });
                    }
                    Message::Approval { request } if session.yolo => {
                        eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                        stream.write_all(b"allow\n").await?;
                    }
                    Message::Approval { request } => {
                        show_request(&request)?;
                        pending.push_back((request, stream));
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { anyhow::bail!("session input closed") };
                handle_input(line.trim(), session, pending).await?;
            }
            signal = &mut ctrl_c => {
                signal.context("failed to receive Ctrl-C")?;
                eprintln!("Stopping local-mcp session {}", session.id);
                return Ok(());
            }
        }
    }
}

async fn handle_service_account_request(
    session: &Session,
    token: Option<&str>,
    request: ServiceAccountRequest,
) -> Result<Value> {
    let Some(token) = token else {
        return match request {
            ServiceAccountRequest::Status => {
                Ok(json!({"configured": false, "authenticated": false}))
            }
            ServiceAccountRequest::Run { .. } => {
                anyhow::bail!(
                    "1Password service account is not configured; start the session with OP_SERVICE_ACCOUNT_TOKEN set"
                )
            }
        };
    };

    match request {
        ServiceAccountRequest::Status => service_account_status(session, token).await,
        ServiceAccountRequest::Run {
            cwd,
            command,
            env_files,
            environment,
        } => service_account_run(session, token, cwd, command, env_files, environment).await,
    }
}

async fn service_account_status(session: &Session, token: &str) -> Result<Value> {
    let command = vec![
        "op".to_owned(),
        "whoami".to_owned(),
        "--format=json".to_owned(),
    ];
    let mut environment = HashMap::new();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &command,
        &session.cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to check 1Password service-account authentication")?;
    let authenticated = output.status == 0;
    let account = if authenticated {
        serde_json::from_str::<Value>(&output.stdout)
            .ok()
            .map(|value| {
                json!({
                    "account_uuid": value.get("account_uuid").cloned().unwrap_or(Value::Null),
                    "account_url": value.get("account_url").cloned().unwrap_or(Value::Null),
                    "user_uuid": value.get("user_uuid").cloned().unwrap_or(Value::Null),
                })
            })
    } else {
        None
    };
    Ok(json!({
        "configured": true,
        "authenticated": authenticated,
        "account": account,
    }))
}

async fn service_account_run(
    session: &Session,
    token: &str,
    cwd: PathBuf,
    command: Vec<String>,
    env_files: Vec<PathBuf>,
    environment_refs: BTreeMap<String, String>,
) -> Result<Value> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = config::resolve_cwd(session, Some(&cwd))?;
    let env_files = env_files
        .into_iter()
        .map(|path| {
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            config::resolve_existing_path(session, &path)
        })
        .collect::<Result<Vec<_>>>()?;
    for (name, reference) in &environment_refs {
        anyhow::ensure!(
            valid_environment_name(name),
            "invalid environment variable name: {name}"
        );
        anyhow::ensure!(
            !name.starts_with("OP_"),
            "environment variables beginning with OP_ are reserved for 1Password CLI"
        );
        anyhow::ensure!(
            reference.starts_with("op://"),
            "1Password environment values must be op:// secret references"
        );
    }

    let mut op_command = vec!["op".to_owned(), "run".to_owned()];
    for path in &env_files {
        op_command.push(format!("--env-file={}", path.display()));
    }
    op_command.push("--".to_owned());
    op_command.push("/usr/bin/env".to_owned());
    op_command.push("-u".to_owned());
    op_command.push("OP_SERVICE_ACCOUNT_TOKEN".to_owned());
    op_command.extend(command);

    let mut environment = environment_refs.into_iter().collect::<HashMap<_, _>>();
    environment.insert("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), token.to_owned());
    let output = sandbox::run_unrestricted_with_env(
        &op_command,
        &cwd,
        None,
        &environment,
        &["OP_SERVICE_ACCOUNT_TOKEN"],
    )
    .await
    .context("failed to run command through 1Password service account")?;
    let stdout = redact_token(&output.stdout, token);
    let stderr = redact_token(&output.stderr, token);
    Ok(json!({
        "exit_code": output.status,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": output.truncated,
    }))
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn redact_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "[REDACTED_SERVICE_ACCOUNT_TOKEN]")
    }
}

fn show_activity(title: &str, detail: Option<&str>) {
    eprintln!("\n• {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        for line in detail.lines() {
            eprintln!("  {line}");
        }
    }
}

async fn handle_input(
    input: &str,
    session: &mut Session,
    pending: &mut VecDeque<(Request, UnixStream)>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            session.yolo = true;
            config::save_session(session).await?;
            eprintln!("Permissions: yolo (full host permissions; no local approvals)");
            while let Some((request, mut stream)) = pending.pop_front() {
                eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                stream.write_all(b"allow\n").await?;
            }
        }
        "/permissions ask" | "/permission ask" => {
            session.yolo = false;
            config::save_session(session).await?;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"allow\n").await?;
            show_next(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            show_next(pending)?;
        }
        "/permission list" | "/permissions list" => show_permissions(session),
        "/permission status" | "/permissions status" => {
            eprintln!("Permissions: {}", if session.yolo { "yolo" } else { "ask" });
            show_permissions(session);
        }
        command if permission_arg(command, "allow").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "allow").unwrap()).as_path(),
            )?;
            if !session.permitted_directories.contains(&directory) {
                session.permitted_directories.push(directory.clone());
                session.permitted_directories.sort();
                config::save_session(session).await?;
            }
            eprintln!("Allowed sandbox root: {}", directory.display());
        }
        command if permission_arg(command, "revoke").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "revoke").unwrap()).as_path(),
            )?;
            if directory == session.cwd {
                eprintln!("Cannot revoke the session cwd");
            } else {
                session
                    .permitted_directories
                    .retain(|item| item != &directory);
                config::save_session(session).await?;
                eprintln!("Revoked sandbox root: {}", directory.display());
            }
        }
        "/permission" | "/permissions" | "/permission help" | "/permissions help" => {
            eprintln!("/permission ask|yolo|allow <directory>|revoke <directory>|list|status");
        }
        "" => {}
        command if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            eprintln!("Denied request (unrecognized response: {command})");
            show_next(pending)?;
        }
        command => eprintln!("Unknown command: {command}"),
    }
    Ok(())
}

fn permission_arg<'a>(command: &'a str, action: &str) -> Option<&'a str> {
    ["/permission", "/permissions"]
        .into_iter()
        .find_map(|prefix| {
            command
                .strip_prefix(&format!("{prefix} {action} "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", path.display());
    }
}

fn show_request(request: &Request) -> Result<()> {
    eprintln!(
        "\n[{}] {}\ncwd: {}\n{}",
        request.id,
        request.operation,
        request.cwd.display(),
        request.detail
    );
    eprint!("Allow operation? [y/N] ");
    std::io::stderr().flush()?;
    Ok(())
}

fn show_next(pending: &VecDeque<(Request, UnixStream)>) -> Result<()> {
    if let Some((request, _)) = pending.front() {
        show_request(request)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn test_session(root: &Path) -> Session {
        let root = config::canonical_directory(root).unwrap();
        Session {
            id: "service-account-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        }
    }

    #[tokio::test]
    async fn service_account_status_without_token_is_non_secret_and_inactive() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let result = handle_service_account_request(&session, None, ServiceAccountRequest::Status)
            .await
            .unwrap();
        assert_eq!(result["configured"], false);
        assert_eq!(result["authenticated"], false);
    }

    #[tokio::test]
    async fn service_account_run_rejects_plaintext_environment_values_before_op_exec() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let error = service_account_run(
            &session,
            "fake-token-that-must-never-run",
            session.cwd.clone(),
            vec!["true".to_owned()],
            Vec::new(),
            BTreeMap::from([("API_TOKEN".to_owned(), "plaintext-secret".to_owned())]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("op:// secret references"));
    }

    #[tokio::test]
    async fn service_account_run_rejects_reserved_op_environment_names() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let error = service_account_run(
            &session,
            "fake-token-that-must-never-run",
            session.cwd.clone(),
            vec!["true".to_owned()],
            Vec::new(),
            BTreeMap::from([(
                "OP_ACCOUNT".to_owned(),
                "op://vault/item/account".to_owned(),
            )]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("reserved for 1Password CLI"));
    }

    #[test]
    fn service_account_helpers_validate_names_and_redact_tokens() {
        assert!(valid_environment_name("API_TOKEN"));
        assert!(valid_environment_name("_TOKEN2"));
        assert!(!valid_environment_name("2TOKEN"));
        assert!(!valid_environment_name("BAD-NAME"));
        assert_eq!(
            redact_token("before secret-token after", "secret-token"),
            "before [REDACTED_SERVICE_ACCOUNT_TOKEN] after"
        );
    }
}
