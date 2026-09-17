//! Local-agent `git` shim and the parent-side Git broker.
//!
//! The shim is the temote-mcp binary itself, exposed to a local agent as a
//! private `bin/git` symlink. It forwards one bounded JSON request through a
//! private request/response directory under the agent's own state root, and a
//! broker runs for the duration of `local_agent::run`. The broker accepts only
//! the bounded `switch`, `add`, and `commit` forms implemented here and
//! validates every path, message, and ref again on the parent side; everything
//! else fails closed with a fixed message.
//!
//! A directory transport is used instead of a Unix-domain socket on purpose:
//! the Linux local-agent seccomp profile denies `socket(AF_UNIX, ...)` so a
//! socket would be unreachable, and macOS `sun_path` is too short for the
//! private state root. File operations are already part of the agent profile.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::{config, mcp, sandbox};

pub(crate) const BROKER_ENVIRONMENT_VARIABLE: &str = "TEMOTE_MCP_GIT_BROKER_DIR";
pub(crate) const SHIM_EXIT_REJECTED: i32 = 128;
pub(crate) const SHIM_REJECTION_MESSAGE: &str = "git shim supports only: switch <existing-branch>, switch -c <new-branch>, add <path>..., commit -m <message>";
const BROKER_SCHEMA: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = sandbox::MAX_COMMAND_OUTPUT_BYTES * 8;
const REQUESTS_DIRECTORY: &str = "requests";
const RESPONSES_DIRECTORY: &str = "responses";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GitShimRequest {
    schema: u32,
    cwd: PathBuf,
    argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShimCommand {
    SwitchExisting,
    SwitchCreate,
    Add,
    Commit,
}

/// Classifies the raw shim argv.
///
/// Only `switch <branch>`, `switch -c|--create <branch>`, `add <path>...`, and
/// `commit -m|--message <message>` are accepted. Any global option, extra
/// argument, option-like path or branch, unsupported option, or other
/// subcommand is rejected here, before any Git process runs. Filesystem
/// containment for `add` paths is re-checked by the broker against the
/// prepared session roots.
fn classify_argv(argv: &[String]) -> Result<ShimCommand> {
    match argv {
        [command, branch] if command.as_str() == "switch" => {
            validate_shim_branch(branch)?;
            Ok(ShimCommand::SwitchExisting)
        }
        [command, option, branch]
            if command.as_str() == "switch" && matches!(option.as_str(), "-c" | "--create") =>
        {
            validate_shim_branch(branch)?;
            Ok(ShimCommand::SwitchCreate)
        }
        [command, paths @ ..] if command.as_str() == "add" => {
            validate_shim_add_paths(paths)?;
            Ok(ShimCommand::Add)
        }
        [command, option, message]
            if command.as_str() == "commit" && matches!(option.as_str(), "-m" | "--message") =>
        {
            mcp::validate_git_commit_message(message)?;
            Ok(ShimCommand::Commit)
        }
        _ => anyhow::bail!("unsupported Git shim command"),
    }
}

fn validate_shim_branch(branch: &str) -> Result<()> {
    anyhow::ensure!(!branch.is_empty(), "branch must not be empty");
    anyhow::ensure!(!branch.starts_with('-'), "branch must not start with '-'");
    Ok(())
}

fn validate_shim_add_paths(paths: &[String]) -> Result<()> {
    anyhow::ensure!(!paths.is_empty(), "Git shim add requires at least one path");
    anyhow::ensure!(
        paths.len() <= mcp::MAX_GIT_ADD_PATHS,
        "Git shim add supports at most {} paths",
        mcp::MAX_GIT_ADD_PATHS
    );
    for path in paths {
        validate_shim_add_path(path)?;
    }
    Ok(())
}

fn validate_shim_add_path(path: &str) -> Result<()> {
    mcp::validate_git_path_syntax(path)?;
    let candidate = Path::new(path);
    anyhow::ensure!(
        candidate.is_relative(),
        "Git shim add path must be relative: {path:?}"
    );
    let mut components = candidate.components().peekable();
    anyhow::ensure!(
        components.peek().is_some(),
        "Git shim add path must not be empty"
    );
    for component in components {
        anyhow::ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "Git shim add path must not contain '.' or '..' components: {path:?}"
        );
    }
    Ok(())
}

/// Re-checks one classified `add` path on the parent side. The path must
/// resolve inside the prepared session roots; the returned string is the
/// absolute path handed to Git, mirroring the structured `git_add` command
/// shape.
fn resolve_shim_add_path(roots: &[PathBuf], cwd: &Path, path: &str) -> Result<String> {
    let candidate = cwd.join(path);
    let resolved = if candidate.exists() || std::fs::symlink_metadata(&candidate).is_ok() {
        std::fs::canonicalize(&candidate)
            .with_context(|| format!("cannot resolve Git shim add path {}", candidate.display()))?
    } else {
        let parent = candidate
            .parent()
            .context("Git shim add path has no parent directory")?;
        std::fs::canonicalize(parent)
            .with_context(|| format!("cannot resolve Git shim add path {}", candidate.display()))?
    };
    anyhow::ensure!(
        roots
            .iter()
            .any(|root| resolved == *root || resolved.starts_with(root)),
        "Git shim add path is outside the permitted session roots: {}",
        candidate.display()
    );
    Ok(candidate.to_string_lossy().into_owned())
}

fn validate_request_cwd(roots: &[PathBuf], cwd: &Path) -> Result<()> {
    anyhow::ensure!(!roots.is_empty(), "Git broker has no permitted roots");
    anyhow::ensure!(cwd.is_absolute(), "Git broker cwd must be absolute");
    let canonical = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve Git broker cwd {}", cwd.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "Git broker cwd is not a directory: {}",
        canonical.display()
    );
    anyhow::ensure!(
        roots
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root)),
        "Git broker cwd is outside the permitted session roots: {}",
        canonical.display()
    );
    Ok(())
}

async fn handle_request(
    session: &config::Session,
    roots: &[PathBuf],
    request: GitShimRequest,
) -> Result<sandbox::Output> {
    anyhow::ensure!(
        request.schema == BROKER_SCHEMA,
        "unsupported Git broker schema"
    );
    validate_request_cwd(roots, &request.cwd)?;
    match classify_argv(&request.argv)? {
        ShimCommand::SwitchExisting => {
            mcp::validate_git_branch_name(session, &request.cwd, &request.argv[1]).await?;
            mcp::ensure_local_branch_exists(session, &request.cwd, &request.argv[1]).await?;
            let command = mcp::build_git_switch_command(&request.argv[1]);
            run_git_command(session, &request.cwd, command).await
        }
        ShimCommand::SwitchCreate => {
            mcp::validate_git_branch_name(session, &request.cwd, &request.argv[2]).await?;
            mcp::ensure_local_branch_absent(session, &request.cwd, &request.argv[2]).await?;
            let base = mcp::resolve_git_base_commit(session, &request.cwd, "HEAD").await?;
            let create = mcp::build_git_branch_create_command(&request.argv[2], &base);
            let created = run_git_command(session, &request.cwd, create).await?;
            if created.status != 0 {
                return Ok(created);
            }
            let switch = mcp::build_git_switch_command(&request.argv[2]);
            run_git_command(session, &request.cwd, switch).await
        }
        ShimCommand::Add => {
            let mut command = vec![
                "git".to_owned(),
                "-c".to_owned(),
                "core.hooksPath=/dev/null".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
            ];
            for path in &request.argv[1..] {
                command.push(resolve_shim_add_path(roots, &request.cwd, path)?);
            }
            run_git_command(session, &request.cwd, command).await
        }
        ShimCommand::Commit => {
            mcp::ensure_staged_paths_are_permitted(session, &request.cwd).await?;
            let command = mcp::build_git_commit_command(&request.argv[2]);
            run_git_command(session, &request.cwd, command).await
        }
    }
}

async fn run_git_command(
    session: &config::Session,
    cwd: &Path,
    command: Vec<String>,
) -> Result<sandbox::Output> {
    if session.yolo() {
        sandbox::run_unrestricted(&command, cwd, None).await
    } else {
        let git_roots = sandbox::git_metadata_roots(cwd)?;
        sandbox::run_git(
            &command,
            cwd,
            &session.permitted_directories,
            &git_roots,
            None,
        )
        .await
    }
}

struct BrokerState {
    session: config::Session,
    roots: Vec<PathBuf>,
}

/// A running Git broker queue. Dropping it aborts the serve loop and removes
/// the private request/response directory.
pub(crate) struct GitBroker {
    directory: PathBuf,
    task: JoinHandle<()>,
}

impl GitBroker {
    pub(crate) fn start(
        directory: PathBuf,
        session: config::Session,
        roots: Vec<PathBuf>,
    ) -> Result<Self> {
        let requests = directory.join(REQUESTS_DIRECTORY);
        let responses = directory.join(RESPONSES_DIRECTORY);
        std::fs::create_dir_all(&requests).with_context(|| {
            format!(
                "cannot create the Git broker request queue {}",
                requests.display()
            )
        })?;
        std::fs::create_dir_all(&responses).with_context(|| {
            format!(
                "cannot create the Git broker response queue {}",
                responses.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&directory, &requests, &responses] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("cannot protect {}", path.display()))?;
            }
        }
        let state = Arc::new(BrokerState { session, roots });
        let task = tokio::spawn(serve(directory.clone(), state));
        Ok(Self { directory, task })
    }
}

impl Drop for GitBroker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn serve(directory: PathBuf, state: Arc<BrokerState>) {
    let requests = directory.join(REQUESTS_DIRECTORY);
    let responses = directory.join(RESPONSES_DIRECTORY);
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        let Ok(entries) = std::fs::read_dir(&requests) else {
            continue;
        };
        let mut names = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name.ends_with(".json"))
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            let id = name.trim_end_matches(".json").to_owned();
            let request_path = requests.join(&name);
            let response_path = responses.join(format!("{id}.json"));
            let payload = if !valid_request_id(&id) {
                error_payload()
            } else {
                match std::fs::read(&request_path) {
                    Ok(bytes) if bytes.len() <= MAX_REQUEST_BYTES => {
                        match serde_json::from_slice::<GitShimRequest>(&bytes) {
                            Ok(request) => {
                                match handle_request(&state.session, &state.roots, request).await {
                                    Ok(output) => success_payload(&output),
                                    Err(_) => error_payload(),
                                }
                            }
                            Err(_) => error_payload(),
                        }
                    }
                    _ => error_payload(),
                }
            };
            if payload.len() <= MAX_RESPONSE_BYTES {
                let _ = std::fs::write(&response_path, payload);
            }
            let _ = std::fs::remove_file(&request_path);
        }
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn success_payload(output: &sandbox::Output) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": BROKER_SCHEMA,
        "status": output.status,
        "stdout": output.stdout,
        "stderr": output.stderr,
    }))
    .unwrap_or_else(|_| error_payload())
}

fn error_payload() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": BROKER_SCHEMA,
        "error": SHIM_REJECTION_MESSAGE,
    }))
    .unwrap_or_default()
}

/// Returns the shim exit status when the process was invoked as `git` with a
/// broker directory configured, and `None` for every normal CLI invocation.
pub(crate) fn maybe_run_as_git_shim() -> Option<i32> {
    let mut args = std::env::args();
    let argv0 = args.next()?;
    let basename = Path::new(&argv0).file_name()?.to_str()?;
    if basename != "git" || std::env::var_os(BROKER_ENVIRONMENT_VARIABLE).is_none() {
        return None;
    }
    Some(run_shim(args.collect()))
}

pub(crate) fn run_shim(argv: Vec<String>) -> i32 {
    let Some(directory) = std::env::var_os(BROKER_ENVIRONMENT_VARIABLE).map(PathBuf::from) else {
        return reject();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return reject();
    };
    match request_broker(&directory, &cwd, &argv) {
        Ok(result) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(result.stdout.as_bytes());
            let _ = stdout.flush();
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(result.stderr.as_bytes());
            let _ = stderr.flush();
            result.status
        }
        Err(_) => reject(),
    }
}

fn reject() -> i32 {
    eprintln!("{SHIM_REJECTION_MESSAGE}");
    SHIM_EXIT_REJECTED
}

struct ShimResult {
    status: i32,
    stdout: String,
    stderr: String,
}

fn request_broker(directory: &Path, cwd: &Path, argv: &[String]) -> Result<ShimResult> {
    anyhow::ensure!(
        directory.is_absolute(),
        "Git broker directory must be an absolute path"
    );
    anyhow::ensure!(cwd.is_absolute(), "Git shim cwd must be absolute");
    let request = GitShimRequest {
        schema: BROKER_SCHEMA,
        cwd: cwd.to_owned(),
        argv: argv.to_vec(),
    };
    let encoded = serde_json::to_vec(&request).context("cannot encode the Git broker request")?;
    anyhow::ensure!(
        encoded.len() <= MAX_REQUEST_BYTES,
        "Git broker request exceeds the size limit"
    );

    let requests = directory.join(REQUESTS_DIRECTORY);
    let responses = directory.join(RESPONSES_DIRECTORY);
    let id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let staged = requests.join(format!("{id}.tmp"));
    let queued = requests.join(format!("{id}.json"));
    std::fs::write(&staged, &encoded).context("cannot stage the Git broker request")?;
    std::fs::rename(&staged, &queued).context("cannot queue the Git broker request")?;

    let response_path = responses.join(format!("{id}.json"));
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        match std::fs::read(&response_path) {
            Ok(bytes) => {
                let _ = std::fs::remove_file(&response_path);
                let _ = std::fs::remove_file(&queued);
                anyhow::ensure!(
                    bytes.len() <= MAX_RESPONSE_BYTES,
                    "Git broker response exceeds the size limit"
                );
                return decode_response(&bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot read the Git broker response"),
        }
        if Instant::now() >= deadline {
            let _ = std::fs::remove_file(&queued);
            anyhow::bail!("timed out waiting for the Git broker response");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn decode_response(bytes: &[u8]) -> Result<ShimResult> {
    let response: Value = serde_json::from_slice(bytes).context("invalid Git broker response")?;
    anyhow::ensure!(
        response.get("schema").and_then(Value::as_u64) == Some(BROKER_SCHEMA as u64),
        "unexpected Git broker schema"
    );
    anyhow::ensure!(
        response.get("error").is_none(),
        "Git broker rejected the request"
    );
    let status = response
        .get("status")
        .and_then(Value::as_i64)
        .context("missing Git broker status")?;
    let status = i32::try_from(status).context("invalid Git broker status")?;
    let stdout = response
        .get("stdout")
        .and_then(Value::as_str)
        .context("missing Git broker stdout")?;
    let stderr = response
        .get("stderr")
        .and_then(Value::as_str)
        .context("missing Git broker stderr")?;
    Ok(ShimResult {
        status,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn request(cwd: &Path, values: &[&str]) -> GitShimRequest {
        GitShimRequest {
            schema: BROKER_SCHEMA,
            cwd: cwd.to_owned(),
            argv: argv(values),
        }
    }

    fn session(repository: &Path) -> config::Session {
        config::Session {
            id: "git-broker-test".to_owned(),
            cwd: repository.to_owned(),
            permitted_directories: vec![repository.to_owned()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        }
    }

    fn run_host_git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Temote Test",
                "-c",
                "user.email=temote-test@example.invalid",
            ])
            .args(args)
            .current_dir(repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("host git must be installed for this test");
        assert!(
            output.status.success(),
            "host git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn classifier_accepts_only_the_bounded_switch_add_and_commit_forms() {
        assert_eq!(
            classify_argv(&argv(&["switch", "main"])).unwrap(),
            ShimCommand::SwitchExisting
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "feature/x"])).unwrap(),
            ShimCommand::SwitchExisting
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "-c", "feature/x"])).unwrap(),
            ShimCommand::SwitchCreate
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "--create", "feature/x"])).unwrap(),
            ShimCommand::SwitchCreate
        );
        assert_eq!(
            classify_argv(&argv(&["add", "tracked.txt"])).unwrap(),
            ShimCommand::Add
        );
        assert_eq!(
            classify_argv(&argv(&["add", "src/main.rs", "docs/usage.md"])).unwrap(),
            ShimCommand::Add
        );
        assert_eq!(
            classify_argv(&argv(&["commit", "-m", "message"])).unwrap(),
            ShimCommand::Commit
        );
        assert_eq!(
            classify_argv(&argv(&["commit", "--message", "message"])).unwrap(),
            ShimCommand::Commit
        );
    }

    #[test]
    fn classifier_rejects_unsupported_options_config_and_subcommands() {
        for values in [
            vec![],
            vec!["switch"],
            vec!["switch", "-f"],
            vec!["switch", "--force"],
            vec!["switch", "-c"],
            vec!["switch", "-c", "extra", "branch"],
            vec!["switch", "-c", "-x"],
            vec!["switch", "-x"],
            vec!["switch", "--"],
            vec!["switch", "-"],
            vec!["switch", "--", "main"],
            vec!["switch", "main", "extra"],
            vec!["branch"],
            vec!["checkout", "main"],
            vec!["worktree", "add", "feature"],
            vec!["-c", "core.hooksPath=/tmp"],
            vec!["--config-env", "x=y", "true"],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn classifier_rejects_unbounded_add_and_commit_forms() {
        let overly_long_message = "x".repeat(mcp::MAX_GIT_COMMIT_MESSAGE_BYTES + 1);
        for values in [
            vec!["add"],
            vec!["add", "-A"],
            vec!["add", "--all"],
            vec!["add", "."],
            vec!["add", ".."],
            vec!["add", "/abs/path"],
            vec!["add", "-p"],
            vec!["add", "--", "path"],
            vec!["add", "../outside.txt"],
            vec!["add", "dir/../../outside.txt"],
            vec!["add", "tracked.txt", "/abs/other.txt"],
            vec!["add", "-tracked.txt"],
            vec!["add", "tracked*txt"],
            vec!["add", ":tracked.txt"],
            vec!["commit"],
            vec!["commit", "-m"],
            vec!["commit", "--message"],
            vec!["commit", "-a", "-m", "x"],
            vec!["commit", "--amend", "-m", "x"],
            vec!["commit", "-m", "x", "-m", "y"],
            vec!["commit", "--no-verify", "-m", "x"],
            vec!["commit", "-S", "-m", "x"],
            vec!["commit", "-m", "x", "--", "path"],
            vec!["commit", "-m", ""],
            vec!["commit", "--message", "   "],
            vec!["commit", "-m", overly_long_message.as_str()],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
    }

    #[tokio::test]
    async fn broker_rejects_cwd_outside_the_prepared_session_roots() {
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(repository.path()).unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        let error = handle_request(
            &session(&repository),
            std::slice::from_ref(&repository),
            request(&outside, &["switch", "main"]),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the permitted session roots")
        );
    }

    #[tokio::test]
    async fn broker_rejects_unsupported_argv_before_touching_a_repository() {
        let repository = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(repository.path()).unwrap();
        let error = handle_request(
            &session(&repository),
            std::slice::from_ref(&repository),
            request(&repository, &["checkout", "main"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("unsupported Git shim command"));
    }

    #[tokio::test]
    async fn broker_adds_and_commits_only_the_named_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        run_host_git(&repository, &["init", "--quiet"]);
        run_host_git(&repository, &["config", "user.name", "Temote Test"]);
        run_host_git(
            &repository,
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        std::fs::write(repository.join("other.txt"), "base\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt", "other.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "initial"]);
        run_host_git(&repository, &["branch", "-M", "main"]);

        std::fs::write(repository.join("tracked.txt"), "updated\n").unwrap();
        std::fs::write(repository.join("other.txt"), "unrelated\n").unwrap();
        std::fs::write(repository.join("untracked.txt"), "new\n").unwrap();

        let roots = std::slice::from_ref(&repository);
        let session = session(&repository);

        let added = handle_request(
            &session,
            roots,
            request(&repository, &["add", "tracked.txt"]),
        )
        .await
        .unwrap();
        assert_eq!(added.status, 0, "{}", added.stderr);

        let committed = handle_request(
            &session,
            roots,
            request(&repository, &["commit", "-m", "update tracked only"]),
        )
        .await
        .unwrap();
        assert_eq!(committed.status, 0, "{}", committed.stderr);

        assert_eq!(
            run_host_git(
                &repository,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
            ),
            "tracked.txt"
        );
        assert_eq!(
            run_host_git(&repository, &["show", "HEAD:tracked.txt"]),
            "updated"
        );

        let status = run_host_git(&repository, &["status", "--porcelain"]);
        let status_lines = status
            .lines()
            .map(|line| line.trim_start().to_owned())
            .collect::<Vec<_>>();
        assert!(
            status_lines.iter().any(|line| line == "M other.txt"),
            "{status_lines:?}"
        );
        assert!(
            status_lines.iter().any(|line| line == "?? untracked.txt"),
            "{status_lines:?}"
        );
        assert!(
            !status_lines
                .iter()
                .any(|line| line.ends_with(" tracked.txt")),
            "{status_lines:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("other.txt")).unwrap(),
            "unrelated\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("untracked.txt")).unwrap(),
            "new\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn broker_rejects_add_paths_that_resolve_outside_the_prepared_roots() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        run_host_git(&repository, &["init", "--quiet"]);
        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), repository.join("link.txt"))
            .unwrap();

        let error = handle_request(
            &session(&repository),
            std::slice::from_ref(&repository),
            request(&repository, &["add", "link.txt"]),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the permitted session roots"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn broker_creates_switches_and_preserves_a_dirty_worktree() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        run_host_git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "initial"]);
        run_host_git(&repository, &["branch", "-M", "main"]);

        let roots = std::slice::from_ref(&repository);
        let session = session(&repository);

        let created = handle_request(
            &session,
            roots,
            request(&repository, &["switch", "-c", "feature/x"]),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "feature/x"
        );

        let switched = handle_request(&session, roots, request(&repository, &["switch", "main"]))
            .await
            .unwrap();
        assert_eq!(switched.status, 0, "{}", switched.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "main"
        );

        run_host_git(
            &repository,
            &["switch", "--quiet", "-c", "feature/conflict"],
        );
        std::fs::write(repository.join("tracked.txt"), "feature\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "feature"]);
        let switched = handle_request(&session, roots, request(&repository, &["switch", "main"]))
            .await
            .unwrap();
        assert_eq!(switched.status, 0, "{}", switched.stderr);

        std::fs::write(repository.join("tracked.txt"), "dirty-main\n").unwrap();
        let conflicted = handle_request(
            &session,
            roots,
            request(&repository, &["switch", "feature/conflict"]),
        )
        .await
        .unwrap();
        assert_ne!(conflicted.status, 0);
        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt")).unwrap(),
            "dirty-main\n"
        );
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "main"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shim_and_broker_round_trip_over_the_private_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        run_host_git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "initial"]);
        run_host_git(&repository, &["branch", "-M", "main"]);

        let broker_root = tempfile::tempdir().unwrap();
        let broker_directory = std::fs::canonicalize(broker_root.path()).unwrap();
        let _broker = GitBroker::start(
            broker_directory.clone(),
            session(&repository),
            vec![repository.clone()],
        )
        .unwrap();

        let request_directory = broker_directory.clone();
        let request_repository = repository.clone();
        let result = tokio::task::spawn_blocking(move || {
            request_broker(
                &request_directory,
                &request_repository,
                &argv(&["switch", "-c", "feature/round-trip"]),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.status, 0, "{}", result.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "feature/round-trip"
        );

        let request_directory = broker_directory.clone();
        let request_repository = repository.clone();
        let rejected = tokio::task::spawn_blocking(move || {
            request_broker(
                &request_directory,
                &request_repository,
                &argv(&["checkout", "main"]),
            )
        })
        .await
        .unwrap();
        assert!(rejected.is_err());
    }
}
