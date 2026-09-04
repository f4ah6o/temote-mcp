#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const MARKER: &str = "fabricated-supervisor-startup-token-5d8e";

fn environ_contains(pid: u32, marker: &[u8]) -> std::io::Result<bool> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => Ok(bytes.windows(marker.len()).any(|window| window == marker)),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error),
    }
}

#[test]
fn actual_supervisor_binary_protects_service_account_startup_environment() {
    let executable = env!("CARGO_BIN_EXE_temote-mcp");
    let mut child = Command::new(executable)
        .arg("mcp")
        .env("OP_SERVICE_ACCOUNT_TOKEN", MARKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start temote-mcp fixture");

    thread::sleep(Duration::from_millis(100));
    assert!(
        !environ_contains(child.id(), MARKER.as_bytes()).expect("inspect supervisor environment"),
        "raw service-account token remained readable from actual temote-mcp startup environment"
    );

    // Keep stdin alive until after the inspection so the stdio MCP server cannot
    // exit early and accidentally turn this into a PID-reuse test.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"");
        drop(stdin);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn sealed_service_account_handoff(marker: &str) -> File {
    use std::ffi::CString;

    let name = CString::new("temote-test-service-account-token").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
    assert!(
        fd >= 3,
        "memfd_create failed: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(marker.as_bytes()).unwrap();
    file.flush().unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(unsafe { libc::fchmod(file.as_raw_fd(), 0) }, 0);
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) },
        0
    );
    file
}

#[test]
fn actual_supervisor_binary_accepts_sealed_upgrade_handoff_without_raw_startup_env() {
    const HANDOFF_MARKER: &str = "fabricated-upgrade-handoff-token-8a31";
    const HANDOFF_ENV: &str = "TEMOTE_MCP_OP_SERVICE_ACCOUNT_TOKEN_FD";

    let executable = env!("CARGO_BIN_EXE_temote-mcp");
    let handoff = sealed_service_account_handoff(HANDOFF_MARKER);
    let handoff_fd = handoff.as_raw_fd();
    let mut child = Command::new(executable)
        .arg("mcp")
        .env_remove("OP_SERVICE_ACCOUNT_TOKEN")
        .env(HANDOFF_ENV, handoff_fd.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start temote-mcp handoff fixture");

    // During the exec-to-main transition the raw token must never be present in
    // the process startup environment. Once bootstrap completes, /proc access
    // should become permission denied because dumpability is disabled.
    for _ in 0..200 {
        match std::fs::read(format!("/proc/{}/environ", child.id())) {
            Ok(bytes) => assert!(
                !bytes
                    .windows(HANDOFF_MARKER.len())
                    .any(|window| window == HANDOFF_MARKER.as_bytes()),
                "raw token appeared in upgraded supervisor startup environment"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                panic!("upgraded supervisor exited before credential bootstrap")
            }
            Err(error) => panic!("failed to inspect upgraded supervisor environment: {error}"),
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "upgraded supervisor exited early"
    );

    // Even in the small pre-main window, mode 000 on the sealed memfd prevents
    // same-UID peers from duplicating and reading the credential through /proc.
    match File::open(format!("/proc/{}/fd/{handoff_fd}", child.id())) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert!(
                !bytes
                    .windows(HANDOFF_MARKER.len())
                    .any(|window| window == HANDOFF_MARKER.as_bytes()),
                "raw token was readable through upgraded supervisor credential FD"
            );
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) => {}
        Err(error) => panic!("unexpected credential FD inspection error: {error}"),
    }

    if let Some(stdin) = child.stdin.take() {
        drop(stdin);
    }
    let _ = child.kill();
    let _ = child.wait();
}
