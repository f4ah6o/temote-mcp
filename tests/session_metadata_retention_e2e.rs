use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_NAMESPACE: AtomicUsize = AtomicUsize::new(1);

fn unique_namespace() -> String {
    let serial = NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed);
    format!("r{:x}{:x}", std::process::id() & 0xffff, serial)
}

fn state_root(state_home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        state_home
            .join("Library")
            .join("Application Support")
            .join("temote-mcp")
    }
    #[cfg(not(target_os = "macos"))]
    {
        state_home.join("temote-mcp")
    }
}

fn sessions_dir(state_home: &Path) -> PathBuf {
    state_root(state_home).join("sessions")
}

fn upgrade_dir(state_home: &Path) -> PathBuf {
    state_root(state_home).join("upgrade")
}

fn socket_dir(namespace: &str) -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp").join(format!("tmcp-{uid}-{namespace}"))
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.spawn().expect("failed to spawn child process"),
        }
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
    fn spawn(binary: &Path, state_home: &Path, namespace: &str) -> Self {
        let mut command = Command::new(binary);
        command
            .arg("mcp")
            .env("XDG_STATE_HOME", state_home)
            .env("HOME", state_home)
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", namespace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = ChildGuard::spawn(&mut command);
        let stdin = process.child.stdin.take().expect("MCP stdin was not piped");
        let stdout = process
            .child
            .stdout
            .take()
            .expect("MCP stdout was not piped");
        let mut client = Self {
            process,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        let response = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "metadata-retention-e2e", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "temote-mcp");
        client
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
        assert_ne!(bytes, 0, "MCP process exited before responding");
        let response: Value = serde_json::from_str(line.trim()).expect("invalid MCP response JSON");
        assert_eq!(response["id"], id, "MCP response ID mismatch");
        response
    }

    fn session_list(&mut self) -> Vec<Value> {
        let response = self.request(
            "tools/call",
            json!({"name": "session_list", "arguments": {}}),
        );
        assert!(
            response.get("error").is_none(),
            "session_list failed: {response}"
        );
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("session_list response had no text content");
        serde_json::from_str::<Value>(text)
            .expect("session_list text was not JSON")
            .as_array()
            .expect("session_list was not an array")
            .clone()
    }

    fn shutdown(&mut self) {
        self.stdin.take();
        let status = self.process.wait_for_exit(SHUTDOWN_TIMEOUT);
        assert!(status.success(), "MCP process exited with {status}");
    }
}

struct Fixture {
    binary: PathBuf,
    project: TempDir,
    state: TempDir,
    namespace: String,
}

impl Fixture {
    fn new() -> Self {
        let project = TempDir::new().expect("failed to create project fixture");
        let state = TempDir::new().expect("failed to create state fixture");
        fs::write(project.path().join("marker.txt"), "fixture\n").unwrap();
        Self {
            binary: PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp")),
            project,
            state,
            namespace: unique_namespace(),
        }
    }

    fn roots_env(&self) -> String {
        serde_json::to_string(&json!({"src": self.project.path()})).unwrap()
    }

    fn spawn_supervisor(&self) -> ChildGuard {
        let mut command = Command::new(&self.binary);
        command
            .arg("supervisor")
            .env("XDG_STATE_HOME", self.state.path())
            .env("HOME", self.state.path())
            .env("TEMOTE_MCP_ROOTS", self.roots_env())
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", &self.namespace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::spawn(&mut command)
    }

    fn run_cli(&self, args: &[&str]) -> Output {
        Command::new(&self.binary)
            .args(args)
            .current_dir(self.project.path())
            .env("XDG_STATE_HOME", self.state.path())
            .env("HOME", self.state.path())
            .env("TEMOTE_MCP_ROOTS", self.roots_env())
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", &self.namespace)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run temote-mcp CLI")
    }

    fn wait_for_supervisor(&self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            let output = self.run_cli(&["session", "list"]);
            if output.status.success() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "supervisor did not become ready: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn start_active(&self, id: &str) {
        let output = self.run_cli(&["session", "start", "--path", "src", id]);
        assert!(
            output.status.success(),
            "session start failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn stop_active(&self, id: &str) {
        let output = self.run_cli(&["session", "stop", id]);
        assert!(
            output.status.success(),
            "session stop failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn mcp_client(&self) -> McpClient {
        McpClient::spawn(&self.binary, self.state.path(), &self.namespace)
    }

    fn metadata_dir(&self) -> PathBuf {
        sessions_dir(self.state.path())
    }

    fn canonical_project(&self) -> PathBuf {
        fs::canonicalize(self.project.path()).unwrap()
    }
}

fn write_terminal_pair(directory: &Path, cwd: &Path, id: &str, started_at: u64, stopped_at: u64) {
    fs::create_dir_all(directory).unwrap();
    let metadata = json!({
        "id": id,
        "cwd": cwd,
        "permitted_directories": [cwd],
        "started_at": started_at,
        "process_id": 0,
        "yolo": false,
    });
    let lifecycle = json!({
        "status": "stopped",
        "started_at": started_at,
        "stopped_at": stopped_at,
        "exit_reason": "fixture",
        "last_error": null,
        "logical_path": null,
        "restart_policy": "never"
    });
    fs::write(
        directory.join(format!("{id}.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join(format!("{id}.state")),
        serde_json::to_vec(&lifecycle).unwrap(),
    )
    .unwrap();
}

fn write_live_pair(directory: &Path, cwd: &Path, id: &str, started_at: u64) {
    fs::create_dir_all(directory).unwrap();
    let metadata = json!({
        "id": id,
        "cwd": cwd,
        "permitted_directories": [cwd],
        "started_at": started_at,
        "process_id": 4242,
        "yolo": false,
    });
    let lifecycle = json!({
        "status": "active",
        "started_at": started_at,
        "stopped_at": null,
        "exit_reason": null,
        "last_error": null,
        "logical_path": null,
        "restart_policy": "never"
    });
    fs::write(
        directory.join(format!("{id}.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join(format!("{id}.state")),
        serde_json::to_vec(&lifecycle).unwrap(),
    )
    .unwrap();
}

fn write_restore_plan(state_home: &Path, protected_id: &str, cwd: &Path) -> PathBuf {
    let directory = upgrade_dir(state_home);
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("restore-retention-fixture.json");
    let plan = json!({
        "plan_schema": 1,
        "source_version": "fixture",
        "target_version": "fixture",
        "control_protocol": 1,
        "lifecycle_schema": 1,
        "supervisor_pid": 1,
        "created_at": 1,
        "handoff_required": true,
        "sessions": [{
            "session_id": protected_id,
            "cwd": cwd,
            "permitted_directories": [cwd],
            "yolo": false,
            "logical_path": null,
            "restart_policy": "never",
            "public": false,
            "restart_context_keys": []
        }]
    });
    fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

struct FakeLiveSocket {
    stop: Arc<AtomicBool>,
    path: PathBuf,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeLiveSocket {
    fn start(namespace: &str, id: &str) -> Self {
        let directory = socket_dir(namespace);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{id}.sock"));
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut line = String::new();
                        BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        if line.contains("probe") {
                            stream.write_all(b"active\n").unwrap();
                            stream.flush().unwrap();
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            path,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeLiveSocket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn active_ids(sessions: &[Value]) -> BTreeSet<String> {
    sessions
        .iter()
        .filter(|session| session["status"] == "active")
        .filter_map(|session| session["session_id"].as_str().map(str::to_owned))
        .collect()
}

fn cli_active_ids(output: &Output) -> BTreeSet<String> {
    assert!(
        output.status.success(),
        "session list failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let id = columns.next()?;
            let status = columns.next()?;
            (status == "active").then(|| id.to_owned())
        })
        .collect()
}

#[test]
fn session_list_survives_more_than_scan_limit_of_non_json_metadata() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();
    fixture.start_active("active-non-json");
    let directory = fixture.metadata_dir();
    fs::create_dir_all(&directory).unwrap();
    for index in 0..4200 {
        fs::write(directory.join(format!("noise-{index:05}.tmp")), b"x").unwrap();
    }

    let mut client = fixture.mcp_client();
    let sessions = client.session_list();
    assert!(active_ids(&sessions).contains("active-non-json"));

    fixture.stop_active("active-non-json");
    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
    client.shutdown();
}

#[test]
fn session_list_prioritizes_active_sessions_over_large_history() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();
    let expected = ["active-a", "active-b", "active-c"];
    for id in expected {
        fixture.start_active(id);
    }
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    for index in 0..4200_u64 {
        write_terminal_pair(
            &directory,
            &cwd,
            &format!("history-{index:05}"),
            index + 1,
            index + 10,
        );
    }

    let mut client = fixture.mcp_client();
    let sessions = client.session_list();
    let active = active_ids(&sessions);
    assert_eq!(active, expected.into_iter().map(str::to_owned).collect());

    for id in expected {
        fixture.stop_active(id);
    }
    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
    client.shutdown();
}

#[test]
fn session_list_skips_invalid_legacy_metadata_without_failing_active_discovery() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();
    fixture.start_active("active-valid");
    let directory = fixture.metadata_dir();
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("malformed.json"), b"{").unwrap();
    fs::write(directory.join("bad id.json"), b"{}").unwrap();
    fs::write(
        directory.join("legacy.json"),
        br#"{"id":"legacy","cwd":"/missing","unexpected":true}"#,
    )
    .unwrap();

    let mut client = fixture.mcp_client();
    let sessions = client.session_list();
    assert!(active_ids(&sessions).contains("active-valid"));

    fixture.stop_active("active-valid");
    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
    client.shutdown();
}

#[test]
fn session_list_remains_bounded_and_deterministic() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();
    fixture.start_active("active-a");
    fixture.start_active("active-b");
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    for index in 0..600_u64 {
        write_terminal_pair(
            &directory,
            &cwd,
            &format!("bounded-{index:05}"),
            index + 1,
            10_000 + index,
        );
    }

    let mut client = fixture.mcp_client();
    let first = client.session_list();
    let second = client.session_list();
    assert!(first.len() <= 256);
    assert!(serde_json::to_vec(&first).unwrap().len() <= 4 * 1024 * 1024);
    let first_ids = first
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let second_ids = second
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(first_ids, second_ids);
    assert_eq!(
        &first_ids[..2],
        &["active-a".to_owned(), "active-b".to_owned()]
    );

    fixture.stop_active("active-a");
    fixture.stop_active("active-b");
    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
    client.shutdown();
}

#[test]
fn metadata_retention_never_removes_active_sessions() {
    let fixture = Fixture::new();
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    for index in 0..520_u64 {
        write_terminal_pair(
            &directory,
            &cwd,
            &format!("terminal-{index:05}"),
            index + 100,
            index + 100,
        );
    }
    write_live_pair(&directory, &cwd, "live-protected", 1);
    write_terminal_pair(&directory, &cwd, "upgrade-protected", 1, 1);
    write_restore_plan(fixture.state.path(), "upgrade-protected", &cwd);
    let _live_socket = FakeLiveSocket::start(&fixture.namespace, "live-protected");

    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();

    for id in ["live-protected", "upgrade-protected"] {
        assert!(directory.join(format!("{id}.json")).exists());
        assert!(directory.join(format!("{id}.state")).exists());
    }

    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
}

#[test]
fn metadata_retention_prunes_only_confirmed_terminal_pairs() {
    let fixture = Fixture::new();
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    for index in 0..520_u64 {
        write_terminal_pair(
            &directory,
            &cwd,
            &format!("prune-{index:05}"),
            index + 1,
            index + 1,
        );
    }
    fs::write(directory.join("malformed.json"), b"{").unwrap();
    fs::write(directory.join("malformed.state"), b"{").unwrap();
    fs::write(directory.join("orphan.json"), b"{").unwrap();

    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();

    assert!(!directory.join("prune-00000.json").exists());
    assert!(!directory.join("prune-00000.state").exists());
    assert!(directory.join("prune-00519.json").exists());
    assert!(directory.join("prune-00519.state").exists());
    assert!(directory.join("malformed.json").exists());
    assert!(directory.join("malformed.state").exists());
    assert!(directory.join("orphan.json").exists());

    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
}

#[test]
fn metadata_retention_bounds_repeated_ephemeral_sessions() {
    let fixture = Fixture::new();
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    for index in 0..700_u64 {
        write_terminal_pair(
            &directory,
            &cwd,
            &format!("ephemeral-{index:05}"),
            index + 1,
            index + 1,
        );
    }

    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();

    let mut json_count = 0usize;
    let mut state_count = 0usize;
    for entry in fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|value| value.to_str()) {
            Some("json") => json_count += 1,
            Some("state") => state_count += 1,
            _ => {}
        }
    }
    assert_eq!(json_count, 512);
    assert_eq!(state_count, 512);

    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
}

#[test]
fn mcp_session_list_uses_read_only_fallback_when_supervisor_unavailable() {
    let fixture = Fixture::new();
    let directory = fixture.metadata_dir();
    let cwd = fixture.canonical_project();
    write_terminal_pair(&directory, &cwd, "fallback-stopped", 1, 2);
    write_live_pair(&directory, &cwd, "fallback-unconfirmed", 3);
    let lifecycle_before = fs::read(directory.join("fallback-unconfirmed.state")).unwrap();

    let mut client = fixture.mcp_client();
    let sessions = client.session_list();
    let stopped = sessions
        .iter()
        .find(|session| session["session_id"] == "fallback-stopped")
        .expect("fallback stopped session missing");
    assert_eq!(stopped["status"], "stopped");
    let unconfirmed = sessions
        .iter()
        .find(|session| session["session_id"] == "fallback-unconfirmed")
        .expect("fallback unconfirmed session missing");
    assert_eq!(unconfirmed["status"], "unknown");
    assert_eq!(
        fs::read(directory.join("fallback-unconfirmed.state")).unwrap(),
        lifecycle_before,
        "filesystem fallback must not mutate lifecycle metadata"
    );
    client.shutdown();
}

#[test]
fn cli_mcp_active_session_parity() {
    let fixture = Fixture::new();
    let mut supervisor = fixture.spawn_supervisor();
    fixture.wait_for_supervisor();
    let expected = ["parity-a", "parity-b", "parity-c"];
    for id in expected {
        fixture.start_active(id);
    }

    let cli = cli_active_ids(&fixture.run_cli(&["session", "list"]));
    let mut client = fixture.mcp_client();
    let mcp = active_ids(&client.session_list());
    assert_eq!(cli, mcp);
    assert_eq!(mcp, expected.into_iter().map(str::to_owned).collect());

    for id in expected {
        fixture.stop_active(id);
    }
    supervisor.interrupt();
    assert!(supervisor.wait_for_exit(SHUTDOWN_TIMEOUT).success());
    client.shutdown();
}
