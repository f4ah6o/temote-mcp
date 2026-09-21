mod helper;
pub mod policy;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

use self::policy::{LinuxNetworkPolicy, LinuxSandboxPolicy};
use crate::sandbox::CommandNetworkPolicy;

const HELPER_BINARY_NAME: &str = "temote-linux-sandbox";

pub fn command(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    network: CommandNetworkPolicy,
) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let network = match network {
        CommandNetworkPolicy::Restricted => LinuxNetworkPolicy::Restricted,
        CommandNetworkPolicy::Development => LinuxNetworkPolicy::LocalAgent,
    };
    let policy = LinuxSandboxPolicy::for_command_with_network(
        cwd,
        writable_roots,
        git_metadata_roots,
        network,
    )?;
    let executable = helper_executable()?;
    let args = helper::command_args(&policy, command)?;
    let mut process = Command::new(executable);
    process.args(args);
    Ok(process)
}

pub fn git_worktree_add_command(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    git_metadata_roots: &[PathBuf],
    protected_worktree_roots: &[PathBuf],
) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let policy = LinuxSandboxPolicy::for_git_worktree_add(
        cwd,
        writable_roots,
        git_metadata_roots,
        protected_worktree_roots,
    )?;
    let executable = helper_executable()?;
    let args = helper::command_args(&policy, command)?;
    let mut process = Command::new(executable);
    process.args(args);
    Ok(process)
}

pub fn developer_tool_command(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    network_access: bool,
) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let policy = LinuxSandboxPolicy::for_developer_tool(cwd, writable_roots, network_access)?;
    let executable = helper_executable()?;
    let args = helper::command_args(&policy, command)?;
    let mut process = Command::new(executable);
    process.args(args);
    Ok(process)
}

pub fn local_agent_command(
    command: &[String],
    cwd: &Path,
    scope: &crate::sandbox::LocalAgentScope<'_>,
) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let policy = LinuxSandboxPolicy::for_local_agent(
        cwd,
        scope.writable_roots,
        scope.temporary_roots,
        scope.read_only_paths,
        scope.read_only_roots,
        scope.read_only_symlinks,
        scope.read_only_scaffold_directories,
        scope.read_only_files,
        scope.hidden_roots,
        scope.expected_repository,
    )?;
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
    let executable = match std::env::var_os("TEMOTE_MCP_INTERNAL_INSTALLED_LOCATOR") {
        Some(locator) => std::fs::canonicalize(locator)?,
        None => std::env::current_exe()?,
    };
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
