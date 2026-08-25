use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        let child = command.spawn().expect("failed to spawn child process");
        Self { child }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("failed to poll child process") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "child process did not exit in time"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    #[cfg(unix)]
    fn interrupt(&mut self) {
        let result = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
        assert_eq!(result, 0, "failed to send SIGINT to child");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct McpClient {
    process: ChildGuard,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(binary: &Path, state_home: &Path) -> Self {
        let mut command = Command::new(binary);
        command
            .arg("mcp")
            .env("XDG_STATE_HOME", state_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = ChildGuard::spawn(&mut command);
        let stdin = process
            .child
            .stdin
            .take()
            .expect("temote-mcp mcp stdin was not piped");
        let stdout = process
            .child
            .stdout
            .take()
            .expect("temote-mcp mcp stdout was not piped");
        Self {
            process,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.stdin.as_mut().expect("MCP client is shut down");
        writeln!(stdin, "{request}").expect("failed to write MCP request");
        stdin.flush().expect("failed to flush MCP request");

        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .expect("failed to read MCP response");
        assert_ne!(bytes, 0, "temote-mcp mcp exited before responding");
        let response: Value = serde_json::from_str(line.trim()).expect("invalid MCP JSON response");
        assert_eq!(response["id"], id, "MCP response ID mismatch: {response}");
        response
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cli-session-e2e", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "temote-mcp");
    }

    fn tool_call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
    }

    fn shutdown(&mut self) {
        self.stdin.take();
        let status = self.process.wait_for_exit(SHUTDOWN_TIMEOUT);
        assert!(status.success(), "temote-mcp mcp exited with {status}");
    }
}

fn tool_text(response: &Value) -> &str {
    assert!(
        response.get("error").is_none(),
        "MCP tool call failed: {response}"
    );
    response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("MCP tool response did not contain text content")
}

fn tool_json(response: &Value) -> Value {
    serde_json::from_str(tool_text(response)).expect("MCP text content was not JSON")
}

fn session_list(client: &mut McpClient) -> Vec<Value> {
    tool_json(&client.tool_call("session_list", json!({})))
        .as_array()
        .expect("session_list did not return an array")
        .clone()
}

fn wait_for_session_status(client: &mut McpClient, session_id: &str, status: &str) -> Value {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(session) = session_list(client)
            .into_iter()
            .find(|session| session["session_id"] == session_id && session["status"] == status)
        {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "session {session_id} did not become {status} in time"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn initialize_git_repository(project: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project)
        .status()
        .expect("failed to run git init for E2E fixture");
    assert!(status.success(), "git init failed with {status}");
    fs::write(project.join("marker.txt"), "process-boundary-e2e\n")
        .expect("failed to create E2E marker file");
}

fn roots_env(project: &Path) -> String {
    serde_json::to_string(&json!({"src": project})).unwrap()
}

fn spawn_supervisor(binary: &Path, project: &Path, state_home: &Path) -> ChildGuard {
    let mut command = Command::new(binary);
    command
        .arg("supervisor")
        .env("XDG_STATE_HOME", state_home)
        .env("TEMOTE_MCP_ROOTS", roots_env(project))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ChildGuard::spawn(&mut command)
}

fn run_cli(binary: &Path, args: &[&str], cwd: &Path, state_home: &Path) -> Output {
    Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .env("XDG_STATE_HOME", state_home)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run temote-mcp CLI")
}

fn assert_cli_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_supervisor(binary: &Path, cwd: &Path, state_home: &Path) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let output = run_cli(binary, &["session", "list"], cwd, state_home);
        if output.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session supervisor did not become ready in time: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "process-boundary E2E; run explicitly in GitHub Actions"]
fn supervisor_session_lifecycle_survives_console_eof_and_records_crash() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let project = TempDir::new().expect("failed to create E2E project directory");
    let state = TempDir::new().expect("failed to create isolated state directory");
    initialize_git_repository(project.path());
    let canonical_project =
        fs::canonicalize(project.path()).expect("failed to canonicalize project");
    let session_id = format!("cli-e2e-{}", std::process::id());
    let legacy_id = format!("legacy-e2e-{}", std::process::id());

    let mut supervisor = spawn_supervisor(&binary, project.path(), state.path());
    wait_for_supervisor(&binary, project.path(), state.path());

    let start = run_cli(
        &binary,
        &["session", "start", "--path", "src", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&start, "session start");

    let legacy = run_cli(
        &binary,
        &["start", &legacy_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&legacy, "legacy start");

    let mut client = McpClient::spawn(&binary, state.path());
    client.initialize();

    let session = wait_for_session_status(&mut client, &session_id, "active");
    assert_eq!(session["yolo"], false);
    assert!(session["pid"].as_u64().is_some());
    assert_eq!(
        PathBuf::from(session["cwd"].as_str().expect("session cwd missing")),
        canonical_project
    );
    wait_for_session_status(&mut client, &legacy_id, "active");

    let console = run_cli(
        &binary,
        &["session", "console"],
        project.path(),
        state.path(),
    );
    assert_cli_success(&console, "session console with stdin EOF");
    wait_for_session_status(&mut client, &session_id, "active");

    let info = tool_json(&client.tool_call("session_info", json!({"session_id": session_id})));
    assert_eq!(info["id"], session_id);
    assert_eq!(info["status"], "active");
    assert_eq!(info["permission_mode"], "ask");
    assert_eq!(info["restart_policy"], "never");
    assert_eq!(
        PathBuf::from(info["cwd"].as_str().expect("session_info cwd missing")),
        canonical_project
    );

    let pwd = tool_json(&client.tool_call(
        "execute",
        json!({
            "session_id": session_id,
            "command": ["pwd"],
        }),
    ));
    assert_eq!(pwd["exit_code"], 0);
    assert_eq!(
        pwd["stdout"].as_str().unwrap().trim(),
        canonical_project.to_string_lossy()
    );

    let git_status = tool_json(&client.tool_call(
        "execute",
        json!({
            "session_id": session_id,
            "command": ["git", "status", "--short"],
        }),
    ));
    assert_eq!(git_status["exit_code"], 0);
    assert!(
        git_status["stdout"]
            .as_str()
            .unwrap()
            .lines()
            .any(|line| line == "?? marker.txt"),
        "git did not observe the fixture through the session: {git_status}"
    );

    let outside_cwd = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is required for the E2E boundary check");
    assert_ne!(fs::canonicalize(&outside_cwd).unwrap(), canonical_project);
    let rejected = client.tool_call(
        "execute",
        json!({
            "session_id": session_id,
            "command": ["pwd"],
            "cwd": outside_cwd,
        }),
    );
    assert!(
        rejected["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("outside the permitted sandbox roots")),
        "outside cwd was not rejected: {rejected}"
    );

    let stop_legacy = run_cli(
        &binary,
        &["session", "stop", &legacy_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&stop_legacy, "legacy session stop");
    wait_for_session_status(&mut client, &legacy_id, "stopped");

    supervisor.kill_and_wait();
    let mut restarted_supervisor = spawn_supervisor(&binary, project.path(), state.path());
    wait_for_supervisor(&binary, project.path(), state.path());

    let crashed = wait_for_session_status(&mut client, &session_id, "crashed");
    assert!(crashed["pid"].is_null());
    assert!(crashed["stopped_at"].as_u64().is_some());
    assert!(
        crashed["last_error"]
            .as_str()
            .is_some_and(|error| error.contains("supervisor stopped")),
        "crash reason was not persisted: {crashed}"
    );

    let restart = run_cli(
        &binary,
        &["session", "restart", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&restart, "session restart");
    wait_for_session_status(&mut client, &session_id, "active");

    let stop = run_cli(
        &binary,
        &["session", "stop", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&stop, "session stop");
    let stopped = wait_for_session_status(&mut client, &session_id, "stopped");
    assert!(stopped["pid"].is_null());
    assert!(stopped["stopped_at"].as_u64().is_some());

    #[cfg(unix)]
    {
        restarted_supervisor.interrupt();
        let status = restarted_supervisor.wait_for_exit(SHUTDOWN_TIMEOUT);
        assert!(status.success(), "supervisor exited with {status}");
    }

    client.shutdown();
}
