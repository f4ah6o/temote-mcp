use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;

const PID_FILE_NAME: &str = "up.pid";
const PROCESS_NAME: &str = env!("CARGO_PKG_NAME");

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
    let raw = match std::fs::read_to_string(&pid_file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no temote-mcp up process is recorded");
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", pid_file.display()));
        }
    };
    let pid = match parse_pid(&raw) {
        Ok(pid) => pid,
        Err(error) => {
            let _ = std::fs::remove_file(&pid_file);
            return Err(error).with_context(|| format!("invalid {}", pid_file.display()));
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
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("failed to sync {}", path.display()))?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                    let active = std::fs::read_to_string(path)
                        .ok()
                        .and_then(|raw| parse_pid(&raw).ok())
                        .is_some_and(|pid| is_temote_process(pid).unwrap_or(false));
                    if active {
                        anyhow::bail!("temote-mcp is already running; use temote-mcp down first");
                    }
                    let _ = std::fs::remove_file(path);
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
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cannot read tunnel token file {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() > 0,
        "tunnel token file is missing or empty: {}",
        path.display()
    );
    std::fs::File::open(path)
        .with_context(|| format!("tunnel token file is not readable: {}", path.display()))?;
    Ok(())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
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
