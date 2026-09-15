#![cfg(all(feature = "network", target_os = "linux"))]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const SOURCE_VERSION: &str = env!("CARGO_PKG_VERSION");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SOURCE_FILES: usize = 512;
const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

fn distinct_target_version() -> String {
    let mut components = SOURCE_VERSION
        .split('.')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(components.len(), 3, "package version is not CalVer");
    let patch = components[2]
        .parse::<u64>()
        .expect("package patch version is not numeric");
    components[2] = (patch + 1).to_string();
    components.join(".")
}

fn socket_namespace() -> String {
    format!("up{:x}", std::process::id())
}

#[derive(Clone)]
struct IsolatedEnv {
    home: PathBuf,
    state: PathBuf,
    temporary: PathBuf,
    runtime: PathBuf,
    fake_bin: PathBuf,
    roots: PathBuf,
    namespace: String,
    host_id: String,
}

impl IsolatedEnv {
    fn new(root: &Path) -> Self {
        let environment = Self {
            home: root.join("home"),
            state: root.join("state"),
            temporary: root.join("tmp"),
            runtime: root.join("runtime"),
            fake_bin: root.join("fake-bin"),
            roots: root.join("roots"),
            namespace: socket_namespace(),
            host_id: "upgrade-reconnect-e2e".to_owned(),
        };
        assert!(environment.namespace.len() <= 12);
        for directory in [
            &environment.home,
            &environment.state,
            &environment.temporary,
            &environment.runtime,
            &environment.fake_bin,
            &environment.roots,
            &environment.home.join("codex"),
        ] {
            fs::create_dir_all(directory).expect("failed to create isolated E2E directory");
        }
        environment
    }

    fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join("codex"))
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("TMPDIR", &self.temporary)
            .env("TEMOTE_MCP_RUNTIME_DIR", &self.runtime)
            .env("TEMOTE_MCP_SOCKET_NAMESPACE", &self.namespace)
            .env("TEMOTE_MCP_HOST_ID", &self.host_id)
            .env(
                "TEMOTE_MCP_ROOTS",
                serde_json::to_string(&json!({"fixture": self.roots})).unwrap(),
            )
            .env(
                "PATH",
                format!("{}:/usr/local/bin:/usr/bin:/bin", self.fake_bin.display()),
            )
            .env("USER", "temote-upgrade-e2e")
            .env("LOGNAME", "temote-upgrade-e2e")
            .env("LANG", "C.UTF-8");
    }

    fn supervisor_socket(&self) -> PathBuf {
        let uid = unsafe { libc::geteuid() };
        PathBuf::from("/tmp")
            .join(format!("tmcp-{uid}-{}", self.namespace))
            .join("supervisor.sock")
    }

    fn transaction_directory(&self) -> PathBuf {
        self.state.join("temote-mcp/upgrade-transactions")
    }

    fn ingress_state(&self) -> PathBuf {
        self.runtime.join("temote-mcp/up.state.json")
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command, label: &str) -> Self {
        Self {
            child: command
                .spawn()
                .unwrap_or_else(|error| panic!("failed to spawn {label}: {error}")),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn wait_for_exit(&mut self, timeout: Duration, label: &str) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("failed to poll child") {
                assert!(status.success(), "{label} exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "{label} did not exit in time");
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn interrupt(&mut self) {
        let result = unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
        assert_eq!(result, 0, "failed to interrupt child");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct ScopedProcessCleanup {
    binary: PathBuf,
    environment: IsolatedEnv,
    armed: bool,
}

impl ScopedProcessCleanup {
    fn new(binary: &Path, environment: &IsolatedEnv) -> Self {
        Self {
            binary: binary.to_owned(),
            environment: environment.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScopedProcessCleanup {
    fn drop(&mut self) {
        if self.armed {
            terminate_scoped_processes(&self.environment, ScopedProcessKind::Coordinator);
            run_bounded_cleanup_command(
                &self.binary,
                &["down"],
                &self.environment,
                Duration::from_secs(5),
            );
            terminate_scoped_processes(&self.environment, ScopedProcessKind::Any);
        }
    }
}

#[derive(Clone, Copy)]
enum ScopedProcessKind {
    Coordinator,
    Any,
}

fn process_has_environment_marker(pid: u32, environment: &IsolatedEnv) -> bool {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let namespace = format!("TEMOTE_MCP_SOCKET_NAMESPACE={}", environment.namespace);
    let state = format!("XDG_STATE_HOME={}", environment.state.display());
    let entries = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    entries.iter().any(|entry| *entry == namespace.as_bytes())
        && entries.iter().any(|entry| *entry == state.as_bytes())
}

fn process_matches_kind(pid: u32, kind: ScopedProcessKind) -> bool {
    match kind {
        ScopedProcessKind::Any => true,
        ScopedProcessKind::Coordinator => fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .is_some_and(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == b"upgrade-coordinator")
            }),
    }
}

struct ScopedProcess {
    pidfd: OwnedFd,
}

fn open_pidfd(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as libc::c_int;
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn send_pidfd_signal(pidfd: &OwnedFd, signal: libc::c_int) -> bool {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        ) == 0
    }
}

fn assert_pidfd_cleanup_supported() {
    let pidfd = open_pidfd(std::process::id())
        .expect("Linux pidfd_open is required for scoped process cleanup");
    assert!(
        send_pidfd_signal(&pidfd, 0),
        "Linux pidfd_send_signal is required for scoped process cleanup"
    );
}

fn scoped_processes(environment: &IsolatedEnv, kind: ScopedProcessKind) -> Vec<ScopedProcess> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != std::process::id())
        .filter_map(|pid| Some((pid, open_pidfd(pid)?)))
        .filter(|(pid, _)| process_has_environment_marker(*pid, environment))
        .filter(|(pid, _)| process_matches_kind(*pid, kind))
        .map(|(_, pidfd)| ScopedProcess { pidfd })
        .collect()
}

fn signal_scoped_processes(
    environment: &IsolatedEnv,
    kind: ScopedProcessKind,
    signal: libc::c_int,
) {
    for process in scoped_processes(environment, kind) {
        let _ = send_pidfd_signal(&process.pidfd, signal);
    }
}

fn run_bounded_cleanup_command(
    binary: &Path,
    args: &[&str],
    environment: &IsolatedEnv,
    timeout: Duration,
) {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    environment.apply(&mut command);
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

fn terminate_scoped_processes(environment: &IsolatedEnv, kind: ScopedProcessKind) {
    signal_scoped_processes(environment, kind, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if scoped_processes(environment, kind).is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    signal_scoped_processes(environment, kind, libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if scoped_processes(environment, kind).is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn command_output(binary: &Path, args: &[&str], environment: &IsolatedEnv) -> Output {
    let mut command = Command::new(binary);
    command.args(args).stdin(Stdio::null());
    environment.apply(&mut command);
    command.output().expect("failed to run Temote command")
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("failed to write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("failed to protect executable fixture");
}

fn install_fake_tailscale(environment: &IsolatedEnv) {
    write_executable(
        &environment.fake_bin.join("tailscale"),
        r#"#!/bin/sh
set -eu
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
  printf '%s\n' '{"Self":{"DNSName":"upgrade-reconnect-e2e.ts.net."}}'
  exit 0
fi
if [ "$1" = "funnel" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
  printf '%s\n' '{"TCP":{},"Web":{}}'
  exit 0
fi
if [ "$1" = "funnel" ] && [ "$2" = "--yes" ]; then
  trap 'exit 0' INT TERM
  while :; do /bin/sleep 1; done
fi
printf 'unexpected fake tailscale arguments' >&2
exit 2
"#,
    );
}

fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("failed to reserve test port")
        .local_addr()
        .unwrap()
        .port()
}

struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn decode_chunked(mut input: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk has no size line");
        let size = usize::from_str_radix(
            std::str::from_utf8(&input[..line_end])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .expect("invalid HTTP chunk size");
        input = &input[line_end + 2..];
        if size == 0 {
            break;
        }
        assert!(input.len() >= size + 2, "truncated HTTP chunk");
        decoded.extend_from_slice(&input[..size]);
        assert_eq!(&input[size..size + 2], b"\r\n");
        input = &input[size + 2..];
    }
    decoded
}

fn parse_http_response(bytes: &[u8]) -> HttpResponse {
    let separator = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response has no header terminator");
    let head = std::str::from_utf8(&bytes[..separator]).expect("HTTP headers are not UTF-8");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("HTTP response has no status");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        decode_chunked(&bytes[separator + 4..])
    } else {
        bytes[separator + 4..].to_vec()
    };
    HttpResponse {
        status,
        headers,
        body,
    }
}

fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("failed to connect to HTTP origin");
    stream
        .set_read_timeout(Some(UPGRADE_TIMEOUT))
        .expect("failed to set HTTP read timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("failed to read HTTP response");
    parse_http_response(&response)
}

fn http_json(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap_or_else(|error| {
        panic!(
            "invalid HTTP JSON body: {error}; body={}",
            String::from_utf8_lossy(&response.body)
        )
    })
}

fn wait_for_health(addr: SocketAddr, version: &str) -> Value {
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    loop {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("failed to bound health response read");
            let _ = stream.write_all(
                b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            );
            let mut response = Vec::new();
            if stream.read_to_end(&mut response).is_ok() && !response.is_empty() {
                let response = parse_http_response(&response);
                if response.status == 200 {
                    let value = http_json(&response);
                    if value["version"] == version {
                        return value;
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "HTTP origin did not become healthy"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

struct ApprovalConsole {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl ApprovalConsole {
    fn attach(environment: &IsolatedEnv) -> Self {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let stream = loop {
            match UnixStream::connect(environment.supervisor_socket()) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                Err(error) => panic!("failed to attach approval console: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(STARTUP_TIMEOUT))
            .expect("failed to bound approval console read");
        stream
            .set_write_timeout(Some(STARTUP_TIMEOUT))
            .expect("failed to bound approval console write");
        let mut writer = stream.try_clone().unwrap();
        writer
            .write_all(b"{\"command\":\"attach_console\"}\n")
            .unwrap();
        writer.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(response["ok"], true, "console attach failed: {response}");
        Self { reader, writer }
    }

    fn wait_for(&mut self, expected_operation: &str) {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "approval console disconnected");
        let prompt: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(prompt["type"], "approval");
        assert_eq!(prompt["operation"], expected_operation);
    }

    fn respond(&mut self, allowed: bool) {
        let response = if allowed {
            b"{\"allow\":true}\n".as_slice()
        } else {
            b"{\"allow\":false}\n".as_slice()
        };
        self.writer.write_all(response).unwrap();
        self.writer.flush().unwrap();
    }

    fn allow(&mut self, expected_operation: &str) {
        self.wait_for(expected_operation);
        self.respond(true);
    }
}

fn oauth_token(addr: SocketAddr, console: &mut ApprovalConsole) -> String {
    let registration = http_request(
        addr,
        "POST",
        "/register",
        &[("content-type", "application/json")],
        serde_json::to_string(&json!({
            "client_name": "upgrade reconnect E2E",
            "application_type": "native",
            "redirect_uris": ["http://127.0.0.1:9876/callback"],
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .unwrap()
        .as_bytes(),
    );
    assert_eq!(registration.status, 201);
    let client_id = http_json(&registration)["client_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", "http://127.0.0.1:9876/callback")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", "https://upgrade-reconnect-e2e.ts.net:8443/mcp")
        .append_pair("scope", "mcp")
        .append_pair("state", "upgrade-state");
    let authorize_path = format!("/authorize?{}", query.finish());
    let authorize = thread::spawn(move || http_request(addr, "GET", &authorize_path, &[], &[]));
    console.allow("oauth_authorize");
    let authorize = authorize.join().unwrap();
    assert_eq!(authorize.status, 302);
    let location = url::Url::parse(authorize.headers.get("location").unwrap()).unwrap();
    let code = location
        .query_pairs()
        .find_map(|(name, value)| (name == "code").then(|| value.into_owned()))
        .expect("OAuth redirect has no code");
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "authorization_code")
        .append_pair("code", &code)
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", "http://127.0.0.1:9876/callback")
        .append_pair("code_verifier", verifier)
        .append_pair("resource", "https://upgrade-reconnect-e2e.ts.net:8443/mcp");
    let token = http_request(
        addr,
        "POST",
        "/token",
        &[("content-type", "application/x-www-form-urlencoded")],
        form.finish().as_bytes(),
    );
    assert_eq!(token.status, 200);
    http_json(&token)["access_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn mcp_call(addr: SocketAddr, token: &str, name: &str, arguments: Value) -> HttpResponse {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments}
    }))
    .unwrap();
    http_request(
        addr,
        "POST",
        "/mcp",
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("Bearer {token}")),
        ],
        &body,
    )
}

fn mcp_tool_json(response: &HttpResponse) -> Value {
    assert_eq!(response.status, 200);
    let response = http_json(response);
    assert!(
        response.get("error").is_none(),
        "MCP call failed: {response}"
    );
    serde_json::from_str(
        response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("MCP tool result has no text"),
    )
    .expect("MCP tool text is not JSON")
}

fn copy_source_tree(destination: &Path) {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let git_home = destination
        .parent()
        .expect("target source has no parent")
        .join("git-home");
    fs::create_dir(&git_home).expect("failed to create isolated Git home");
    let mut git = Command::new("git");
    git.args(["ls-files", "-z"])
        .current_dir(&repository)
        .env_clear()
        .env("PATH", required_outer_env("PATH"))
        .env("HOME", &git_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LANG", "C.UTF-8");
    let output = git.output().expect("failed to list tracked source files");
    assert!(output.status.success(), "git ls-files failed");
    let names = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    assert!(
        names.len() <= MAX_SOURCE_FILES,
        "tracked source fixture exceeds {MAX_SOURCE_FILES} files"
    );
    let mut copied_bytes = 0_u64;
    for name in names {
        let name = std::str::from_utf8(name).expect("tracked path is not UTF-8");
        let source = repository.join(name);
        let target = destination.join(name);
        let metadata = fs::symlink_metadata(&source)
            .unwrap_or_else(|error| panic!("failed to inspect tracked source {name}: {error}"));
        assert!(
            metadata.file_type().is_file(),
            "tracked source fixture accepts only regular files (no submodules or symlinks): {name}"
        );
        copied_bytes = copied_bytes
            .checked_add(metadata.len())
            .expect("tracked source fixture byte count overflowed");
        assert!(
            copied_bytes <= MAX_SOURCE_BYTES,
            "tracked source fixture exceeds {MAX_SOURCE_BYTES} bytes"
        );
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("failed to create copied source directory");
        }
        fs::copy(&source, &target)
            .unwrap_or_else(|error| panic!("failed to copy tracked source {name}: {error}"));
    }
}

fn required_outer_env(name: &str) -> OsString {
    std::env::var_os(name).unwrap_or_else(|| panic!("required outer environment variable {name}"))
}

fn outer_home_directory(environment_name: &str, default_name: &str) -> Option<PathBuf> {
    std::env::var_os(environment_name)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(default_name))
        })
}

fn prepare_isolated_cargo_home(root: &Path) -> PathBuf {
    let cargo_home = root.join("cargo-home");
    fs::create_dir(&cargo_home).expect("failed to create isolated Cargo home");
    if let Some(shared) = outer_home_directory("CARGO_HOME", ".cargo") {
        for cache in ["registry", "git"] {
            let source = shared.join(cache);
            if source.is_dir() {
                symlink(&source, cargo_home.join(cache))
                    .unwrap_or_else(|error| panic!("failed to link Cargo {cache} cache: {error}"));
            }
        }
    }
    cargo_home
}

fn replace_once(contents: &str, old: &str, new: &str, label: &str) -> String {
    assert_eq!(contents.matches(old).count(), 1, "unexpected {label} shape");
    contents.replacen(old, new, 1)
}

fn build_distinct_target(root: &Path, target_version: &str) -> (PathBuf, PathBuf) {
    let source = root.join("target-source");
    fs::create_dir(&source).expect("failed to create target source directory");
    copy_source_tree(&source);

    let manifest_path = source.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).unwrap();
    fs::write(
        &manifest_path,
        replace_once(
            &manifest,
            &format!("version = \"{SOURCE_VERSION}\""),
            &format!("version = \"{target_version}\""),
            "Cargo.toml package version",
        ),
    )
    .unwrap();
    let lock_path = source.join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path).unwrap();
    let source_entry =
        format!("[[package]]\nname = \"temote-mcp\"\nversion = \"{SOURCE_VERSION}\"");
    let target_entry =
        format!("[[package]]\nname = \"temote-mcp\"\nversion = \"{target_version}\"");
    fs::write(
        &lock_path,
        replace_once(
            &lock,
            &source_entry,
            &target_entry,
            "Cargo.lock package entry",
        ),
    )
    .unwrap();

    let target = root.join("target-build");
    let cargo_home = prepare_isolated_cargo_home(root);
    let build_home = root.join("build-home");
    fs::create_dir(&build_home).expect("failed to create isolated build home");
    let mut build = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    build
        .args(["build", "--offline", "--bins"])
        .current_dir(&source)
        .env_clear()
        .env("PATH", required_outer_env("PATH"))
        .env("HOME", &build_home)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_NET_OFFLINE", "true")
        .env("TMPDIR", root.join("tmp"))
        .env("LANG", "C.UTF-8");
    if let Some(rustup_home) = outer_home_directory("RUSTUP_HOME", ".rustup") {
        build.env("RUSTUP_HOME", rustup_home);
    }
    for name in ["RUSTUP_TOOLCHAIN", "RUSTC"] {
        if let Some(value) = std::env::var_os(name) {
            build.env(name, value);
        }
    }
    let status = build
        .status()
        .expect("failed to build distinct target executable");
    assert!(
        status.success(),
        "distinct target build failed with {status}"
    );
    (
        target.join("debug/temote-mcp"),
        target.join("debug/temote-linux-sandbox"),
    )
}

fn copy_executable(source: &Path, destination: &Path) {
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy executable {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
}

fn install_source_binary(root: &Path) -> PathBuf {
    let install = root.join("installed");
    fs::create_dir(&install).unwrap();
    let binary = install.join("temote-mcp");
    copy_executable(Path::new(env!("CARGO_BIN_EXE_temote-mcp")), &binary);
    copy_executable(
        Path::new(env!("CARGO_BIN_EXE_temote-linux-sandbox")),
        &install.join("temote-linux-sandbox"),
    );
    binary
}

fn atomically_replace_executable(installed: &Path, replacement: &Path) {
    let pending = installed.with_extension("new");
    copy_executable(replacement, &pending);
    fs::rename(&pending, installed).expect("failed to atomically replace installed executable");
}

fn spawn_supervisor(binary: &Path, environment: &IsolatedEnv) -> ChildGuard {
    let mut command = Command::new(binary);
    command
        .arg("supervisor")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    environment.apply(&mut command);
    ChildGuard::spawn(&mut command, "supervisor")
}

fn wait_for_supervisor(binary: &Path, environment: &IsolatedEnv) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let output = command_output(binary, &["session", "list"], environment);
        if output.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not become ready: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn session_info(binary: &Path, environment: &IsolatedEnv, id: &str) -> Value {
    let output = command_output(binary, &["session", "info", id], environment);
    assert_success(&output, "session info");
    serde_json::from_slice(&output.stdout).expect("session info returned invalid JSON")
}

fn spawn_ingress(binary: &Path, environment: &IsolatedEnv, addr: SocketAddr) -> ChildGuard {
    let mut command = Command::new(binary);
    command
        .args([
            "up",
            "--profile",
            "tailscale",
            "--public-url",
            "https://upgrade-reconnect-e2e.ts.net:8443",
            "--addr",
            &addr.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    environment.apply(&mut command);
    ChildGuard::spawn(&mut command, "direct ingress")
}

fn ingress_pid(environment: &IsolatedEnv) -> u64 {
    serde_json::from_slice::<Value>(&fs::read(environment.ingress_state()).unwrap()).unwrap()["pid"]
        .as_u64()
        .expect("ingress runtime state has no PID")
}

fn transaction_values(environment: &IsolatedEnv) -> Vec<Value> {
    fs::read_dir(environment.transaction_directory())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "json")
        })
        .map(|entry| {
            serde_json::from_slice(&fs::read(entry.path()).unwrap())
                .expect("upgrade transaction is invalid JSON")
        })
        .collect()
}

fn wait_for_upgrade_snapshot_cleanup(environment: &IsolatedEnv) {
    let directory = environment
        .temporary
        .join(format!("temote-mcp-upgrade-candidates-{}", unsafe {
            libc::geteuid()
        }));
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let remaining = match fs::read_dir(&directory) {
            Ok(entries) => entries
                .map(|entry| entry.expect("failed to inspect upgrade snapshot").path())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("failed to inspect upgrade snapshot directory: {error}"),
        };
        if remaining.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "upgrade execution snapshots were not removed: {remaining:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn mcp_initialize(addr: SocketAddr, token: &str) -> Value {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "upgrade-reconnect-e2e", "version": "1"}
        }
    }))
    .unwrap();
    let response = http_request(
        addr,
        "POST",
        "/mcp",
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("Bearer {token}")),
        ],
        &body,
    );
    assert_eq!(response.status, 200);
    let value = http_json(&response);
    assert!(value.get("error").is_none(), "initialize failed: {value}");
    value["result"].clone()
}

fn wait_for_upgrade_status(
    addr: SocketAddr,
    token: &str,
    transaction_id: &str,
    state: &str,
) -> Value {
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    loop {
        let status = mcp_tool_json(&mcp_call(
            addr,
            token,
            "upgrade_status",
            json!({"transaction_id": transaction_id}),
        ));
        if status["state"] == state {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "upgrade transaction did not reach {state}: {status}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_ingress_pid_change(environment: &IsolatedEnv, source_pid: u64) -> u64 {
    let deadline = Instant::now() + UPGRADE_TIMEOUT;
    loop {
        if let Ok(bytes) = fs::read(environment.ingress_state())
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(pid) = value["pid"].as_u64()
            && pid != source_pid
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "replacement direct ingress PID was not published"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "Linux process-boundary direct-HTTP upgrade E2E; run explicitly"]
fn direct_http_upgrade_reconnects_after_real_ingress_replacement() {
    assert_pidfd_cleanup_supported();
    let fixture = TempDir::new().expect("failed to create upgrade E2E fixture");
    let environment = IsolatedEnv::new(fixture.path());
    install_fake_tailscale(&environment);
    for relative in ["one", "two"] {
        let directory = environment.roots.join(relative);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("marker.txt"), format!("{relative}\n")).unwrap();
    }

    let target_version = distinct_target_version();
    let (target_binary, _) = build_distinct_target(fixture.path(), &target_version);
    let installed = install_source_binary(fixture.path());
    let mut process_cleanup = ScopedProcessCleanup::new(&installed, &environment);
    let mut supervisor = spawn_supervisor(&installed, &environment);
    let supervisor_pid = supervisor.id();
    wait_for_supervisor(&installed, &environment);

    let addr = SocketAddr::from(([127, 0, 0, 1], reserve_port()));
    let mut source_ingress = spawn_ingress(&installed, &environment, addr);
    let source_health = wait_for_health(addr, SOURCE_VERSION);
    assert_eq!(source_health["status"], "ok");
    assert_eq!(source_health["host_id"], environment.host_id);
    let source_generation = source_health["boot_generation"]
        .as_str()
        .unwrap()
        .to_owned();
    let source_ingress_pid = ingress_pid(&environment);
    assert_eq!(source_ingress_pid, source_ingress.id() as u64);

    let mut console = ApprovalConsole::attach(&environment);
    let token = oauth_token(addr, &mut console);
    let initialized = mcp_initialize(addr, &token);
    assert_eq!(initialized["serverInfo"]["version"], SOURCE_VERSION);
    assert_eq!(
        initialized["_meta"]["io.temote/processIdentity"]["boot_generation"],
        source_generation
    );

    let first = mcp_tool_json(&mcp_call(
        addr,
        &token,
        "session_start",
        json!({"session_id": "upgrade-one", "path": "fixture/one"}),
    ));
    let second = mcp_tool_json(&mcp_call(
        addr,
        &token,
        "session_start",
        json!({"session_id": "upgrade-two", "path": "fixture/two"}),
    ));
    for session in [&first, &second] {
        assert_eq!(session["status"], "active");
    }
    let source_sessions = ["upgrade-one", "upgrade-two"]
        .map(|session_id| session_info(&installed, &environment, session_id));
    for session in &source_sessions {
        assert_eq!(session["status"], "active");
        assert_eq!(session["permission_mode"], "agent");
        assert_eq!(session["yolo"], false);
    }

    let source_executable_inode = fs::metadata(&installed).unwrap().ino();
    atomically_replace_executable(&installed, &target_binary);
    assert_ne!(
        fs::metadata(&installed).unwrap().ino(),
        source_executable_inode
    );
    for pid in [supervisor_pid, source_ingress_pid as u32] {
        let running_executable = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        assert!(
            running_executable.to_string_lossy().ends_with(" (deleted)"),
            "atomic replacement did not detach process {pid} from the locator: {}",
            running_executable.display()
        );
    }
    let preflight = mcp_tool_json(&mcp_call(addr, &token, "upgrade_preflight", json!({})));
    assert_eq!(preflight["source_version"], SOURCE_VERSION);
    assert_eq!(preflight["target_version"], target_version);
    assert_eq!(preflight["planned_session_count"], 2);
    assert_eq!(preflight["blocked_session_count"], 0);
    assert_eq!(preflight["supervisor_handoff_required"], true);
    assert_eq!(preflight["direct_ingress_action"], "restart");
    assert_eq!(preflight["direct_ingress_blocked"], false);
    assert_eq!(preflight["reconnect_expected"], true);

    for source in &source_sessions {
        let current = session_info(
            &installed,
            &environment,
            source["session_id"].as_str().unwrap(),
        );
        assert_eq!(current["process_id"], source["process_id"]);
        assert_eq!(current["started_at"], source["started_at"]);
    }

    let apply_addr = addr;
    let apply_token = token.clone();
    let apply_version = target_version.clone();
    let apply = thread::spawn(move || {
        mcp_call(
            apply_addr,
            &apply_token,
            "upgrade_apply",
            json!({"session_id": "upgrade-one", "expected_version": apply_version}),
        )
    });
    console.allow("upgrade_apply");
    let accepted_response = apply.join().unwrap();
    assert_eq!(
        accepted_response
            .headers
            .get("connection")
            .map(String::as_str),
        Some("close")
    );
    let accepted = mcp_tool_json(&accepted_response);
    assert_eq!(accepted["accepted"], true);
    assert_eq!(accepted["transaction"]["state"], "prepared");
    let transaction_id = accepted["transaction"]["transaction_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(console);

    let replacement_ingress_pid = wait_for_ingress_pid_change(&environment, source_ingress_pid);
    assert_ne!(replacement_ingress_pid, source_ingress_pid);
    source_ingress.wait_for_exit(UPGRADE_TIMEOUT, "source direct ingress");
    let replacement_health = wait_for_health(addr, &target_version);
    assert_eq!(replacement_health["status"], "ok");
    assert_eq!(replacement_health["service"], "temote-mcp");
    assert_eq!(replacement_health["host_id"], environment.host_id);
    assert_ne!(replacement_health["boot_generation"], source_generation);
    assert_eq!(
        replacement_health["last_upgrade_transaction"],
        transaction_id
    );
    assert_eq!(supervisor.id(), supervisor_pid);
    assert!(supervisor.child.try_wait().unwrap().is_none());

    let mut replacement_console = ApprovalConsole::attach(&environment);
    let replacement_token = oauth_token(addr, &mut replacement_console);
    let replacement_initialize = mcp_initialize(addr, &replacement_token);
    let process_identity = &replacement_initialize["_meta"]["io.temote/processIdentity"];
    assert_eq!(process_identity["host_id"], environment.host_id);
    assert_eq!(process_identity["version"], target_version);
    assert_eq!(
        process_identity["boot_generation"],
        replacement_health["boot_generation"]
    );
    let completed = wait_for_upgrade_status(addr, &replacement_token, &transaction_id, "completed");
    assert_eq!(completed["terminal"], true);
    assert_eq!(completed["source_version"], SOURCE_VERSION);
    assert_eq!(completed["target_version"], target_version);
    assert_eq!(completed["host_id"], environment.host_id);
    assert_eq!(completed["verified_host_id"], environment.host_id);
    assert_eq!(completed["verified_version"], target_version);
    assert_eq!(
        completed["verified_boot_generation"],
        replacement_health["boot_generation"]
    );
    assert_eq!(completed["restored_session_count"], 2);
    assert_eq!(completed["coordinator_alive"], Value::Null);
    assert_eq!(completed["incomplete"], false);
    assert_eq!(completed["failure_summary"], Value::Null);

    let transactions = transaction_values(&environment);
    assert_eq!(
        transactions.len(),
        1,
        "unexpected transactions: {transactions:?}"
    );
    assert_eq!(transactions[0]["transaction_id"], transaction_id);
    assert_eq!(transactions[0]["state"], "completed");
    wait_for_upgrade_snapshot_cleanup(&environment);

    for source in &source_sessions {
        let current = session_info(
            &installed,
            &environment,
            source["session_id"].as_str().unwrap(),
        );
        assert_eq!(current["status"], "active");
        assert_eq!(current["permission_mode"], "agent");
        assert_eq!(current["logical_path"], source["logical_path"]);
    }

    for session_id in ["upgrade-one", "upgrade-two"] {
        let stop = command_output(&installed, &["session", "stop", session_id], &environment);
        assert_success(&stop, "session stop after upgrade");
    }
    let down = command_output(&installed, &["down"], &environment);
    assert_success(&down, "direct ingress shutdown after upgrade");
    supervisor.interrupt();
    supervisor.wait_for_exit(STARTUP_TIMEOUT, "upgraded supervisor");
    process_cleanup.disarm();
}
