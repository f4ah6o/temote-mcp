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

fn socket_namespace(state_home: &Path) -> String {
    use std::hash::{Hash, Hasher};

    let mut hash = std::collections::hash_map::DefaultHasher::new();
    state_home.hash(&mut hash);
    format!("e{:011x}", hash.finish() & 0x7ff_ffff_ffff)
}

fn isolate_process<'a>(command: &'a mut Command, state_home: &Path) -> &'a mut Command {
    // Resolve platform path aliases (for example macOS /var -> /private/var)
    // so the spawned process observes the canonical state root that Temote's
    // swapped-path checks require.
    let state_home =
        fs::canonicalize(state_home).expect("failed to canonicalize isolated process state home");
    let private_directories = [
        state_home.join("cache"),
        state_home.join("codex"),
        state_home.join("config"),
        state_home.join("runtime"),
        state_home.join("tmp"),
        state_home.join("xdg-runtime"),
    ];
    for directory in &private_directories {
        fs::create_dir_all(directory).expect("failed to create isolated process directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .expect("failed to protect isolated process directory");
        }
    }
    let path = std::env::var_os("PATH").expect("PATH is required for process-boundary tests");
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", &state_home)
        .env("CODEX_HOME", state_home.join("codex"))
        .env("XDG_CACHE_HOME", state_home.join("cache"))
        .env("XDG_CONFIG_HOME", state_home.join("config"))
        .env("XDG_RUNTIME_DIR", state_home.join("xdg-runtime"))
        .env("XDG_STATE_HOME", &state_home)
        .env("TMPDIR", state_home.join("tmp"))
        .env("TEMOTE_MCP_RUNTIME_DIR", state_home.join("runtime"))
}

#[cfg(unix)]
fn private_upgrade_binary() -> (TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().expect("failed to create private executable directory");
    let binary = directory.path().join("temote-mcp");
    fs::copy(env!("CARGO_BIN_EXE_temote-mcp"), &binary)
        .expect("failed to copy upgrade test executable");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
        .expect("failed to protect upgrade test executable");
    let helper = directory.path().join("temote-linux-sandbox");
    fs::copy(env!("CARGO_BIN_EXE_temote-linux-sandbox"), &helper)
        .expect("failed to copy upgrade test sandbox helper");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
        .expect("failed to protect upgrade test sandbox helper");
    (directory, binary)
}

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
        isolate_process(&mut command, state_home)
            .arg("mcp")
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", socket_namespace(state_home))
            .current_dir(state_home)
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

fn initialize_git_repository(project: &Path, state_home: &Path) {
    let mut command = Command::new("git");
    let status = isolate_process(&mut command, state_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
    isolate_process(&mut command, state_home)
        .arg("supervisor")
        .env("TEMOTE_MCP_ROOTS", roots_env(project))
        .env("TEMOTE_MCP_SOCKET_NAMESPACE", socket_namespace(state_home))
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ChildGuard::spawn(&mut command)
}

fn run_cli(binary: &Path, args: &[&str], cwd: &Path, state_home: &Path) -> Output {
    let mut command = Command::new(binary);
    isolate_process(&mut command, state_home)
        .args(args)
        .current_dir(cwd)
        .env("TEMOTE_MCP_SOCKET_NAMESPACE", socket_namespace(state_home))
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

#[cfg(unix)]
fn supervisor_socket_path(state_home: &Path) -> PathBuf {
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp")
        .join(format!("tmcp-{uid}-{}", socket_namespace(state_home)))
        .join("supervisor.sock")
}

#[cfg(unix)]
fn running_supervisor_pid(state_home: &Path) -> u32 {
    use std::os::unix::net::UnixStream;

    let socket = supervisor_socket_path(state_home);
    let mut stream =
        UnixStream::connect(&socket).expect("failed to connect to bootstrapped supervisor");
    writeln!(stream, "{}", json!({"command": "ping"})).expect("failed to write supervisor ping");
    stream.flush().expect("failed to flush supervisor ping");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("failed to finish supervisor ping request");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("failed to read supervisor ping response");
    let response: Value =
        serde_json::from_str(line.trim()).expect("invalid supervisor ping response");
    assert_eq!(response["ok"], true, "supervisor ping failed: {response}");
    response["result"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("supervisor ping did not return a valid pid")
}

#[cfg(unix)]
struct BootstrappedSupervisorGuard {
    pid: u32,
    socket: PathBuf,
}

#[cfg(unix)]
impl Drop for BootstrappedSupervisorGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGINT) };
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while self.socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn upgrade_failure_reports(state_home: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let directory = state_home
        .join("Library")
        .join("Application Support")
        .join("temote-mcp")
        .join("upgrade");
    #[cfg(not(target_os = "macos"))]
    let directory = state_home.join("temote-mcp").join("upgrade");
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(".failure.json"))
        .collect()
}

#[cfg(unix)]
#[test]
#[ignore = "process-boundary upgrade E2E; run explicitly on Linux and macOS"]
fn supervisor_upgrade_rejects_incompatible_generation_before_handoff() {
    let (_binary_directory, binary) = private_upgrade_binary();
    let project = TempDir::new().expect("failed to create E2E project directory");
    let state = TempDir::new().expect("failed to create isolated state directory");
    initialize_git_repository(project.path(), state.path());

    let namespace = socket_namespace(state.path());
    assert!(namespace.len() <= 12, "test socket namespace is too long");
    let uid = unsafe { libc::geteuid() };
    let socket_dir = PathBuf::from("/tmp").join(format!("tmcp-{uid}-{namespace}"));
    fs::create_dir_all(&socket_dir).expect("failed to create fake supervisor socket directory");
    let socket_path = socket_dir.join("supervisor.sock");
    let _ = fs::remove_file(&socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .expect("failed to bind fake old-generation supervisor socket");

    let server = thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .expect("failed to accept upgrade compatibility probe");
        let mut line = String::new();
        BufReader::new(
            stream
                .try_clone()
                .expect("failed to clone fake supervisor stream"),
        )
        .read_line(&mut line)
        .expect("failed to read upgrade compatibility probe");
        let request: Value = serde_json::from_str(line.trim())
            .expect("upgrade compatibility probe was not valid JSON");
        assert_eq!(request["command"], "ping");

        let response = json!({
            "ok": true,
            "result": {
                "status": "active",
                "host_id": "old-generation-host",
                "version": "old-generation",
                "pid": 4242,
                "control_protocol": 999,
                "lifecycle_schema": 1,
                "upgrade_plan_schema": 1,
                "roots_configured": true
            },
            "error": Value::Null
        });
        writeln!(stream, "{response}").expect("failed to write fake supervisor response");
        stream
            .flush()
            .expect("failed to flush fake supervisor response");
        drop(stream);

        listener
            .set_nonblocking(true)
            .expect("failed to make fake supervisor listener nonblocking");
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => panic!("upgrade sent a control request after incompatible Ping"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    panic!("failed while checking for unexpected control request: {error}")
                }
            }
        }
    });

    let mut command = Command::new(&binary);
    let output = isolate_process(&mut command, state.path())
        .args(["upgrade", "--force"])
        .current_dir(project.path())
        .env("TEMOTE_MCP_SOCKET_NAMESPACE", &namespace)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run incompatible-generation upgrade probe");
    assert!(
        !output.status.success(),
        "incompatible generation unexpectedly upgraded: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "running supervisor control protocol is incompatible; manual supervisor restart is required"
        ),
        "upgrade did not report the compatibility gate: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    server
        .join()
        .expect("fake old-generation supervisor thread panicked");
    assert!(
        upgrade_failure_reports(state.path()).is_empty(),
        "compatibility rejection must not create upgrade failure artifacts"
    );

    fs::remove_file(&socket_path).expect("failed to remove fake supervisor socket");
    fs::remove_dir(&socket_dir).expect("failed to remove fake supervisor socket directory");
}

#[test]
#[ignore = "process-boundary upgrade E2E; run explicitly on Linux and macOS"]
fn supervisor_upgrade_handoff_preserves_active_session_and_pid() {
    let (_binary_directory, binary) = private_upgrade_binary();
    let project = TempDir::new().expect("failed to create E2E project directory");
    let state = TempDir::new().expect("failed to create isolated state directory");
    initialize_git_repository(project.path(), state.path());
    let canonical_project =
        fs::canonicalize(project.path()).expect("failed to canonicalize project");
    let session_id = format!("upgrade-e2e-{}", std::process::id());

    let mut supervisor = spawn_supervisor(&binary, project.path(), state.path());
    let supervisor_pid = supervisor.child.id();
    wait_for_supervisor(&binary, project.path(), state.path());

    let start = run_cli(
        &binary,
        &["session", "start", "--path", "src", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&start, "session start before upgrade");
    let policy = run_cli(
        &binary,
        &["session", "restart-policy", &session_id, "on-failure"],
        project.path(),
        state.path(),
    );
    assert_cli_success(&policy, "restart policy before upgrade");

    let upgrade = run_cli(
        &binary,
        &["upgrade", "--force"],
        project.path(),
        state.path(),
    );
    assert_cli_success(&upgrade, "forced same-version supervisor upgrade");
    assert!(
        String::from_utf8_lossy(&upgrade.stdout).contains("Temote upgrade complete:"),
        "upgrade did not report handoff completion: stdout={} stderr={}",
        String::from_utf8_lossy(&upgrade.stdout),
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert_eq!(supervisor.child.id(), supervisor_pid);
    assert!(
        supervisor.child.try_wait().unwrap().is_none(),
        "supervisor exited during same-PID exec handoff"
    );

    let info = run_cli(
        &binary,
        &["session", "info", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&info, "session info after upgrade");
    let info: Value = serde_json::from_slice(&info.stdout).expect("invalid session info JSON");
    assert_eq!(info["status"], "active");
    assert_eq!(info["permission_mode"], "agent");
    assert_eq!(info["restart_policy"], "on-failure");
    assert_eq!(
        PathBuf::from(info["cwd"].as_str().expect("session cwd missing")),
        canonical_project
    );
    assert!(
        upgrade_failure_reports(state.path()).is_empty(),
        "successful supervisor handoff must not create an upgrade failure report"
    );

    let stop = run_cli(
        &binary,
        &["session", "stop", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&stop, "session stop after upgrade");
    supervisor.interrupt();
    let status = supervisor.wait_for_exit(SHUTDOWN_TIMEOUT);
    assert!(status.success(), "supervisor exited with {status}");
}

#[cfg(unix)]
#[test]
#[ignore = "process-boundary E2E; run explicitly on Linux and macOS"]
fn legacy_start_bootstraps_agent_supervisor_without_manual_socket_setup() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let project = TempDir::new().expect("failed to create E2E project directory");
    let state = TempDir::new().expect("failed to create isolated state directory");
    initialize_git_repository(project.path(), state.path());
    let session_id = format!("bootstrap-e2e-{}", std::process::id());
    let socket = supervisor_socket_path(state.path());
    let _ = fs::remove_file(&socket);

    let start = run_cli(
        &binary,
        &["start", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&start, "legacy start with automatic supervisor bootstrap");

    let supervisor = BootstrappedSupervisorGuard {
        pid: running_supervisor_pid(state.path()),
        socket: socket.clone(),
    };
    let info = run_cli(
        &binary,
        &["session", "info", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&info, "session info after automatic supervisor bootstrap");
    let info: Value = serde_json::from_slice(&info.stdout).expect("invalid session info JSON");
    assert_eq!(info["status"], "active");
    assert_eq!(info["permission_mode"], "agent");
    assert_eq!(info["yolo"], false);

    let stop = run_cli(
        &binary,
        &["session", "stop", &session_id],
        project.path(),
        state.path(),
    );
    assert_cli_success(&stop, "session stop after automatic supervisor bootstrap");
    drop(supervisor);
    assert!(
        !socket.exists(),
        "bootstrapped supervisor socket remained after SIGINT"
    );
}

#[test]
#[ignore = "process-boundary E2E; run explicitly in GitHub Actions"]
fn supervisor_session_lifecycle_survives_console_eof_and_records_crash() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let project = TempDir::new().expect("failed to create E2E project directory");
    let state = TempDir::new().expect("failed to create isolated state directory");
    initialize_git_repository(project.path(), state.path());
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
    assert_eq!(info["permission_mode"], "agent");
    assert_eq!(info["restart_policy"], "never");
    assert_eq!(
        PathBuf::from(info["cwd"].as_str().expect("session_info cwd missing")),
        canonical_project
    );

    let jobs = tool_json(&client.tool_call("job_list", json!({"session_id": session_id})));
    assert!(jobs["jobs"].is_array(), "{jobs}");

    let rejected = client.tool_call(
        "stop_job",
        json!({"session_id": session_id, "job_id": "missing-job"}),
    );
    assert!(
        rejected["error"]["message"].as_str().is_some(),
        "unknown job cancellation was not rejected: {rejected}"
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
