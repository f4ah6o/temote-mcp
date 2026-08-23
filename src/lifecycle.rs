use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;

use crate::profile::Profile;

const PID_FILE_NAME: &str = "up.pid";
const LEGACY_PID_FILE_NAME: &str = "up.pids";
const MAX_LEGACY_PID_FILE_BYTES: usize = 64;
const PROCESS_NAME: &str = env!("CARGO_PKG_NAME");
const MAX_PID_FILE_BYTES: usize = 64;
const MAX_TUNNEL_TOKEN_BYTES: u64 = 64 * 1024;
#[cfg(target_os = "macos")]
const PS_COMMAND: &str = "/bin/ps";
#[cfg(target_os = "macos")]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";
#[cfg(target_os = "linux")]
const PS_COMMAND: &str = "/usr/bin/ps";
#[cfg(target_os = "linux")]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
const PS_COMMAND: &str = "/bin/ps";
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
const PGREP_COMMAND: &str = "/usr/bin/pgrep";

pub async fn up(
    profile: Profile,
    public_url: Option<String>,
    addr: std::net::SocketAddr,
    tunnel_token_file: Option<PathBuf>,
) -> Result<()> {
    let legacy_pid_file = runtime_directory()?.join(LEGACY_PID_FILE_NAME);
    if read_legacy_up_pids(&legacy_pid_file)?.is_some() {
        anyhow::bail!(
            "legacy Temote runtime state exists at {}; run `temote-mcp migrate --dry-run` and `temote-mcp migrate` before `temote-mcp up`",
            legacy_pid_file.display()
        );
    }

    if profile == Profile::Cloudflare {
        crate::load_public_env()?;
    }

    let tunnel_token_file = match profile {
        Profile::Cloudflare => {
            let path = tunnel_token_file
                .or_else(|| env_path("TUNNEL_TOKEN_FILE"))
                .unwrap_or(default_tunnel_token_file()?);
            ensure_tunnel_token_file(&path)?;
            Some(path)
        }
        Profile::Tailscale | Profile::Openai => {
            anyhow::ensure!(
                tunnel_token_file.is_none(),
                "--tunnel-token-file is only valid for the cloudflare profile"
            );
            None
        }
    };

    let pid_file = pid_file(true)?;
    let _pid_file = PidFile::create(&pid_file)?;
    crate::serve_http(
        profile,
        public_url,
        addr,
        true,
        tunnel_token_file.as_deref(),
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LegacyUpPids {
    serve: i32,
    tunnel: i32,
}

pub async fn migrate(dry_run: bool) -> Result<()> {
    let path = runtime_directory()?.join(LEGACY_PID_FILE_NAME);
    let Some(pids) = read_legacy_up_pids(&path)? else {
        println!("no legacy temote-mcp runtime state found");
        return Ok(());
    };

    let serve_alive = process_exists(pids.serve);
    let tunnel_alive = process_exists(pids.tunnel);
    if serve_alive {
        ensure_process_name(pids.serve, PROCESS_NAME, "legacy Temote supervisor")?;
    }
    if tunnel_alive {
        ensure_process_name(pids.tunnel, "cloudflared", "legacy Cloudflare Tunnel")?;
    }

    if dry_run {
        println!(
            "legacy runtime migration required: {} (serve pid {}, tunnel pid {})",
            path.display(),
            pids.serve,
            pids.tunnel
        );
        println!("dry run: no processes were signaled and no state was removed");
        return Ok(());
    }

    if !serve_alive && !tunnel_alive {
        remove_legacy_pid_file(&path)?;
        println!("removed stale legacy runtime state {}", path.display());
        return Ok(());
    }

    if serve_alive {
        send_signal(pids.serve, libc::SIGTERM)?;
    }
    if tunnel_alive {
        send_signal(pids.tunnel, libc::SIGTERM)?;
    }

    for _ in 0..15 {
        if !process_exists(pids.serve) && !process_exists(pids.tunnel) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    if process_exists(pids.serve) {
        ensure_process_name(pids.serve, PROCESS_NAME, "legacy Temote supervisor")?;
        send_signal(pids.serve, libc::SIGKILL)?;
    }
    if process_exists(pids.tunnel) {
        ensure_process_name(pids.tunnel, "cloudflared", "legacy Cloudflare Tunnel")?;
        send_signal(pids.tunnel, libc::SIGKILL)?;
    }

    for _ in 0..10 {
        if !process_exists(pids.serve) && !process_exists(pids.tunnel) {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(
        !process_exists(pids.serve) && !process_exists(pids.tunnel),
        "legacy Temote runtime did not stop cleanly; refusing to remove {}",
        path.display()
    );

    remove_legacy_pid_file(&path)?;
    println!("migrated legacy Temote runtime state");
    println!("configuration and independently running local sessions were left unchanged");
    println!("next: run `temote-mcp up --profile cloudflare`");
    Ok(())
}

fn read_legacy_up_pids(path: &Path) -> Result<Option<LegacyUpPids>> {
    let file = match open_readonly_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open legacy runtime state {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect legacy runtime state {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "legacy runtime state must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_LEGACY_PID_FILE_BYTES as u64,
        "legacy runtime state exceeds {MAX_LEGACY_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_LEGACY_PID_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read legacy runtime state {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_LEGACY_PID_FILE_BYTES,
        "legacy runtime state exceeds {MAX_LEGACY_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let raw = std::str::from_utf8(&bytes).context("legacy runtime state is not valid UTF-8")?;
    Ok(Some(parse_legacy_up_pids(raw)?))
}

fn parse_legacy_up_pids(raw: &str) -> Result<LegacyUpPids> {
    let fields: Vec<_> = raw.split_whitespace().collect();
    anyhow::ensure!(
        fields.len() == 2,
        "legacy runtime state must contain exactly two positive PIDs"
    );
    Ok(LegacyUpPids {
        serve: parse_pid(fields[0]).context("invalid legacy Temote supervisor PID")?,
        tunnel: parse_pid(fields[1]).context("invalid legacy Cloudflare Tunnel PID")?,
    })
}

fn ensure_process_name(pid: i32, expected: &str, label: &str) -> Result<()> {
    let actual = process_name(pid)?;
    anyhow::ensure!(
        actual.as_deref() == Some(expected),
        "{label} PID {pid} belongs to an unexpected process ({actual:?}); refusing to signal it"
    );
    Ok(())
}

fn remove_legacy_pid_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

pub async fn down() -> Result<()> {
    let pid_file = pid_file(false)?;
    let mut pid_handle = match open_readonly_nofollow(&pid_file) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no temote-mcp up process is recorded");
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to open {}", pid_file.display()));
        }
    };
    let pid = read_pid_from_open_file(&mut pid_handle, &pid_file)
        .with_context(|| format!("failed to read {}", pid_file.display()))?;
    if try_acquire_pid_lock(&pid_handle)? {
        let _ = std::fs::remove_file(&pid_file);
        println!("recorded temote-mcp process is not running");
        return Ok(());
    }
    anyhow::ensure!(
        is_temote_process(pid)?,
        "PID file is locked by an unexpected process; refusing to signal PID {pid}"
    );

    let tunnel_pids = child_processes(pid);
    send_signal(pid, libc::SIGTERM)?;

    for _ in 0..15 {
        if !process_exists(pid) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    if process_exists(pid) {
        for child_pid in tunnel_pids {
            if process_exists(child_pid) {
                let _ = send_signal(child_pid, libc::SIGKILL);
            }
        }
        let _ = send_signal(pid, libc::SIGKILL);
    }

    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}

struct PidFile {
    path: PathBuf,
    _file: std::fs::File,
}

impl PidFile {
    fn create(path: &Path) -> Result<Self> {
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(mut file) => {
                    acquire_pid_lock(&file)
                        .with_context(|| format!("failed to lock {}", path.display()))?;
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file.set_permissions(std::fs::Permissions::from_mode(0o600))
                            .with_context(|| format!("failed to protect {}", path.display()))?;
                    }
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("failed to sync {}", path.display()))?;
                    return Ok(Self {
                        path: path.to_owned(),
                        _file: file,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let mut existing = open_readonly_nofollow(path).with_context(|| {
                        format!("cannot safely open existing PID file {}", path.display())
                    })?;
                    let _pid = read_pid_from_open_file(&mut existing, path).with_context(|| {
                        format!("cannot safely inspect existing PID file {}", path.display())
                    })?;
                    if !try_acquire_pid_lock(&existing)? {
                        anyhow::bail!("temote-mcp is already running; use temote-mcp down first");
                    }
                    std::fs::remove_file(path).with_context(|| {
                        format!("failed to remove stale PID file {}", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }

        anyhow::bail!("failed to create {}", path.display())
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn pid_file(create_parent: bool) -> Result<PathBuf> {
    let directory = runtime_directory()?;
    if create_parent {
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
    }
    Ok(directory.join(PID_FILE_NAME))
}

fn runtime_directory() -> Result<PathBuf> {
    if let Some(path) = env_path("TEMOTE_MCP_RUNTIME_DIR") {
        return Ok(path.join("temote-mcp"));
    }
    if let Some(path) = env_path("XDG_RUNTIME_DIR") {
        return Ok(path.join("temote-mcp"));
    }
    dirs::home_dir()
        .map(|home| home.join(".cache").join("temote-mcp"))
        .context("could not determine a runtime directory")
}

fn default_tunnel_token_file() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(".config").join("temote-mcp").join("tunnel-token"))
        .context("could not determine HOME for the default tunnel token file")
}

fn ensure_tunnel_token_file(path: &Path) -> Result<()> {
    let mut file = open_readonly_nofollow(path)
        .with_context(|| format!("cannot open tunnel token file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect tunnel token file {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "tunnel token file must be a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_TUNNEL_TOKEN_BYTES,
        "tunnel token file must contain 1..={MAX_TUNNEL_TOKEN_BYTES} bytes: {}",
        path.display()
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        anyhow::ensure!(
            private_unix_mode(mode),
            "tunnel token file must not be accessible by group or other users (mode {mode:04o}): {}",
            path.display()
        );
    }
    let mut probe = [0u8; 1];
    let read = file
        .read(&mut probe)
        .with_context(|| format!("tunnel token file is not readable: {}", path.display()))?;
    anyhow::ensure!(read == 1, "tunnel token file is empty: {}", path.display());
    Ok(())
}

fn open_readonly_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

#[cfg(unix)]
fn private_unix_mode(mode: u32) -> bool {
    mode & 0o077 == 0
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
fn read_pid_file(path: &Path) -> Result<i32> {
    let mut file = open_readonly_nofollow(path)
        .with_context(|| format!("cannot open PID file {}", path.display()))?;
    read_pid_from_open_file(&mut file, path)
}

fn read_pid_from_open_file(file: &mut std::fs::File, path: &Path) -> Result<i32> {
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect PID file {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "PID file is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_PID_FILE_BYTES as u64,
        "PID file exceeds {MAX_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_PID_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read PID file {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_PID_FILE_BYTES,
        "PID file exceeds {MAX_PID_FILE_BYTES} bytes: {}",
        path.display()
    );
    let raw = std::str::from_utf8(&bytes).context("PID file is not valid UTF-8")?;
    parse_pid(raw)
}

fn try_acquire_pid_lock(file: &std::fs::File) -> Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    let raw = error.raw_os_error();
    if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
        return Ok(false);
    }
    Err(error).context("failed to inspect PID file lock")
}

fn acquire_pid_lock(file: &std::fs::File) -> Result<()> {
    anyhow::ensure!(
        try_acquire_pid_lock(file)?,
        "PID file is already locked by another process"
    );
    Ok(())
}

fn parse_pid(raw: &str) -> Result<i32> {
    let pid = raw
        .trim()
        .parse::<i32>()
        .context("invalid temote-mcp PID file")?;
    anyhow::ensure!(pid > 0, "invalid temote-mcp PID file");
    Ok(pid)
}

fn is_temote_process(pid: i32) -> Result<bool> {
    Ok(process_name(pid)?.as_deref() == Some(PROCESS_NAME))
}

fn process_name(pid: i32) -> Result<Option<String>> {
    let output = Command::new(PS_COMMAND)
        .env_clear()
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .context("failed to inspect the temote-mcp process")?;
    if !output.status.success() {
        return Ok(None);
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let name = output.trim();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(Some(executable_name(name).to_owned()))
}

fn executable_name(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(command)
}

fn child_processes(parent_pid: i32) -> Vec<i32> {
    let Ok(output) = Command::new(PGREP_COMMAND)
        .env_clear()
        .args(["-P", &parent_pid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .filter(|pid| {
            process_name(*pid).ok().flatten().is_some_and(|name| {
                name == "cloudflared" || name == "tailscale" || name == "tunnel-client"
            })
        })
        .collect()
}

fn process_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_signal(pid: i32, signal: libc::c_int) -> Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to signal process {pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn parses_only_exact_legacy_pid_pairs() {
        assert_eq!(
            parse_legacy_up_pids("123 456\n").unwrap(),
            LegacyUpPids {
                serve: 123,
                tunnel: 456,
            }
        );
        for invalid in ["", "123", "123 456 789", "0 456", "123 -1", "abc 456"] {
            assert!(parse_legacy_up_pids(invalid).is_err(), "{invalid:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_pid_reader_rejects_symlink_and_oversize() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pids");
        let target = root.path().join("target");
        std::fs::write(&target, b"123 456\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::create_dir(&path).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(&path, vec![b'1'; MAX_LEGACY_PID_FILE_BYTES + 1]).unwrap();
        assert!(read_legacy_up_pids(&path).is_err());
        std::fs::write(&path, b"123 456\n").unwrap();
        assert_eq!(
            read_legacy_up_pids(&path).unwrap(),
            Some(LegacyUpPids {
                serve: 123,
                tunnel: 456,
            })
        );
    }

    #[test]
    fn process_name_guard_rejects_unexpected_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let result = ensure_process_name(pid, "cloudflared", "legacy Cloudflare Tunnel");
        assert!(result.is_err());
        assert!(process_exists(pid));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reads_small_regular_pid_files_and_rejects_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        std::fs::write(&path, b"123\n").unwrap();
        assert_eq!(read_pid_file(&path).unwrap(), 123);

        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_PID_FILE_BYTES as u64 + 1).unwrap();
        assert!(
            read_pid_file(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("PID file exceeds")
        );
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_rejects_symlink_and_malformed_existing_state_without_deleting_it() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let path = root.path().join("up.pid");
        std::fs::write(&target, b"123\n").unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_pid_file(&path).is_err());
        assert!(PidFile::create(&path).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, b"not-a-pid\n").unwrap();
        assert!(PidFile::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not-a-pid\n");
    }

    fn wait_for_pid_lock_release(file: &std::fs::File) -> Result<bool> {
        for _ in 0..50 {
            if try_acquire_pid_lock(file)? {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(false)
    }

    #[test]
    fn generated_pid_file_lock_tracks_owner_lifetime() -> noprop::TestResult {
        test_support::run(0x5049_444c_4f43_4b01, 128, |ctx| {
            let root = tempfile::tempdir().unwrap();
            let path = root
                .path()
                .join(format!("up-{:x}.pid", noprop::sample_u64(ctx)));
            let holder = PidFile::create(&path).unwrap();
            let probe = open_readonly_nofollow(&path).unwrap();
            let drop_before_probe = noprop::sample_bool(ctx);
            if drop_before_probe {
                drop(holder);
                assert!(
                    wait_for_pid_lock_release(&probe).unwrap(),
                    "released PID file lock remained busy"
                );
            } else {
                assert!(
                    !try_acquire_pid_lock(&probe).unwrap(),
                    "live PID file lock was unexpectedly acquirable"
                );
                drop(holder);
                assert!(
                    wait_for_pid_lock_release(&probe).unwrap(),
                    "PID file lock did not release after owner drop"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn unlocked_pid_file_is_stale_even_when_recorded_pid_is_live() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();

        let holder = PidFile::create(&path).unwrap();
        assert_eq!(read_pid_file(&path).unwrap(), std::process::id() as i32);
        drop(holder);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pid_file_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("up.pid");
        {
            let _pid_file = PidFile::create(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert_eq!(read_pid_file(&path).unwrap(), std::process::id() as i32);
        }
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_token_file_rejects_symlink_public_mode_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        std::fs::write(&path, b"token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_ok());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_TUNNEL_TOKEN_BYTES + 1).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
        std::fs::remove_file(&path).unwrap();

        let target = root.path().join("target-token");
        std::fs::write(&target, b"token").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(ensure_tunnel_token_file(&path).is_err());
    }

    #[test]
    fn generated_pid_file_byte_limits_match_reference_model() -> noprop::TestResult {
        test_support::run(0x5049_4442_4f55_4e44, 256, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=MAX_PID_FILE_BYTES + 8);
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("up.pid");
            std::fs::write(&path, vec![b'1'; len]).unwrap();
            let result = read_pid_file(&path);
            let syntactically_valid = len > 0
                && len <= 10
                && std::str::from_utf8(&vec![b'1'; len])
                    .ok()
                    .and_then(|raw| raw.parse::<i32>().ok())
                    .is_some_and(|pid| pid > 0);
            assert_eq!(
                result.is_ok(),
                len <= MAX_PID_FILE_BYTES && syntactically_valid,
                "len={len} result={result:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_private_file_modes_match_owner_only_reference() -> noprop::TestResult {
        test_support::run(0x4c49_4645_4d4f_4445, test_support::DEFAULT_CASES, |ctx| {
            let mode = u32::from(noprop::sample_u16(ctx)) & 0o777;
            assert_eq!(private_unix_mode(mode), mode & 0o077 == 0);
            Ok(())
        })
    }

    #[test]
    fn parses_only_positive_pids() {
        assert_eq!(parse_pid("123\n").unwrap(), 123);
        assert!(parse_pid("").is_err());
        assert!(parse_pid("0").is_err());
        assert!(parse_pid("-1").is_err());
    }

    #[test]
    fn process_inspection_uses_absolute_system_utilities() {
        assert!(Path::new(PS_COMMAND).is_absolute());
        assert!(Path::new(PGREP_COMMAND).is_absolute());
        #[cfg(target_os = "macos")]
        {
            assert_eq!(PS_COMMAND, "/bin/ps");
            assert_eq!(PGREP_COMMAND, "/usr/bin/pgrep");
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(PS_COMMAND, "/usr/bin/ps");
            assert_eq!(PGREP_COMMAND, "/usr/bin/pgrep");
        }
    }

    #[test]
    fn extracts_the_executable_name() {
        assert_eq!(executable_name("temote-mcp"), "temote-mcp");
        assert_eq!(executable_name("/usr/local/bin/temote-mcp"), "temote-mcp");
    }

    #[test]
    fn generated_pid_strings_match_positive_i32_model() -> noprop::TestResult {
        test_support::run(0x5049_4446_494c_4501, test_support::DEFAULT_CASES, |ctx| {
            let raw = match noprop::sample_usize_in(ctx, 0..=5) {
                0 => noprop::sample_u32(ctx).to_string(),
                1 => format!("-{}", noprop::sample_u32(ctx)),
                2 => "0".to_owned(),
                3 => format!(" {} \n", 1 + noprop::sample_u16(ctx)),
                4 => test_support::safe_component(ctx),
                _ => format!(
                    "{}{}",
                    u64::from(i32::MAX as u32) + 1,
                    noprop::sample_u16(ctx)
                ),
            };
            let expected = raw.trim().parse::<i32>().ok().filter(|pid| *pid > 0);
            assert_eq!(parse_pid(&raw).ok(), expected, "raw={raw:?}");
            Ok(())
        })
    }

    #[test]
    fn generated_executable_paths_return_last_component() -> noprop::TestResult {
        test_support::run(0x5052_4f43_4e41_4d45, 512, |ctx| {
            let executable = test_support::safe_component(ctx);
            let path = format!(
                "/{}/{}/{}",
                test_support::safe_component(ctx),
                test_support::safe_component(ctx),
                executable
            );
            assert_eq!(executable_name(&path), executable);
            assert_eq!(executable_name(&executable), executable);
            Ok(())
        })
    }
}
