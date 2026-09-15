use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::os::fd::FromRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const EXIT_TIMEOUT: Duration = Duration::from_secs(7);
static NEXT_NAMESPACE: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy)]
struct ServerScript {
    event: bool,
    end: bool,
    hold_open: bool,
    history_truncated: bool,
}

struct FakeActivityServer {
    namespace: String,
    socket_dir: PathBuf,
    task: Option<thread::JoinHandle<()>>,
}

impl FakeActivityServer {
    fn start(script: ServerScript) -> (Self, mpsc::Receiver<()>) {
        let suffix = NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed);
        let namespace = format!("s9{:x}{suffix:x}", std::process::id());
        assert!(namespace.len() <= 12);
        let uid = unsafe { libc::geteuid() };
        let socket_dir = PathBuf::from(format!("/tmp/tmcp-{uid}-{namespace}"));
        fs::create_dir(&socket_dir).expect("failed to create fake supervisor socket directory");
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = socket_dir.join("supervisor.sock");
        let listener = UnixListener::bind(&socket_path).expect("failed to bind fake supervisor");
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + EXIT_TIMEOUT;
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "activity CLI did not connect");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("failed to accept activity CLI: {error}"),
                }
            };
            stream.set_read_timeout(Some(EXIT_TIMEOUT)).unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(request["command"], "attach_activity");
            let snapshot_sequence = u64::from(script.event);
            let replayed = usize::from(script.event);
            writeln!(
                stream,
                "{}",
                json!({
                    "ok": true,
                    "result": {
                        "control_protocol": 2,
                        "activity_schema": 1,
                        "generation": "00000000-0000-4000-8000-000000009001",
                        "snapshot_sequence": snapshot_sequence,
                        "replayed": replayed,
                        "history_truncated": script.history_truncated,
                    },
                    "error": Value::Null,
                })
            )
            .unwrap();
            if script.event {
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "type": "activity",
                        "event": {
                            "schema_version": 1,
                            "sequence": 1,
                            "operation_id": "00000000-0000-4000-8000-000000009002",
                            "timestamp_ms": 1_780_000_000_123_u64,
                            "session_id": "target",
                            "session_instance": "00000000-0000-4000-8000-000000009003",
                            "operation": "read_file",
                            "state": "started",
                            "duration_ms": Value::Null,
                            "safe_summary": "",
                        }
                    })
                )
                .unwrap();
            }
            if script.end {
                writeln!(
                    stream,
                    "{}",
                    json!({
                        "type": "activity_end",
                        "snapshot_sequence": snapshot_sequence,
                        "history_truncated": script.history_truncated,
                    })
                )
                .unwrap();
            }
            stream.flush().unwrap();
            let _ = ready_sender.send(());
            if script.hold_open {
                stream.set_read_timeout(Some(EXIT_TIMEOUT)).unwrap();
                let mut byte = [0_u8; 1];
                let _ = stream.read(&mut byte);
            }
        });
        (
            Self {
                namespace,
                socket_dir,
                task: Some(task),
            },
            ready_receiver,
        )
    }

    fn finish(mut self) {
        self.task.take().unwrap().join().unwrap();
        let _ = fs::remove_file(self.socket_dir.join("supervisor.sock"));
        let _ = fs::remove_dir(&self.socket_dir);
    }
}

impl Drop for FakeActivityServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.socket_dir.join("supervisor.sock"));
        let _ = fs::remove_dir(&self.socket_dir);
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.spawn().expect("failed to spawn activity CLI"),
        }
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "activity CLI did not exit");
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

fn activity_command(binary: &Path, home: &TempDir, namespace: &str) -> Command {
    for directory in ["codex", "state", "runtime", "temote-runtime", "tmp"] {
        fs::create_dir(home.path().join(directory)).unwrap();
    }
    let mut command = Command::new(binary);
    command
        .env_clear()
        .arg("activity")
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join("codex"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_RUNTIME_DIR", home.path().join("runtime"))
        .env("TEMOTE_MCP_RUNTIME_DIR", home.path().join("temote-runtime"))
        .env("TMPDIR", home.path().join("tmp"))
        .env("TEMOTE_MCP_SOCKET_NAMESPACE", namespace);
    command
}

fn read_child_pipe(pipe: Option<impl Read>) -> String {
    let mut text = String::new();
    pipe.unwrap().read_to_string(&mut text).unwrap();
    text
}

fn full_pipe() -> (File, File) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let bytes = [0_u8; 4096];
    loop {
        let written = unsafe { libc::write(fds[1], bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            break;
        }
    }
    assert_eq!(unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags) }, 0);
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn broken_pipe_writer() -> File {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::close(fds[0]) }, 0);
    unsafe { File::from_raw_fd(fds[1]) }
}

fn open_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) }
}

#[test]
fn activity_cli_no_follow_ignores_stdin_eof_and_requires_replay_end() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: true,
        end: true,
        hold_open: false,
        history_truncated: true,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .args(["--tail", "1", "--no-follow"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard::spawn(&mut command);
    drop(child.child.stdin.take());
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    assert!(child.wait(EXIT_TIMEOUT).success());
    let stdout = read_child_pipe(child.child.stdout.take());
    let stderr = read_child_pipe(child.child.stderr.take());
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    assert!(
        stdout.contains("operation=read_file state=started"),
        "{stdout:?}"
    );
    assert!(stderr.contains("best-effort recent activity"), "{stderr:?}");
    assert!(stderr.contains("history was truncated"), "{stderr:?}");
    server.finish();

    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: false,
        hold_open: false,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .arg("--no-follow")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    assert!(!child.wait(EXIT_TIMEOUT).success());
    let stderr = read_child_pipe(child.child.stderr.take());
    assert!(stderr.contains("before activity_end"), "{stderr:?}");
    server.finish();
}

#[test]
fn activity_cli_follow_ignores_non_tty_eof_and_ctrl_c_exits_zero() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: true,
        hold_open: true,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGuard::spawn(&mut command);
    drop(child.child.stdin.take());
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    thread::sleep(Duration::from_millis(5_200));
    assert!(child.child.try_wait().unwrap().is_none());
    child.interrupt();
    assert!(child.wait(EXIT_TIMEOUT).success());
    server.finish();
}

#[test]
fn activity_cli_follow_socket_eof_and_broken_pipe_exit_zero() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: false,
        hold_open: false,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    assert!(child.wait(EXIT_TIMEOUT).success());
    let stderr = read_child_pipe(child.child.stderr.take());
    assert!(stderr.contains("stream disconnected"), "{stderr:?}");
    server.finish();

    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: true,
        end: false,
        hold_open: true,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .arg("--no-follow")
        .stdin(Stdio::null())
        .stdout(Stdio::from(broken_pipe_writer()))
        .stderr(Stdio::null());
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    assert!(child.wait(EXIT_TIMEOUT).success());
    server.finish();
}

#[test]
fn activity_cli_no_follow_ctrl_c_exits_zero() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: false,
        hold_open: true,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .arg("--no-follow")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(child.child.try_wait().unwrap().is_none());
    child.interrupt();
    assert!(child.wait(EXIT_TIMEOUT).success());
    server.finish();
}

#[test]
fn activity_cli_follow_tty_eof_exits_zero() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: true,
        hold_open: true,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let (mut master, slave) = open_pty();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .stdin(Stdio::from(slave))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    master.write_all(&[4]).unwrap();
    master.flush().unwrap();
    assert!(child.wait(EXIT_TIMEOUT).success());
    server.finish();
}

#[test]
fn activity_cli_ctrl_c_bounds_blocked_stdout_and_stderr_drain() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    for block_stdout in [true, false] {
        let (server, ready) = FakeActivityServer::start(ServerScript {
            event: block_stdout,
            end: true,
            hold_open: !block_stdout,
            history_truncated: false,
        });
        let home = tempfile::tempdir().unwrap();
        let (_reader, writer) = full_pipe();
        let mut command = activity_command(&binary, &home, &server.namespace);
        command.stdin(Stdio::null());
        if block_stdout {
            command
                .arg("--no-follow")
                .stdout(Stdio::from(writer))
                .stderr(Stdio::null());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::from(writer));
        }
        let mut child = ChildGuard::spawn(&mut command);
        ready.recv_timeout(EXIT_TIMEOUT).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(child.child.try_wait().unwrap().is_none());
        child.interrupt();
        let started = Instant::now();
        let status = child.wait(EXIT_TIMEOUT);
        assert!(status.success(), "activity CLI exited with {status}");
        assert!(started.elapsed() <= Duration::from_secs(6));
        server.finish();
    }
}

#[test]
fn activity_cli_blocked_stderr_failure_does_not_hang_final_error_reporting() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_temote-mcp"));
    let (server, ready) = FakeActivityServer::start(ServerScript {
        event: false,
        end: true,
        hold_open: false,
        history_truncated: false,
    });
    let home = tempfile::tempdir().unwrap();
    let (_reader, writer) = full_pipe();
    let mut command = activity_command(&binary, &home, &server.namespace);
    command
        .arg("--no-follow")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer));
    let mut child = ChildGuard::spawn(&mut command);
    ready.recv_timeout(EXIT_TIMEOUT).unwrap();
    let started = Instant::now();
    let status = child.wait(EXIT_TIMEOUT);
    assert!(!status.success());
    assert!(started.elapsed() <= Duration::from_secs(6));
    server.finish();
}
