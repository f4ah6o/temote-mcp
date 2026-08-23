use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;

const PID_FILE_NAME: &str = "up.pid";
const PROCESS_NAME: &str = env!("CARGO_PKG_NAME");
const MAX_PID_FILE_BYTES: usize = 64;
const MAX_TUNNEL_TOKEN_BYTES: u64 = 64 * 1024;

pub async fn up(
    public_url: Option<String>,
    addr: std::net::SocketAddr,
    tunnel_token_file: Option<PathBuf>,
) -> Result<()> {
    crate::load_public_env()?;

    let tunnel_token_file = tunnel_token_file
        .or_else(|| env_path("TUNNEL_TOKEN_FILE"))
        .unwrap_or(default_tunnel_token_file()?);
    ensure_tunnel_token_file(&tunnel_token_file)?;

    let pid_file = pid_file(true)?;
    let _pid_file = PidFile::create(&pid_file)?;
    crate::serve_http(public_url, addr, Some(&tunnel_token_file)).await
}

pub async fn down() -> Result<()> {
    let pid_file = pid_file(false)?;
    let pid = match read_pid_file(&pid_file) {
        Ok(pid) => pid,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            println!("no temote-mcp up process is recorded");
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", pid_file.display()));
        }
    };

    if !is_temote_process(pid)? {
        let _ = std::fs::remove_file(&pid_file);
        println!("recorded temote-mcp process is not running");
        return Ok(());
    }

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
}

impl PidFile {
    fn create(path: &Path) -> Result<Self> {
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    #[cfg(unix)]
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
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let pid = read_pid_file(path).with_context(|| {
                        format!("cannot safely inspect existing PID file {}", path.display())
                    })?;
                    if is_temote_process(pid)
                        .with_context(|| format!("cannot safely inspect recorded process {pid}"))?
                    {
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
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot read tunnel token file {}", path.display()))?;
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
    #[cfg(unix)]
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
    let read = std::fs::File::open(path)
        .with_context(|| format!("tunnel token file is not readable: {}", path.display()))?
        .read(&mut probe)
        .with_context(|| format!("tunnel token file is not readable: {}", path.display()))?;
    anyhow::ensure!(read == 1, "tunnel token file is empty: {}", path.display());
    Ok(())
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

fn read_pid_file(path: &Path) -> Result<i32> {
    let metadata = std::fs::symlink_metadata(path)
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
    std::fs::File::open(path)
        .with_context(|| format!("cannot open PID file {}", path.display()))?
        .take((MAX_PID_FILE_BYTES + 1) as u64)
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
    let output = Command::new("ps")
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
    let Ok(output) = Command::new("pgrep")
        .args(["-P", &parent_pid.to_string(), "-x", "cloudflared"])
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
