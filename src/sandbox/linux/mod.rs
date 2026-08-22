mod helper;
pub mod policy;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

use self::policy::LinuxSandboxPolicy;

const HELPER_BINARY_NAME: &str = "temote-linux-sandbox";

pub fn command(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let policy = LinuxSandboxPolicy::for_command(cwd, writable_roots, git_metadata_roots)?;
    let executable = helper_executable()?;
    let args = helper::command_args(&policy, command)?;
    let mut process = Command::new(executable);
    process.args(args);
    Ok(process)
}

pub fn run_main() -> ! {
    helper::run_main()
}

fn helper_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .context("temote-mcp executable has no parent directory")?;

    let candidates = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .map(|profile| {
                vec![
                    directory.join(HELPER_BINARY_NAME),
                    profile.join(HELPER_BINARY_NAME),
                ]
            })
            .unwrap_or_else(|| vec![directory.join(HELPER_BINARY_NAME)])
    } else {
        vec![directory.join(HELPER_BINARY_NAME)]
    };
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .with_context(|| {
            format!(
                "sandbox helper {HELPER_BINARY_NAME} is missing next to {}",
                executable.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_name_is_temote_specific() {
        assert_eq!(HELPER_BINARY_NAME, "temote-linux-sandbox");
    }
}
