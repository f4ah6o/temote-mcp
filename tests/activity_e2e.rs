use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MCP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn set_nonblocking(file: &impl AsRawFd) {
    let descriptor = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    assert!(flags >= 0, "failed to read descriptor flags");
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "failed to make descriptor nonblocking"
    );
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.spawn().expect("failed to spawn Temote process"),
        }
    }

    fn wait(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "Temote process did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn interrupt(&self) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) },
            0
        );
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
    fn spawn(binary: &Path, fixture: &Fixture) -> Self {
        let mut command = fixture.command(binary);
        command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = ChildGuard::spawn(&mut command);
        let stdin = process.child.stdin.take().unwrap();
        let stdout = process.child.stdout.take().unwrap();
        set_nonblocking(&stdout);
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
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut response = String::new();
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            match self.stdout.read_line(&mut response) {
                Ok(0) => panic!("MCP stdout closed before a response"),
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "MCP response timed out");
                    assert!(
                        response.len() <= MAX_MCP_RESPONSE_BYTES,
                        "MCP response exceeded {MAX_MCP_RESPONSE_BYTES} bytes"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to read MCP response: {error}"),
            }
        }
        assert!(
            response.len() <= MAX_MCP_RESPONSE_BYTES,
            "MCP response exceeded {MAX_MCP_RESPONSE_BYTES} bytes"
        );
        let response: Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(response["id"], id);
        response
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "activity-e2e", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["serverInfo"]["name"], "temote-mcp");
    }

    fn tool(&mut self, name: &str, arguments: Value) -> String {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(response.get("error").is_none(), "{response}");
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool result text missing")
            .to_owned()
    }

    fn shutdown(&mut self) {
        self.stdin.take();
        assert!(self.process.wait(PROCESS_TIMEOUT).success());
    }
}

struct Fixture {
    _root: TempDir,
    project: PathBuf,
    home: PathBuf,
    namespace: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let home = root.path().join("home");
        for directory in [
            project.join("one"),
            project.join("two"),
            home.join("codex"),
            home.join("state"),
            home.join("runtime"),
            home.join("temote-runtime"),
            home.join("tmp"),
        ] {
            fs::create_dir_all(directory).unwrap();
        }
        let namespace = format!("s10{:x}", std::process::id());
        assert!(namespace.len() <= 12);
        Self {
            _root: root,
            project,
            home,
            namespace,
        }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .current_dir(&self.project)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join("codex"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("XDG_RUNTIME_DIR", self.home.join("runtime"))
            .env("TEMOTE_MCP_RUNTIME_DIR", self.home.join("temote-runtime"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", &self.namespace);
        command
    }

    fn run(&self, binary: &Path, args: &[&str]) -> Output {
        let mut command = self.command(binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = ChildGuard::spawn(&mut command);
        let mut stdout = process.child.stdout.take().unwrap();
        let mut stderr = process.child.stderr.take().unwrap();
        let stdout_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let status = process.wait(PROCESS_TIMEOUT);
        Output {
            status,
            stdout: stdout_reader.join().unwrap(),
            stderr: stderr_reader.join().unwrap(),
        }
    }

    fn start_supervisor(&self, binary: &Path) -> ChildGuard {
        let roots = serde_json::to_string(&json!({"src": self.project})).unwrap();
        let mut command = self.command(binary);
        command
            .arg("supervisor")
            .env("TEMOTE_MCP_ROOTS", roots)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::spawn(&mut command)
    }

    fn wait_for_supervisor(&self, binary: &Path) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let output = self.run(binary, &["session", "list"]);
            if output.status.success() {
                return;
            }
            assert!(Instant::now() < deadline, "supervisor did not become ready");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn activity(&self, binary: &Path, session_id: Option<&str>) -> Output {
        let mut args = vec!["activity"];
        if let Some(session_id) = session_id {
            args.push(session_id);
        }
        args.extend(["--tail", "100", "--no-follow"]);
        self.run(binary, &args)
    }

    fn wait_for_activity(
        &self,
        binary: &Path,
        session_id: Option<&str>,
        expected: &[&str],
    ) -> String {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let output = self.activity(binary, session_id);
            assert!(
                output.status.success(),
                "activity failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            if expected.iter().all(|part| stdout.contains(part)) {
                return stdout;
            }
            assert!(
                Instant::now() < deadline,
                "activity never contained {expected:?}: {stdout}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_activity_count(
        &self,
        binary: &Path,
        session_id: Option<&str>,
        expected: &str,
        minimum: usize,
    ) -> String {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            let output = self.activity(binary, session_id);
            assert!(
                output.status.success(),
                "activity failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            if stdout.matches(expected).count() >= minimum {
                return stdout;
            }
            assert!(
                Instant::now() < deadline,
                "activity never contained {minimum} occurrences of {expected:?}: {stdout}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn assert_cli_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn activity_file_roundtrip() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let fixture = Fixture::new();
    let first = format!("s10-one-{}", std::process::id());
    let second = format!("s10-two-{}", std::process::id());
    let mut supervisor = fixture.start_supervisor(&binary);
    fixture.wait_for_supervisor(&binary);

    assert_cli_success(
        &fixture.run(&binary, &["session", "start", "--path", "src/one", &first]),
        "start first session",
    );
    assert_cli_success(
        &fixture.run(&binary, &["session", "start", "--path", "src/two", &second]),
        "start second session",
    );

    let mut mcp = McpClient::spawn(&binary, &fixture);
    mcp.initialize();
    for (session_id, content) in [(&first, "first\n"), (&second, "second\n")] {
        mcp.tool(
            "write_file",
            json!({"session_id": session_id, "path": "activity.txt", "content": content}),
        );
        assert_eq!(
            mcp.tool(
                "read_file",
                json!({"session_id": session_id, "path": "activity.txt"}),
            ),
            content
        );
    }

    let all = fixture.wait_for_activity(
        &binary,
        None,
        &[
            &format!("session={first}"),
            &format!("session={second}"),
            "operation=session_start state=completed",
            "operation=write_file state=running",
            "operation=write_file state=completed",
            "operation=read_file state=completed",
        ],
    );
    assert!(all.lines().count() >= 14, "{all}");

    let filtered = fixture.wait_for_activity(
        &binary,
        Some(&first),
        &[
            "operation=session_start state=completed",
            "operation=write_file state=completed",
            "operation=read_file state=completed",
        ],
    );
    assert!(
        filtered
            .lines()
            .all(|line| line.contains(&format!("session={first}")))
    );
    assert!(!filtered.contains(&format!("session={second}")));

    assert_eq!(
        mcp.tool(
            "read_file",
            json!({"session_id": first, "path": "activity.txt"}),
        ),
        "first\n"
    );
    let after_disconnect = fixture.wait_for_activity_count(
        &binary,
        Some(&first),
        "operation=read_file state=completed",
        2,
    );
    assert_eq!(
        after_disconnect
            .matches("operation=read_file state=started")
            .count(),
        2,
        "{after_disconnect}"
    );
    mcp.shutdown();

    assert_cli_success(
        &fixture.run(&binary, &["session", "stop", &first]),
        "stop first session",
    );
    assert_cli_success(
        &fixture.run(&binary, &["session", "stop", &second]),
        "stop second session",
    );
    fixture.wait_for_activity(
        &binary,
        Some(&first),
        &["operation=session_stop state=completed"],
    );

    supervisor.interrupt();
    assert!(supervisor.wait(PROCESS_TIMEOUT).success());
}
