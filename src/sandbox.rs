use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

pub const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let roots = writable_roots
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    let permissions = PermissionProfile::workspace_write_with(
        &roots,
        NetworkSandboxPolicy::Restricted,
        true,
        true,
    )
    .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);

    #[cfg(target_os = "linux")]
    let mut process = {
        let args =
            codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
                command.to_vec(),
                &cwd,
                &permissions,
                &cwd,
                false,
                false,
            );
        let executable = std::env::current_exe()?
            .parent()
            .context("local-mcp executable has no parent directory")?
            .join("codex-linux-sandbox");
        anyhow::ensure!(
            executable.is_file(),
            "sandbox helper is missing: {}",
            executable.display()
        );
        let mut process = Command::new(executable);
        process.args(args);
        process
    };

    #[cfg(target_os = "macos")]
    let mut process = {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args;

        let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
        let args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
            command: command.to_vec(),
            file_system_sandbox_policy: &file_system_policy,
            network_sandbox_policy: network_policy,
            sandbox_policy_cwd: &cwd,
            enforce_managed_network: false,
            network: None,
            extra_allow_unix_sockets: &[],
        });
        let mut process = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        process.args(args);
        process
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut process =
        { anyhow::bail!("sandboxed execution is currently implemented for Linux and macOS only") };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(safe_environment())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .context("failed to start sandboxed command")?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(bytes).await?;
    }
    wait_with_limited_output(child).await
}

pub async fn run_unrestricted(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .context("failed to start unsandboxed command")?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(bytes).await?;
    }
    wait_with_limited_output(child).await
}

async fn wait_with_limited_output(mut child: tokio::process::Child) -> Result<Output> {
    let stdout = child
        .stdout
        .take()
        .context("sandbox command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("sandbox command stderr was not captured")?;
    let remaining = Arc::new(AtomicUsize::new(MAX_COMMAND_OUTPUT_BYTES));
    let (stdout, stderr) = tokio::join!(
        read_limited(stdout, remaining.clone()),
        read_limited(stderr, remaining)
    );
    let (stdout, stdout_truncated) = stdout?;
    let (stderr, stderr_truncated) = stderr?;
    let status = child.wait().await?;
    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_limited<R>(mut reader: R, remaining: Arc<AtomicUsize>) -> Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    const CHUNK_SIZE: usize = 8192;
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let allowance = reserve_bytes(&remaining, CHUNK_SIZE);
        if allowance == 0 {
            let mut discard = [0_u8; CHUNK_SIZE];
            loop {
                let read = reader.read(&mut discard).await?;
                if read == 0 {
                    break;
                }
                truncated = true;
            }
            break;
        }

        let mut buffer = vec![0_u8; allowance];
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            remaining.fetch_add(allowance, Ordering::SeqCst);
            break;
        }
        if read < allowance {
            remaining.fetch_add(allowance - read, Ordering::SeqCst);
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

fn reserve_bytes(remaining: &AtomicUsize, maximum: usize) -> usize {
    let mut current = remaining.load(Ordering::SeqCst);
    loop {
        if current == 0 {
            return 0;
        }
        let reserved = current.min(maximum);
        match remaining.compare_exchange(
            current,
            current - reserved,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return reserved,
            Err(next) => current = next,
        }
    }
}

fn safe_environment() -> HashMap<String, String> {
    ["PATH", "LANG", "LC_ALL", "TERM", "TMPDIR"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod generic_tests {
    use super::*;

    #[tokio::test]
    async fn limits_a_stream_and_drains_the_rest() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let writer_task = tokio::spawn(async move {
            writer.write_all(b"0123456789abcdef").await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let remaining = Arc::new(AtomicUsize::new(8));
        let (output, truncated) = read_limited(reader, remaining).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(output, b"01234567");
        assert!(truncated);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-sandbox-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn seatbelt_allows_workspace_writes_and_denies_other_writes() -> Result<()> {
        // Nix's macOS build sandbox does not allow a nested Seatbelt profile.
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;

        let allowed = run(
            &["/usr/bin/touch".into(), "allowed".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert!(workspace.join("allowed").is_file());

        let denied_path = outside.join("denied");
        let denied = run(
            &[
                "/usr/bin/touch".into(),
                denied_path.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!denied_path.exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_network_access() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;
        let output = run(
            &[
                "/usr/bin/curl".into(),
                "--fail".into(),
                "--max-time".into(),
                "2".into(),
                "https://example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(output.status, 0);

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
