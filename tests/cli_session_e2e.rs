use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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
            assert!(Instant::now() < deadline, "child process did not exit in time");
            thread::sleep(Duration::from_millis(50));
        }
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
    fn spawn(binary: &Path) -> Self {
        let mut command = Command::new(binary);
        command
            .arg("mcp")
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

fn wait_for_session(client: &mut McpClient, session_id: &str) -> Value {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(session) = session_list(client)
            .into_iter()
            .find(|session| session["session_id"] == session_id)
        {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "session {session_id} did not become active in time"
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

#[test]
#[ignore = "process-boundary E2E; run explicitly in GitHub Actions"]
fn cli_session_runs_real_cli_through_real_mcp_process() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let project = TempDir::new().expect("failed to create E2E project directory");
    initialize_git_repository(project.path());
    let canonical_project = fs::canonicalize(project.path()).expect("failed to canonicalize project");
    let session_id = format!("cli-e2e-{}", std::process::id());

    let mut start_command = Command::new(&binary);
    start_command
        .args(["start", &session_id])
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut session_process = ChildGuard::spawn(&mut start_command);
    let session_stdin = session_process
        .child
        .stdin
        .take()
        .expect("temote-mcp start stdin was not piped");

    let mut client = McpClient::spawn(&binary);
    client.initialize();

    let session = wait_for_session(&mut client, &session_id);
    assert_eq!(session["status"], "active");
    assert_eq!(session["yolo"], false);
    assert_eq!(
        PathBuf::from(session["cwd"].as_str().expect("session cwd missing")),
        canonical_project
    );

    let info = tool_json(&client.tool_call(
        "session_info",
        json!({"session_id": session_id}),
    ));
    assert_eq!(info["session_id"], session_id);
    assert_eq!(info["yolo"], false);

    let pwd = tool_json(&client.tool_call(
        "execute",
        json!({
            "session_id": session_id,
            "command": ["pwd"],
        }),
    ));
    assert_eq!(pwd["exit_code"], 0);
    assert_eq!(pwd["stdout"].as_str().unwrap().trim(), canonical_project.to_string_lossy());

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

    drop(session_stdin);
    let status = session_process.wait_for_exit(SHUTDOWN_TIMEOUT);
    assert!(status.success(), "temote-mcp start exited with {status}");
    assert!(
        session_list(&mut client)
            .iter()
            .all(|session| session["session_id"] != session_id),
        "session remained active after temote-mcp start shutdown"
    );

    client.shutdown();
}
