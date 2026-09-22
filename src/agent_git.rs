//! Local-agent `git` shim and the parent-side Git broker.
//!
//! The shim is the temote-mcp binary itself, exposed to a local agent as a
//! private `bin/git` symlink. Mutating commands (`switch`, `add`, `commit`) are
//! forwarded as one bounded JSON request through a private request/response
//! directory under the agent's own state root, and a broker runs for the
//! duration of `local_agent::run`. The broker revalidates every path, message,
//! and ref against the selected workspace on the parent side, enforces the
//! per-run `Access`, and fails closed with a fixed message for everything else.
//!
//! Bounded read-only commands (`status`, `diff`, `log`, `show`, `rev-parse`,
//! `ls-files`) run the trusted Git executable inside the same agent sandbox and
//! never enter the mutation broker.
//!
//! A directory transport is used instead of a Unix-domain socket on purpose:
//! the Linux local-agent seccomp profile denies `socket(AF_UNIX, ...)` so a
//! socket would be unreachable, and macOS `sun_path` is too short for the
//! private state root. File operations are already part of the agent profile.
//!
//! The queue directories are writable by the agent, so the broker treats every
//! entry as untrusted: directory handles are opened with `O_NOFOLLOW` and kept
//! for the broker lifetime, entries are opened with `openat(O_NOFOLLOW)` and
//! checked as bounded regular files, responses are published by atomic rename,
//! and enumeration/fan-out are capped.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::local_agent::Access;
use crate::{config, mcp, sandbox};

pub(crate) const BROKER_ENVIRONMENT_VARIABLE: &str = "TEMOTE_MCP_GIT_BROKER_DIR";
/// Parent-owned response queue exposed to the sandbox read-only. The agent can
/// read broker outcomes but can never author them.
pub(crate) const BROKER_RESPONSES_ENVIRONMENT_VARIABLE: &str =
    "TEMOTE_MCP_GIT_BROKER_RESPONSES_DIR";
pub(crate) const GIT_EXECUTABLE_ENVIRONMENT_VARIABLE: &str = "TEMOTE_MCP_GIT_EXECUTABLE";
pub(crate) const SHIM_EXIT_REJECTED: i32 = 128;
pub(crate) const SHIM_EXIT_INDETERMINATE: i32 = 70;
pub(crate) const SHIM_REJECTION_MESSAGE: &str = "git shim supports only: switch <existing-branch>, switch -c <new-branch>, add <path>..., commit -m <message>, worktree list [--porcelain], worktree add [-b <new-branch>] <broker-derived-managed-path> [<existing-branch>], worktree remove <broker-derived-managed-path>, fetch [--prune] [<configured-remote>], pull [--ff-only], push [-u] [<configured-remote>], and the read-only commands status, diff, log, show, rev-parse, ls-files";
pub(crate) const SHIM_INDETERMINATE_MESSAGE: &str = "git shim could not confirm the operation result; the operation may still be running; inspect the repository before retrying";
const BROKER_SCHEMA: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = sandbox::MAX_COMMAND_OUTPUT_BYTES * 8;
const REQUESTS_DIRECTORY: &str = "requests";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUEUE_ENTRIES_SCANNED: usize = 4096;
const MAX_REQUESTS_PER_TICK: usize = 16;
const MAX_READ_ONLY_PATHS: usize = 256;
const MAX_READ_ONLY_REVISION_BYTES: usize = 128;
const MAX_READ_ONLY_COUNT: u64 = 10_000;
const MAX_SHIM_WORKTREE_PATH_BYTES: usize = 4096;
const MAX_SHIM_POLICY_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GitShimRequest {
    schema: u32,
    cwd: PathBuf,
    argv: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShimCommand {
    SwitchExisting,
    SwitchCreate,
    Add,
    Commit,
    /// `worktree list [--porcelain]` — served from the managed inventory.
    WorktreeList,
    /// `worktree add` — the destination must be the broker-derived managed
    /// path; the caller never chooses filesystem placement.
    WorktreeAdd {
        branch: String,
        path: String,
        create_branch: bool,
    },
    /// `worktree remove` — delegated to the structured managed-worktree remove.
    WorktreeRemove {
        path: String,
    },
    /// `fetch [--prune] [<configured-remote>]`.
    Fetch {
        remote: Option<String>,
    },
    /// `pull [--ff-only]` for the current branch's configured upstream.
    Pull,
    /// `push [<configured-remote>]`, optionally setting the upstream.
    Push {
        remote: Option<String>,
        set_upstream: bool,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum ShimOutcome {
    Completed(ShimResult),
    Rejected,
    Indeterminate,
}

#[derive(Debug, Eq, PartialEq)]
struct ShimResult {
    status: i32,
    stdout: String,
    stderr: String,
}

/// Classifies the raw shim argv for the mutation broker.
///
/// Only `switch <branch>`, `switch -c|--create <branch>`, `add <path>...`, and
/// `commit -m|--message <message>` are accepted. Any global option, extra
/// argument, option-like path or branch, unsupported option, or other
/// subcommand is rejected here, before any Git process runs. Filesystem
/// containment for `add` paths is re-checked by the broker against the
/// selected workspace.
fn classify_argv(argv: &[String]) -> Result<ShimCommand> {
    match argv {
        [command, branch] if command.as_str() == "switch" => {
            validate_shim_branch(branch)?;
            Ok(ShimCommand::SwitchExisting)
        }
        [command, option, branch]
            if command.as_str() == "switch" && matches!(option.as_str(), "-c" | "--create") =>
        {
            validate_shim_branch(branch)?;
            Ok(ShimCommand::SwitchCreate)
        }
        [command, paths @ ..] if command.as_str() == "add" => {
            validate_shim_add_paths(paths)?;
            Ok(ShimCommand::Add)
        }
        [command, option, message]
            if command.as_str() == "commit" && matches!(option.as_str(), "-m" | "--message") =>
        {
            mcp::validate_git_commit_message(message)?;
            Ok(ShimCommand::Commit)
        }
        [command, subcommand, rest @ ..] if command.as_str() == "worktree" => {
            classify_worktree_argv(subcommand, rest)
        }
        [command, rest @ ..] if command.as_str() == "fetch" => classify_fetch(rest),
        [command, rest @ ..] if command.as_str() == "pull" => classify_pull(rest),
        [command, rest @ ..] if command.as_str() == "push" => classify_push(rest),
        _ => anyhow::bail!("unsupported Git shim command"),
    }
}

/// Classifies the bounded network allowlist.
///
/// Only the configured remote name (or its absence) may appear. URLs, refspecs,
/// `--force`, `-c`, hooks, filters and every other injection shape are rejected
/// here; the broker re-validates that the remote is actually configured and, for
/// GitHub HTTPS remotes, that the repository-local managed credential mapping is
/// present.
fn classify_fetch(rest: &[String]) -> Result<ShimCommand> {
    let mut remote = None;
    let mut seen_prune = false;
    for token in rest {
        if token == "--prune" && !seen_prune {
            seen_prune = true;
            continue;
        }
        if remote.is_none() {
            validate_shim_remote(token)?;
            remote = Some(token.clone());
            continue;
        }
        anyhow::bail!("unsupported git fetch arguments");
    }
    Ok(ShimCommand::Fetch { remote })
}

fn classify_pull(rest: &[String]) -> Result<ShimCommand> {
    for token in rest {
        anyhow::ensure!(
            token == "--ff-only",
            "unsupported git pull arguments; only the fast-forward-only form is available"
        );
    }
    Ok(ShimCommand::Pull)
}

fn classify_push(rest: &[String]) -> Result<ShimCommand> {
    let mut remote = None;
    let mut set_upstream = false;
    for token in rest {
        if matches!(token.as_str(), "-u" | "--set-upstream") && !set_upstream {
            set_upstream = true;
            continue;
        }
        if remote.is_none() {
            validate_shim_remote(token)?;
            remote = Some(token.clone());
            continue;
        }
        anyhow::bail!("unsupported git push arguments");
    }
    anyhow::ensure!(
        !set_upstream || remote.is_some(),
        "git push --set-upstream requires a configured remote name"
    );
    Ok(ShimCommand::Push {
        remote,
        set_upstream,
    })
}

/// Bounded syntax check for one remote argument. The broker still requires the
/// name to resolve to a configured remote; this never accepts a URL or refspec.
fn validate_shim_remote(remote: &str) -> Result<()> {
    anyhow::ensure!(!remote.is_empty(), "remote must not be empty");
    anyhow::ensure!(remote.len() <= 255, "remote must be at most 255 bytes");
    anyhow::ensure!(
        !remote.starts_with('-'),
        "remote must not look like a command option"
    );
    anyhow::ensure!(
        !remote.contains('@') && !remote.contains(':') && !remote.contains("//"),
        "remote must be a configured name, not a URL or refspec"
    );
    anyhow::ensure!(
        remote
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "remote must be a configured name, not a URL or refspec"
    );
    Ok(())
}

/// Classifies the narrow `worktree` allowlist.
///
/// Only `list [--porcelain]`, `add` with a branch (new via `-b`, or an existing
/// trailing branch) and `remove <path>` are accepted. The path is never
/// authoritative: the broker requires it to equal the Temote-derived managed
/// path before any Git metadata mutation. Force, `--detach`, `--lock`, prune,
/// move, repair and every other worktree shape stay rejected.
fn classify_worktree_argv(subcommand: &str, rest: &[String]) -> Result<ShimCommand> {
    match subcommand {
        "list" => {
            anyhow::ensure!(
                rest.is_empty() || (rest.len() == 1 && rest[0] == "--porcelain"),
                "unsupported git worktree list arguments"
            );
            Ok(ShimCommand::WorktreeList)
        }
        "add" => classify_worktree_add(rest),
        "remove" => {
            let [path] = rest else {
                anyhow::bail!("git worktree remove requires exactly one managed path")
            };
            validate_shim_worktree_path(path)?;
            Ok(ShimCommand::WorktreeRemove { path: path.clone() })
        }
        _ => anyhow::bail!("unsupported git worktree subcommand"),
    }
}

fn classify_worktree_add(rest: &[String]) -> Result<ShimCommand> {
    let (branch, path, create_branch) = match rest {
        [option, branch, path] if matches!(option.as_str(), "-b" | "--create") => {
            (branch, path, true)
        }
        [path, option, branch] if matches!(option.as_str(), "-b" | "--create") => {
            (branch, path, true)
        }
        [path, branch] => (branch, path, false),
        _ => anyhow::bail!(
            "git worktree add requires -b <new-branch> <path> or <path> <existing-branch>; the destination is always the Temote-derived managed path"
        ),
    };
    validate_shim_branch(branch)?;
    validate_shim_worktree_path(path)?;
    Ok(ShimCommand::WorktreeAdd {
        branch: branch.clone(),
        path: path.clone(),
        create_branch,
    })
}

/// Bounded syntax check for one worktree path argument. Authority is not
/// granted here: the broker re-resolves and requires the exact managed path.
fn validate_shim_worktree_path(path: &str) -> Result<()> {
    anyhow::ensure!(!path.is_empty(), "worktree path must not be empty");
    anyhow::ensure!(
        path.len() <= MAX_SHIM_WORKTREE_PATH_BYTES,
        "worktree path is too long"
    );
    anyhow::ensure!(
        !path.starts_with('-'),
        "worktree path must not look like a command option"
    );
    anyhow::ensure!(
        !path.chars().any(char::is_control),
        "worktree path must not contain control characters"
    );
    Ok(())
}

fn validate_shim_branch(branch: &str) -> Result<()> {
    anyhow::ensure!(!branch.is_empty(), "branch must not be empty");
    anyhow::ensure!(!branch.starts_with('-'), "branch must not start with '-'");
    Ok(())
}

fn validate_shim_add_paths(paths: &[String]) -> Result<()> {
    anyhow::ensure!(!paths.is_empty(), "Git shim add requires at least one path");
    anyhow::ensure!(
        paths.len() <= mcp::MAX_GIT_ADD_PATHS,
        "Git shim add supports at most {} paths",
        mcp::MAX_GIT_ADD_PATHS
    );
    for path in paths {
        validate_shim_add_path(path)?;
    }
    Ok(())
}

fn validate_shim_add_path(path: &str) -> Result<()> {
    mcp::validate_git_path_syntax(path)?;
    let candidate = Path::new(path);
    anyhow::ensure!(
        candidate.is_relative(),
        "Git shim add path must be relative: {path:?}"
    );
    let mut components = candidate.components().peekable();
    anyhow::ensure!(
        components.peek().is_some(),
        "Git shim add path must not be empty"
    );
    for component in components {
        anyhow::ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "Git shim add path must not contain '.' or '..' components: {path:?}"
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ReadOnlySpec {
    flags: &'static [&'static str],
    revisions: usize,
    paths: bool,
    counts: bool,
}

const READ_ONLY_STATUS: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--porcelain",
        "--porcelain=v1",
        "--porcelain=v2",
        "--short",
        "--branch",
        "--long",
        "--no-color",
        "--color=never",
        "--untracked-files=no",
        "--untracked-files=normal",
        "--untracked-files=all",
    ],
    revisions: 0,
    paths: false,
    counts: false,
};

const READ_ONLY_DIFF: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--stat",
        "--shortstat",
        "--name-only",
        "--name-status",
        "--numstat",
        "--cached",
        "--staged",
        "--no-color",
        "--color=never",
        "--no-renames",
        "--no-ext-diff",
    ],
    revisions: 2,
    paths: true,
    counts: false,
};

const READ_ONLY_LOG: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--oneline",
        "--stat",
        "--shortstat",
        "--name-only",
        "--name-status",
        "--decorate",
        "--no-decorate",
        "--graph",
        "--all",
        "--first-parent",
        "--no-color",
        "--color=never",
        "--no-renames",
        "--no-ext-diff",
    ],
    revisions: 2,
    paths: true,
    counts: true,
};

const READ_ONLY_SHOW: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--stat",
        "--shortstat",
        "--name-only",
        "--name-status",
        "--oneline",
        "--no-color",
        "--color=never",
        "--no-renames",
        "--no-ext-diff",
    ],
    revisions: 1,
    paths: true,
    counts: false,
};

const READ_ONLY_REV_PARSE: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--verify",
        "--short",
        "--abbrev-ref",
        "--symbolic",
        "--show-toplevel",
        "--show-prefix",
        "--is-inside-work-tree",
        "--is-bare-repository",
        "--git-dir",
        "--absolute-git-dir",
        "--show-cdup",
        "--end-of-options",
        "--no-color",
        "--color=never",
    ],
    revisions: 1,
    paths: false,
    counts: false,
};

const READ_ONLY_LS_FILES: ReadOnlySpec = ReadOnlySpec {
    flags: &[
        "--cached",
        "--deleted",
        "--modified",
        "--others",
        "--ignored",
        "--stage",
        "--unmerged",
        "--exclude-standard",
        "--no-empty-directory",
        "--directory",
        "--error-unmatch",
        "--full-name",
        "--no-color",
        "--color=never",
    ],
    revisions: 0,
    paths: true,
    counts: false,
};

fn read_only_spec(subcommand: &str) -> Result<ReadOnlySpec> {
    match subcommand {
        "status" => Ok(READ_ONLY_STATUS),
        "diff" => Ok(READ_ONLY_DIFF),
        "log" => Ok(READ_ONLY_LOG),
        "show" => Ok(READ_ONLY_SHOW),
        "rev-parse" => Ok(READ_ONLY_REV_PARSE),
        "ls-files" => Ok(READ_ONLY_LS_FILES),
        _ => anyhow::bail!("unsupported read-only Git shim command"),
    }
}

/// Validates one bounded read-only Git argv.
///
/// Only the six documented subcommands with the fixed flag sets above are
/// accepted. Revisions are restricted to plain local revision syntax, option
/// values must be bounded decimal counts, and paths after `--` must be
/// repository-relative without `.`/`..`, globs, or pathspec magic. `-c`,
/// `--config*`, `--git-dir`, `--work-tree`, `--exec-path`, `--output`,
/// `--ext-diff`, aliases, hooks, and every other shape are rejected.
fn validate_read_only_git_argv(argv: &[String]) -> Result<()> {
    let (subcommand, args) = argv
        .split_first()
        .context("Git shim read-only command is missing")?;
    let spec = read_only_spec(subcommand)?;
    let mut index = 0usize;
    let mut revisions = 0usize;
    let mut paths = 0usize;
    let mut after_separator = false;
    while index < args.len() {
        let token = args[index].as_str();
        if after_separator {
            validate_read_only_path(token)?;
            paths += 1;
            anyhow::ensure!(
                paths <= MAX_READ_ONLY_PATHS,
                "Git shim read-only path list exceeds the limit"
            );
            index += 1;
            continue;
        }
        if token == "--" && spec.paths {
            after_separator = true;
            index += 1;
            continue;
        }
        if token == "-n" && spec.counts {
            let value = args
                .get(index + 1)
                .context("Git shim -n requires a numeric count")?;
            validate_read_only_count(value)?;
            index += 2;
            continue;
        }
        if spec.counts
            && let Some(value) = token.strip_prefix("--max-count=")
        {
            validate_read_only_count(value)?;
            index += 1;
            continue;
        }
        if spec.flags.contains(&token) {
            index += 1;
            continue;
        }
        if !token.starts_with('-') && revisions < spec.revisions {
            validate_read_only_revision(token)?;
            revisions += 1;
            index += 1;
            continue;
        }
        anyhow::bail!("unsupported read-only Git shim argument");
    }
    Ok(())
}

fn validate_read_only_count(value: &str) -> Result<()> {
    let count: u64 = value
        .parse()
        .context("Git shim count must be a decimal number")?;
    anyhow::ensure!(
        (1..=MAX_READ_ONLY_COUNT).contains(&count),
        "Git shim count is outside the supported range"
    );
    Ok(())
}

fn validate_read_only_revision(revision: &str) -> Result<()> {
    anyhow::ensure!(!revision.is_empty(), "Git revision must not be empty");
    anyhow::ensure!(
        revision.len() <= MAX_READ_ONLY_REVISION_BYTES,
        "Git revision exceeds the size limit"
    );
    anyhow::ensure!(
        !revision.starts_with('-'),
        "Git revision must not start with '-'"
    );
    anyhow::ensure!(
        revision
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '.' | '_' | '/' | '~' | '^' | '-' | '{' | '}' | '@'
                )),
        "Git revision contains unsupported characters"
    );
    Ok(())
}

fn validate_read_only_path(path: &str) -> Result<()> {
    validate_shim_add_path(path).context("unsupported read-only Git path")
}

/// Runs one validated read-only Git command inside the current sandbox with a
/// sanitized environment. The trusted executable is selected by the parent;
/// no agent-provided environment can redirect Git state or execution.
fn run_read_only_git(git: &Path, argv: &[String], cwd: &Path) -> i32 {
    let mut command = Command::new(git);
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .args(argv)
        .current_dir(cwd);
    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str()
            && (name.starts_with("GIT_") || matches!(name, "PAGER" | "GIT_PAGER"))
        {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    match command.status() {
        Ok(status) => status.code().unwrap_or(SHIM_EXIT_REJECTED),
        Err(_) => reject(),
    }
}

/// Resolves the trusted Git executable from a pre-shim `PATH`, skipping the
/// private shim itself so read-only commands can never recurse.
pub(crate) fn resolve_trusted_git(path: Option<&str>, shim_target: &Path) -> Option<PathBuf> {
    let path = path?;
    for entry in std::env::split_paths(path) {
        let candidate = entry.join("git");
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        let Ok(canonical) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if canonical == shim_target {
            continue;
        }
        return Some(canonical);
    }
    None
}

/// Re-checks one classified `add` path on the parent side. The path must
/// resolve inside the selected workspace; the returned string is the
/// absolute path handed to Git, mirroring the structured `git_add` command
/// shape.
fn resolve_shim_add_path(workspace: &Path, cwd: &Path, path: &str) -> Result<String> {
    let candidate = cwd.join(path);
    let resolved = if candidate.exists() || std::fs::symlink_metadata(&candidate).is_ok() {
        std::fs::canonicalize(&candidate)
            .with_context(|| format!("cannot resolve Git shim add path {}", candidate.display()))?
    } else {
        let parent = candidate
            .parent()
            .context("Git shim add path has no parent directory")?;
        std::fs::canonicalize(parent)
            .with_context(|| format!("cannot resolve Git shim add path {}", candidate.display()))?
    };
    anyhow::ensure!(
        resolved == workspace || resolved.starts_with(workspace),
        "Git shim add path is outside the selected workspace: {}",
        candidate.display()
    );
    Ok(candidate.to_string_lossy().into_owned())
}

/// The fixed per-run Git operation scope: the canonical selected workspace and,
/// when that workspace is itself a Git worktree root, its pinned repository
/// identity. Request content never changes this scope.
///
/// When the caller supplies the expected identity that a managed-worktree
/// authority resolution already validated, the broker fails closed unless the
/// workspace presents exactly that identity at start. A freshly observed
/// different repository is never adopted as a new valid identity.
#[derive(Clone, Debug)]
struct BrokerScope {
    workspace: PathBuf,
    repository: Option<sandbox::WorkspaceRepositoryIdentity>,
}

impl BrokerScope {
    #[cfg(test)]
    fn for_workspace(workspace: &Path) -> Result<Self> {
        Self::for_workspace_with_expected(workspace, None)
    }

    fn for_workspace_with_expected(
        workspace: &Path,
        expected: Option<&sandbox::WorkspaceRepositoryIdentity>,
    ) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace).with_context(|| {
            format!(
                "cannot resolve Git broker workspace {}",
                workspace.display()
            )
        })?;
        anyhow::ensure!(
            workspace.is_dir(),
            "Git broker workspace is not a directory: {}",
            workspace.display()
        );
        let repository = match sandbox::WorkspaceRepositoryIdentity::for_workspace(&workspace) {
            Ok(identity) => Some(identity),
            Err(error) => {
                if expected.is_some() {
                    return Err(error).context(
                        "validated managed worktree identity is no longer a supported Git worktree root",
                    );
                }
                None
            }
        };
        if let Some(expected) = expected {
            let observed = repository
                .as_ref()
                .context("validated managed worktree identity is missing")?;
            anyhow::ensure!(
                observed == expected,
                "Git broker workspace managed worktree identity does not match the validated identity: {}",
                workspace.display()
            );
        }
        Ok(Self {
            workspace,
            repository,
        })
    }

    /// Resolves a request `cwd` against the fixed scope. The canonical target
    /// must be the selected workspace or a descendant, and its re-resolved
    /// repository identity must equal the identity captured at broker start.
    /// A different workspace, a symlinked target, a nested repository, or a
    /// swapped linked worktree fails closed.
    fn resolve_cwd(&self, requested: &Path) -> Result<PathBuf> {
        anyhow::ensure!(requested.is_absolute(), "Git broker cwd must be absolute");
        let canonical = std::fs::canonicalize(requested)
            .with_context(|| format!("cannot resolve Git broker cwd {}", requested.display()))?;
        anyhow::ensure!(
            canonical.is_dir(),
            "Git broker cwd is not a directory: {}",
            canonical.display()
        );
        anyhow::ensure!(
            canonical == self.workspace || canonical.starts_with(&self.workspace),
            "Git broker cwd is outside the selected workspace: {}",
            canonical.display()
        );
        let identity = self
            .repository
            .as_ref()
            .context("the selected workspace is not a Git worktree root")?;
        anyhow::ensure!(
            sandbox::git_worktree_root(&canonical)? == identity.worktree_root,
            "Git broker cwd belongs to a different repository than the selected workspace"
        );
        anyhow::ensure!(
            sandbox::git_metadata_roots(&canonical)? == identity.metadata_roots,
            "Git broker cwd resolves to different Git metadata than the selected workspace"
        );
        Ok(canonical)
    }
}

struct BrokerState {
    session: config::Session,
    access: Access,
    scope: BrokerScope,
    /// The configured managed-worktree `src` named root, resolved by the parent
    /// at broker start. `None` disables every managed-worktree shim form.
    src_root: Option<PathBuf>,
}

impl BrokerState {
    /// The session used for one structured managed-worktree operation.
    ///
    /// `cwd` is the already-validated broker request directory. Only the
    /// working directory changes; the permitted roots stay exactly the ones the
    /// parent authorized, so no request can widen filesystem scope.
    fn operation_session(&self, cwd: &Path) -> config::Session {
        let mut session = self.session.clone();
        session.cwd = cwd.to_path_buf();
        session
    }

    fn managed_src_root(&self) -> Result<PathBuf> {
        self.src_root
            .clone()
            .context("managed worktrees require the configured src named root in TEMOTE_MCP_ROOTS")
    }
}

/// Resolves the managed `src` root or returns the bounded shim outcome that the
/// agent should see. A missing named root never becomes a broker protocol error
/// so the agent receives a correctable policy message on stderr.
fn managed_src_root_or_output(
    state: &BrokerState,
) -> std::result::Result<PathBuf, sandbox::Output> {
    state
        .managed_src_root()
        .map_err(|error| structured_shim_output(Err(error)))
}

/// Renders one structured tool result as a bounded shim process outcome.
///
/// Structured failures keep their specific bounded policy message on stderr so
/// the agent can correct its intent, instead of the fixed generic rejection.
fn structured_shim_output(result: Result<Value>) -> sandbox::Output {
    match result {
        Ok(value) => sandbox::Output {
            status: 0,
            stdout: structured_result_text(&value),
            stderr: String::new(),
            truncated: false,
        },
        Err(error) => bounded_error_output(error),
    }
}

/// Renders one failed broker operation as a bounded shim process outcome with
/// the specific policy message on stderr.
fn bounded_error_output(error: anyhow::Error) -> sandbox::Output {
    let mut message = format!("{error:#}");
    if message.len() > MAX_SHIM_POLICY_MESSAGE_BYTES {
        let mut boundary = MAX_SHIM_POLICY_MESSAGE_BYTES;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
    }
    sandbox::Output {
        status: 1,
        stdout: String::new(),
        stderr: message,
        truncated: false,
    }
}

/// Relays one raw network Git process outcome, converting a validation failure
/// into the same bounded stderr shape.
fn shim_output_or_error(result: Result<sandbox::Output>) -> sandbox::Output {
    match result {
        Ok(output) => output,
        Err(error) => bounded_error_output(error),
    }
}

fn structured_result_text(value: &Value) -> String {
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Resolves one shim worktree path argument against the validated request
/// directory. The result is a lexical path; it never grants authority.
fn resolve_shim_worktree_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

async fn handle_request(state: &BrokerState, request: GitShimRequest) -> Result<sandbox::Output> {
    handle_request_inner(state, request, None).await
}

type OwnershipSnapshots<'a> = (
    &'a [crate::session_control::SessionView],
    &'a [(String, PathBuf)],
);

#[cfg(test)]
async fn handle_request_with_ownership_snapshots(
    state: &BrokerState,
    request: GitShimRequest,
    views: &[crate::session_control::SessionView],
    jobs: &[(String, PathBuf)],
) -> Result<sandbox::Output> {
    handle_request_inner(state, request, Some((views, jobs))).await
}

async fn handle_request_inner(
    state: &BrokerState,
    request: GitShimRequest,
    ownership_snapshots: Option<OwnershipSnapshots<'_>>,
) -> Result<sandbox::Output> {
    #[cfg(not(test))]
    let _ = ownership_snapshots;
    anyhow::ensure!(
        request.schema == BROKER_SCHEMA,
        "unsupported Git broker schema"
    );
    anyhow::ensure!(
        state.access == Access::WorkspaceWrite,
        "Git broker mutations require workspace_write access"
    );
    let cwd = state.scope.resolve_cwd(&request.cwd)?;
    let workspace = state.scope.workspace.clone();
    match classify_argv(&request.argv)? {
        ShimCommand::SwitchExisting => {
            mcp::validate_git_branch_name(&state.session, &cwd, &request.argv[1]).await?;
            mcp::ensure_local_branch_exists(&state.session, &cwd, &request.argv[1]).await?;
            let command = mcp::build_git_switch_command(&request.argv[1]);
            run_git_command(state, &workspace, command).await
        }
        ShimCommand::SwitchCreate => {
            mcp::validate_git_branch_name(&state.session, &cwd, &request.argv[2]).await?;
            mcp::ensure_local_branch_absent(&state.session, &cwd, &request.argv[2]).await?;
            let base = mcp::resolve_git_base_commit(&state.session, &cwd, "HEAD").await?;
            let create = mcp::build_git_branch_create_command(&request.argv[2], &base);
            let created = run_git_command(state, &workspace, create).await?;
            if created.status != 0 {
                return Ok(created);
            }
            let switch = mcp::build_git_switch_command(&request.argv[2]);
            run_git_command(state, &workspace, switch).await
        }
        ShimCommand::Add => {
            let mut command = vec![
                "git".to_owned(),
                "-c".to_owned(),
                "core.hooksPath=/dev/null".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
            ];
            for path in &request.argv[1..] {
                command.push(resolve_shim_add_path(&workspace, &cwd, path)?);
            }
            run_git_command(state, &workspace, command).await
        }
        ShimCommand::Commit => {
            let narrowed = narrowed_session(&state.session, &workspace);
            mcp::ensure_staged_paths_are_permitted(&narrowed, &workspace).await?;
            let command = mcp::build_git_commit_command(&request.argv[2]);
            run_git_command(state, &workspace, command).await
        }
        ShimCommand::WorktreeList => {
            let src_root = match managed_src_root_or_output(state) {
                Ok(src_root) => src_root,
                Err(output) => return Ok(output),
            };
            let operation = state.operation_session(&cwd);
            let result =
                mcp::git_worktree_list_with_src_root(&json!({}), &operation, Some(&src_root), None)
                    .await;
            Ok(structured_shim_output(result))
        }
        ShimCommand::WorktreeAdd {
            branch,
            path,
            create_branch,
        } => {
            let src_root = match managed_src_root_or_output(state) {
                Ok(src_root) => src_root,
                Err(output) => return Ok(output),
            };
            let operation = state.operation_session(&cwd);
            let result = async {
                let repository = mcp::managed_repository_for_requested(
                    None,
                    &operation,
                    &cwd,
                    &src_root,
                )?;
                let task = crate::managed_worktree::derive_task_name(&branch)?;
                let target = repository.target(&task)?;
                let requested = resolve_shim_worktree_path(&cwd, &path);
                anyhow::ensure!(
                    requested == target,
                    "Temote workspace policy: worktree destination is derived by Temote and must be {}; no worktree was created",
                    target.display()
                );
                if create_branch {
                    mcp::validate_git_branch_name(&operation, &cwd, &branch).await?;
                    mcp::ensure_local_branch_absent(&operation, &cwd, &branch).await?;
                    let base = mcp::resolve_git_base_commit(&operation, &cwd, "HEAD").await?;
                    let created = run_git_command(
                        state,
                        &workspace,
                        mcp::build_git_branch_create_command(&branch, &base),
                    )
                    .await?;
                    anyhow::ensure!(
                        created.status == 0,
                        "cannot create the managed worktree branch: {}",
                        created.stderr.trim()
                    );
                }
                let (_, value) = mcp::create_managed_worktree(
                    &operation,
                    &src_root,
                    &branch,
                    Some(&task),
                    None,
                    None,
                )
                .await?;
                Ok(value)
            }
            .await;
            Ok(structured_shim_output(result))
        }
        ShimCommand::WorktreeRemove { path } => {
            let src_root = match managed_src_root_or_output(state) {
                Ok(src_root) => src_root,
                Err(output) => return Ok(output),
            };
            let operation = state.operation_session(&cwd);
            let requested = resolve_shim_worktree_path(&cwd, &path);
            let args = json!({"path": requested.to_string_lossy()});
            #[cfg(test)]
            let result = match ownership_snapshots {
                Some((views, jobs)) => {
                    mcp::git_worktree_remove_in_src_root_with_snapshots(
                        &args, &operation, &src_root, None, views, jobs,
                    )
                    .await
                }
                None => {
                    mcp::git_worktree_remove_in_src_root(&args, &operation, &src_root, None).await
                }
            };
            #[cfg(not(test))]
            let result =
                mcp::git_worktree_remove_in_src_root(&args, &operation, &src_root, None).await;
            Ok(structured_shim_output(result))
        }
        ShimCommand::Fetch { remote } => {
            let operation = state.operation_session(&cwd);
            Ok(shim_output_or_error(
                mcp::git_fetch_output(&operation, cwd, remote, None).await,
            ))
        }
        ShimCommand::Pull => {
            let operation = state.operation_session(&cwd);
            Ok(shim_output_or_error(
                mcp::git_pull_output(&operation, cwd, None).await,
            ))
        }
        ShimCommand::Push {
            remote,
            set_upstream,
        } => {
            let operation = state.operation_session(&cwd);
            Ok(shim_output_or_error(
                mcp::git_push_output(&operation, cwd, remote, set_upstream, None).await,
            ))
        }
    }
}

fn narrowed_session(session: &config::Session, workspace: &Path) -> config::Session {
    let mut narrowed = session.clone();
    narrowed.cwd = workspace.to_owned();
    narrowed.permitted_directories = vec![workspace.to_owned()];
    narrowed
}

async fn run_git_command(
    state: &BrokerState,
    cwd: &Path,
    command: Vec<String>,
) -> Result<sandbox::Output> {
    if state.session.yolo() {
        return sandbox::run_unrestricted(&command, cwd, None).await;
    }
    let identity = state
        .scope
        .repository
        .as_ref()
        .context("the selected workspace has no pinned Git repository identity")?;
    if identity.linked_worktree() {
        // The selected workspace is a linked worktree whose validated metadata
        // lives below the primary checkout. `BrokerScope` pinned this exact
        // identity at broker start and `resolve_cwd` re-validated it for this
        // request, so the sandbox may authorize exactly those derived metadata
        // roots without widening the writable workspace scope.
        sandbox::run_git_with_pinned_worktree_metadata(
            &command,
            cwd,
            std::slice::from_ref(&identity.worktree_root),
            &identity.metadata_roots,
            None,
        )
        .await
    } else {
        sandbox::run_git(
            &command,
            cwd,
            std::slice::from_ref(&identity.worktree_root),
            &identity.metadata_roots,
            None,
        )
        .await
    }
}

/// The Git broker queue pair.
///
/// The request queue lives under the agent-writable state root. The response
/// queue lives in a separate parent-owned root that the sandbox exposes only as
/// a read-only visible root, so the agent can read broker outcomes but can
/// never author, replace, unlink, or rename one. Both roots are held open with
/// `O_NOFOLLOW` for the broker lifetime; relative entry operations go through
/// the held descriptors.
struct BrokerQueue {
    requests: std::fs::File,
    responses: std::fs::File,
    #[cfg(not(unix))]
    requests_path: PathBuf,
}

impl BrokerQueue {
    fn create(requests_root: &Path, responses_root: &Path) -> Result<Self> {
        anyhow::ensure!(
            requests_root.is_absolute() && responses_root.is_absolute(),
            "Git broker directories must be absolute paths"
        );
        anyhow::ensure!(
            requests_root != responses_root,
            "Git broker request and response roots must differ"
        );
        create_directory_private(requests_root)?;
        let requests_root_directory =
            open_directory_no_follow(requests_root).with_context(|| {
                format!(
                    "cannot open Git broker request directory {}",
                    requests_root.display()
                )
            })?;
        validate_queue_directory(requests_root, &requests_root_directory)?;
        let requests = create_subdirectory(&requests_root_directory, REQUESTS_DIRECTORY)?;
        create_directory_private(responses_root)?;
        let responses = open_directory_no_follow(responses_root).with_context(|| {
            format!(
                "cannot open Git broker response directory {}",
                responses_root.display()
            )
        })?;
        validate_queue_directory(responses_root, &responses)?;
        Ok(Self::from_handles(requests_root, requests, responses))
    }

    fn open_existing(requests_root: &Path, responses_root: &Path) -> Result<Self> {
        anyhow::ensure!(
            requests_root.is_absolute() && responses_root.is_absolute(),
            "Git broker directories must be absolute paths"
        );
        let requests_root_directory =
            open_directory_no_follow(requests_root).with_context(|| {
                format!(
                    "cannot open Git broker request directory {}",
                    requests_root.display()
                )
            })?;
        validate_queue_directory(requests_root, &requests_root_directory)?;
        let requests = open_subdirectory(&requests_root_directory, REQUESTS_DIRECTORY)?;
        let responses = open_directory_no_follow(responses_root).with_context(|| {
            format!(
                "cannot open Git broker response directory {}",
                responses_root.display()
            )
        })?;
        validate_queue_directory(responses_root, &responses)?;
        Ok(Self::from_handles(requests_root, requests, responses))
    }

    fn from_handles(
        requests_root: &Path,
        requests: std::fs::File,
        responses: std::fs::File,
    ) -> Self {
        #[cfg(unix)]
        let _ = requests_root;
        Self {
            requests,
            responses,
            #[cfg(not(unix))]
            requests_path: requests_root.join(REQUESTS_DIRECTORY),
        }
    }

    fn enumerate_requests(&self) -> Vec<String> {
        #[cfg(unix)]
        {
            enumerate_json_entries(&self.requests, MAX_QUEUE_ENTRIES_SCANNED)
        }
        #[cfg(not(unix))]
        {
            let mut names = Vec::new();
            let Ok(entries) = std::fs::read_dir(&self.requests_path) else {
                return names;
            };
            for entry in entries.flatten().take(MAX_QUEUE_ENTRIES_SCANNED) {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.ends_with(".json") {
                    names.push(name);
                }
            }
            names.sort();
            names
        }
    }

    fn requests_alive(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.requests
                .metadata()
                .map(|m| m.nlink() > 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            std::fs::symlink_metadata(&self.requests_path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
        }
    }

    /// Reads one request entry. Only a bounded regular file is accepted;
    /// symlinks, FIFOs, devices, directories, and oversized files are errors.
    fn read_request(&self, name: &str) -> Result<Vec<u8>> {
        read_entry_bounded(&self.requests, name, MAX_REQUEST_BYTES)
    }

    /// Reads one response entry, returning `Ok(None)` when it does not exist.
    fn read_response(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match read_entry_bounded(&self.responses, name, MAX_RESPONSE_BYTES) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error_is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Publishes a response through an exclusive temp entry and atomic rename.
    /// An existing final entry (including a planted symlink) is replaced, never
    /// followed.
    fn publish_response(&self, name: &str, payload: &[u8]) -> Result<()> {
        write_entry_atomic(&self.responses, name, payload)
    }

    fn remove_request(&self, name: &str) {
        remove_entry(&self.requests, name);
    }
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .map(|error| error.kind() == std::io::ErrorKind::NotFound)
        .unwrap_or(false)
}

/// A running Git broker queue. Dropping it aborts the serve loop and removes
/// both parent-owned queue roots.
pub(crate) struct GitBroker {
    requests_directory: PathBuf,
    responses_directory: PathBuf,
    task: JoinHandle<()>,
}

impl GitBroker {
    #[cfg(test)]
    pub(crate) fn start(
        requests_directory: PathBuf,
        responses_directory: PathBuf,
        session: config::Session,
        workspace: PathBuf,
        access: Access,
    ) -> Result<Self> {
        Self::start_with_expected(
            requests_directory,
            responses_directory,
            session,
            workspace,
            access,
            None,
            None,
        )
    }

    /// Starts the broker pinned to a repository identity that an earlier
    /// managed-worktree authority resolution already validated.
    ///
    /// The broker re-derives the workspace identity once at start and fails
    /// closed unless it equals `expected_identity`; it never adopts a
    /// different observed repository as a new valid identity. Every later
    /// request is re-validated against this pinned scope as before.
    ///
    /// `src_root` is the configured managed-worktree `src` named root resolved
    /// by the parent. `None` disables every managed-worktree shim form.
    pub(crate) fn start_with_expected(
        requests_directory: PathBuf,
        responses_directory: PathBuf,
        session: config::Session,
        workspace: PathBuf,
        access: Access,
        expected_identity: Option<&sandbox::WorkspaceRepositoryIdentity>,
        src_root: Option<PathBuf>,
    ) -> Result<Self> {
        let queue = BrokerQueue::create(&requests_directory, &responses_directory)?;
        let state = Arc::new(BrokerState {
            session,
            access,
            scope: BrokerScope::for_workspace_with_expected(&workspace, expected_identity)?,
            src_root,
        });
        let task = tokio::spawn(serve(queue, state));
        Ok(Self {
            requests_directory,
            responses_directory,
            task,
        })
    }
}

impl Drop for GitBroker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.requests_directory);
        let _ = std::fs::remove_dir_all(&self.responses_directory);
    }
}

async fn serve(queue: BrokerQueue, state: Arc<BrokerState>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        if !queue.requests_alive() {
            break;
        }
        let names = queue
            .enumerate_requests()
            .into_iter()
            .take(MAX_REQUESTS_PER_TICK)
            .collect::<Vec<_>>();
        for name in names {
            let id = name.trim_end_matches(".json").to_owned();
            if !valid_request_id(&id) {
                queue.remove_request(&name);
                continue;
            }
            let payload = match queue.read_request(&name) {
                Ok(bytes) => match serde_json::from_slice::<GitShimRequest>(&bytes) {
                    Ok(request) => match handle_request(&state, request).await {
                        Ok(output) => success_payload(&output),
                        Err(_) => error_payload(),
                    },
                    Err(_) => error_payload(),
                },
                Err(_) => error_payload(),
            };
            if payload.len() <= MAX_RESPONSE_BYTES {
                let _ = queue.publish_response(&format!("{id}.json"), &payload);
            }
            queue.remove_request(&name);
        }
    }
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn success_payload(output: &sandbox::Output) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": BROKER_SCHEMA,
        "status": output.status,
        "stdout": output.stdout,
        "stderr": output.stderr,
    }))
    .unwrap_or_else(|_| error_payload())
}

fn error_payload() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": BROKER_SCHEMA,
        "error": SHIM_REJECTION_MESSAGE,
    }))
    .unwrap_or_default()
}

/// Returns the shim exit status when the process was invoked as `git` with a
/// broker directory configured, and `None` for every normal CLI invocation.
pub(crate) fn maybe_run_as_git_shim() -> Option<i32> {
    let mut args = std::env::args();
    let argv0 = args.next()?;
    let basename = Path::new(&argv0).file_name()?.to_str()?;
    if basename != "git" || std::env::var_os(BROKER_ENVIRONMENT_VARIABLE).is_none() {
        return None;
    }
    Some(run_shim(args.collect()))
}

pub(crate) fn run_shim(argv: Vec<String>) -> i32 {
    let broker = std::env::var_os(BROKER_ENVIRONMENT_VARIABLE).map(PathBuf::from);
    let responses = std::env::var_os(BROKER_RESPONSES_ENVIRONMENT_VARIABLE).map(PathBuf::from);
    let git = std::env::var_os(GIT_EXECUTABLE_ENVIRONMENT_VARIABLE).map(PathBuf::from);
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return reject(),
    };
    run_shim_with(
        broker.as_deref(),
        responses.as_deref(),
        git.as_deref(),
        &cwd,
        &argv,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShimRoute {
    Mutation,
    ReadOnly,
    Reject,
}

/// Routes one shim argv without side effects. Mutation forms go to the broker,
/// the documented read-only forms go to the trusted Git executable, and
/// everything else is rejected. Mutations require both the writable request
/// queue and the read-only response queue.
fn shim_route(
    broker: Option<&Path>,
    responses: Option<&Path>,
    git: Option<&Path>,
    argv: &[String],
) -> ShimRoute {
    if classify_argv(argv).is_ok() {
        return if broker.is_some() && responses.is_some() {
            ShimRoute::Mutation
        } else {
            ShimRoute::Reject
        };
    }
    if validate_read_only_git_argv(argv).is_ok() {
        return if git.is_some() {
            ShimRoute::ReadOnly
        } else {
            ShimRoute::Reject
        };
    }
    ShimRoute::Reject
}

fn run_shim_with(
    broker: Option<&Path>,
    responses: Option<&Path>,
    git: Option<&Path>,
    cwd: &Path,
    argv: &[String],
) -> i32 {
    match shim_route(broker, responses, git, argv) {
        ShimRoute::Reject => reject(),
        ShimRoute::ReadOnly => {
            let Some(git) = git else {
                return reject();
            };
            run_read_only_git(git, argv, cwd)
        }
        ShimRoute::Mutation => {
            let (Some(requests_directory), Some(responses_directory)) = (broker, responses) else {
                return reject();
            };
            match request_broker_with_timeout(
                requests_directory,
                responses_directory,
                cwd,
                argv,
                REQUEST_TIMEOUT,
            ) {
                Ok(ShimOutcome::Completed(result)) => {
                    let mut stdout = std::io::stdout().lock();
                    let _ = stdout.write_all(result.stdout.as_bytes());
                    let _ = stdout.flush();
                    let mut stderr = std::io::stderr().lock();
                    let _ = stderr.write_all(result.stderr.as_bytes());
                    let _ = stderr.flush();
                    result.status
                }
                Ok(ShimOutcome::Rejected) | Err(_) => reject(),
                Ok(ShimOutcome::Indeterminate) => indeterminate(),
            }
        }
    }
}

fn reject() -> i32 {
    eprintln!("{SHIM_REJECTION_MESSAGE}");
    SHIM_EXIT_REJECTED
}

fn indeterminate() -> i32 {
    eprintln!("{SHIM_INDETERMINATE_MESSAGE}");
    SHIM_EXIT_INDETERMINATE
}

fn request_broker_with_timeout(
    requests_directory: &Path,
    responses_directory: &Path,
    cwd: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<ShimOutcome> {
    anyhow::ensure!(
        requests_directory.is_absolute() && responses_directory.is_absolute(),
        "Git broker directories must be absolute paths"
    );
    anyhow::ensure!(cwd.is_absolute(), "Git shim cwd must be absolute");
    let request = GitShimRequest {
        schema: BROKER_SCHEMA,
        cwd: cwd.to_owned(),
        argv: argv.to_vec(),
    };
    let encoded = serde_json::to_vec(&request).context("cannot encode the Git broker request")?;
    anyhow::ensure!(
        encoded.len() <= MAX_REQUEST_BYTES,
        "Git broker request exceeds the size limit"
    );

    let queue = BrokerQueue::open_existing(requests_directory, responses_directory)?;
    let id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let staged = format!("{id}.tmp");
    let queued = format!("{id}.json");
    write_entry_exclusive(&queue.requests, &staged, &encoded)
        .context("cannot stage the Git broker request")?;
    rename_entry(&queue.requests, &staged, &queued)
        .context("cannot queue the Git broker request")?;

    let response_name = queued.clone();
    let deadline = Instant::now() + timeout;
    loop {
        match queue.read_response(&response_name) {
            Ok(Some(bytes)) => {
                return match decode_response(&bytes) {
                    // Responses are parent-owned: the shim never deletes them,
                    // so its cleanup does not require agent write access.
                    Ok(DecodedResponse::Completed(result)) => {
                        queue.remove_request(&queued);
                        Ok(ShimOutcome::Completed(result))
                    }
                    Ok(DecodedResponse::Rejected) => {
                        queue.remove_request(&queued);
                        Ok(ShimOutcome::Rejected)
                    }
                    Err(_) => Ok(ShimOutcome::Indeterminate),
                };
            }
            Ok(None) => {}
            Err(_) => return Ok(ShimOutcome::Indeterminate),
        }
        if Instant::now() >= deadline {
            return Ok(ShimOutcome::Indeterminate);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

enum DecodedResponse {
    Completed(ShimResult),
    Rejected,
}

/// Strict response shapes. Only the exact broker-authored payloads decode; any
/// other document (including an `error` document with a different shape) is a
/// protocol error and never becomes `Rejected`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectedResponse {
    schema: u32,
    error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletedResponse {
    schema: u32,
    status: i64,
    stdout: String,
    stderr: String,
}

fn decode_response(bytes: &[u8]) -> Result<DecodedResponse> {
    let response: Value = serde_json::from_slice(bytes).context("invalid Git broker response")?;
    if let Ok(rejected) = serde_json::from_value::<RejectedResponse>(response.clone()) {
        anyhow::ensure!(
            rejected.schema == BROKER_SCHEMA && rejected.error == SHIM_REJECTION_MESSAGE,
            "unexpected Git broker error response"
        );
        return Ok(DecodedResponse::Rejected);
    }
    let completed: CompletedResponse =
        serde_json::from_value(response).context("invalid Git broker response")?;
    anyhow::ensure!(
        completed.schema == BROKER_SCHEMA,
        "unexpected Git broker schema"
    );
    let status = i32::try_from(completed.status).context("invalid Git broker status")?;
    Ok(DecodedResponse::Completed(ShimResult {
        status,
        stdout: completed.stdout,
        stderr: completed.stderr,
    }))
}

fn create_directory_private(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot create Git broker directory {}", path.display()));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot protect Git broker directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;

    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .context("Git broker directory path contains a NUL byte")?;
    let descriptor = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open Git broker directory {}", path.display()));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(not(unix))]
fn open_directory_no_follow(path: &Path) -> Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect Git broker directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Git broker path is not a real directory: {}",
        path.display()
    );
    std::fs::File::open(path)
        .with_context(|| format!("cannot open Git broker directory {}", path.display()))
}

fn validate_queue_directory(path: &Path, directory: &std::fs::File) -> Result<()> {
    let metadata = directory
        .metadata()
        .with_context(|| format!("cannot inspect Git broker directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "Git broker path is not a directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Git broker directory is not owned by the current user: {}",
            path.display()
        );
        anyhow::ensure!(
            metadata.mode() & 0o077 == 0,
            "Git broker directory is not private: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn create_subdirectory(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name).context("Git broker directory name contains a NUL")?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created != 0 {
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.kind() == std::io::ErrorKind::AlreadyExists,
            "cannot create Git broker queue directory: {error}"
        );
    }
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot open Git broker queue directory");
    }
    let directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    validate_queue_directory(Path::new(name.to_str().unwrap_or_default()), &directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
fn create_subdirectory(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    let _ = parent;
    anyhow::bail!("unsupported platform for Git broker queue: {name}")
}

#[cfg(unix)]
fn open_subdirectory(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name).context("Git broker directory name contains a NUL")?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot open Git broker queue directory");
    }
    let directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    validate_queue_directory(Path::new(name.to_str().unwrap_or_default()), &directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
fn open_subdirectory(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    let _ = parent;
    anyhow::bail!("unsupported platform for Git broker queue: {name}")
}

#[cfg(unix)]
fn read_entry_bounded(directory: &std::fs::File, name: &str, maximum: usize) -> Result<Vec<u8>> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name).context("Git broker entry name contains a NUL")?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot open Git broker entry");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata().context("cannot inspect Git broker entry")?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "Git broker entry is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= maximum as u64,
        "Git broker entry exceeds the size limit"
    );
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum));
    std::io::Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read Git broker entry")?;
    anyhow::ensure!(
        bytes.len() <= maximum,
        "Git broker entry exceeds the size limit"
    );
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_entry_bounded(directory: &std::fs::File, name: &str, maximum: usize) -> Result<Vec<u8>> {
    let path = path_for_entry(directory, name)?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect Git broker entry {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "Git broker entry is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= maximum as u64,
        "Git broker entry exceeds the size limit: {}",
        path.display()
    );
    let mut file = std::fs::File::open(&path)
        .with_context(|| format!("cannot open Git broker entry {}", path.display()))?;
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum));
    std::io::Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read Git broker entry")?;
    anyhow::ensure!(
        bytes.len() <= maximum,
        "Git broker entry exceeds the size limit"
    );
    Ok(bytes)
}

#[cfg(not(unix))]
fn path_for_entry(_directory: &std::fs::File, _name: &str) -> Result<PathBuf> {
    anyhow::bail!("unsupported platform for Git broker queue entries")
}

#[cfg(unix)]
fn write_entry_exclusive(directory: &std::fs::File, name: &str, payload: &[u8]) -> Result<()> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name).context("Git broker entry name contains a NUL")?;
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot create Git broker entry");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    file.write_all(payload)
        .context("cannot write Git broker entry")?;
    file.flush().context("cannot flush Git broker entry")?;
    Ok(())
}

#[cfg(not(unix))]
fn write_entry_exclusive(directory: &std::fs::File, name: &str, payload: &[u8]) -> Result<()> {
    let _ = directory;
    let _ = name;
    let _ = payload;
    anyhow::bail!("unsupported platform for Git broker queue entries")
}

#[cfg(unix)]
fn write_entry_atomic(directory: &std::fs::File, name: &str, payload: &[u8]) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_name = format!(".{name}.{}-{nonce}.tmp", std::process::id());
    write_entry_exclusive(directory, &temp_name, payload)?;
    let temp = std::ffi::CString::new(temp_name.clone()).context("invalid Git broker temp name")?;
    let final_name = std::ffi::CString::new(name).context("invalid Git broker entry name")?;
    let renamed = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            directory.as_raw_fd(),
            final_name.as_ptr(),
        )
    };
    if renamed != 0 {
        let error = std::io::Error::last_os_error();
        remove_entry(directory, &temp_name);
        return Err(error).context("cannot publish Git broker entry");
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_entry_atomic(directory: &std::fs::File, name: &str, payload: &[u8]) -> Result<()> {
    let _ = directory;
    let _ = name;
    let _ = payload;
    anyhow::bail!("unsupported platform for Git broker queue entries")
}

#[cfg(unix)]
fn rename_entry(directory: &std::fs::File, from: &str, to: &str) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let from = std::ffi::CString::new(from).context("invalid Git broker entry name")?;
    let to = std::ffi::CString::new(to).context("invalid Git broker entry name")?;
    let renamed = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if renamed != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot rename Git broker entry");
    }
    Ok(())
}

#[cfg(not(unix))]
fn rename_entry(directory: &std::fs::File, from: &str, to: &str) -> Result<()> {
    let _ = directory;
    let _ = from;
    let _ = to;
    anyhow::bail!("unsupported platform for Git broker queue entries")
}

#[cfg(unix)]
fn remove_entry(directory: &std::fs::File, name: &str) {
    use std::os::unix::io::AsRawFd;

    let Ok(name) = std::ffi::CString::new(name) else {
        return;
    };
    unsafe {
        libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0);
    }
}

#[cfg(not(unix))]
fn remove_entry(directory: &std::fs::File, name: &str) {
    let _ = directory;
    let _ = name;
}

#[cfg(unix)]
fn enumerate_json_entries(directory: &std::fs::File, maximum: usize) -> Vec<String> {
    use std::os::unix::io::AsRawFd;

    let mut names = Vec::new();
    // Open a fresh descriptor for the directory. `dup` would share the file
    // offset with the held descriptor, so the second enumeration would start
    // at the previous end and miss every new entry.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return names;
    }
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        unsafe {
            libc::close(descriptor);
        }
        return names;
    }
    let mut scanned = 0usize;
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        scanned += 1;
        if scanned > maximum {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let Ok(name) = name.to_str() else {
            continue;
        };
        if name.ends_with(".json") && name != "." && name != ".." {
            names.push(name.to_owned());
        }
    }
    unsafe {
        libc::closedir(stream);
    }
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn request(cwd: &Path, values: &[&str]) -> GitShimRequest {
        GitShimRequest {
            schema: BROKER_SCHEMA,
            cwd: cwd.to_owned(),
            argv: argv(values),
        }
    }

    fn session(repository: &Path) -> config::Session {
        config::Session {
            id: "git-broker-test".to_owned(),
            cwd: repository.to_owned(),
            permitted_directories: vec![repository.to_owned()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Yolo,
        }
    }

    fn state(repository: &Path, access: Access) -> BrokerState {
        BrokerState {
            session: session(repository),
            access,
            scope: BrokerScope::for_workspace(repository).unwrap(),
            src_root: None,
        }
    }

    fn state_with_src_root(repository: &Path, src_root: &Path) -> BrokerState {
        BrokerState {
            session: session(repository),
            access: Access::WorkspaceWrite,
            scope: BrokerScope::for_workspace(repository).unwrap(),
            src_root: Some(src_root.to_path_buf()),
        }
    }

    fn run_host_git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Temote Test",
                "-c",
                "user.email=temote-test@example.invalid",
            ])
            .args(args)
            .current_dir(repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("host git must be installed for this test");
        assert!(
            output.status.success(),
            "host git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn broker_roots(root: &Path) -> (PathBuf, PathBuf) {
        (root.join("queue"), root.join("responses"))
    }

    fn init_repository(repository: &Path) {
        run_host_git(repository, &["init", "--quiet"]);
        run_host_git(repository, &["config", "user.name", "Temote Test"]);
        run_host_git(
            repository,
            &["config", "user.email", "temote-test@example.invalid"],
        );
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        run_host_git(repository, &["add", "tracked.txt"]);
        run_host_git(repository, &["commit", "--quiet", "-m", "initial"]);
        run_host_git(repository, &["branch", "-M", "main"]);
    }

    #[test]
    fn worktree_classifier_accepts_only_the_managed_allowlist() {
        let managed = "/src/worktrees/repo/feat-x";
        assert_eq!(
            classify_argv(&argv(&["worktree", "list"])).unwrap(),
            ShimCommand::WorktreeList
        );
        assert_eq!(
            classify_argv(&argv(&["worktree", "list", "--porcelain"])).unwrap(),
            ShimCommand::WorktreeList
        );
        assert_eq!(
            classify_argv(&argv(&["worktree", "add", "-b", "feat/x", managed])).unwrap(),
            ShimCommand::WorktreeAdd {
                branch: "feat/x".to_owned(),
                path: managed.to_owned(),
                create_branch: true,
            }
        );
        assert_eq!(
            classify_argv(&argv(&["worktree", "add", managed, "-b", "feat/x"])).unwrap(),
            ShimCommand::WorktreeAdd {
                branch: "feat/x".to_owned(),
                path: managed.to_owned(),
                create_branch: true,
            }
        );
        assert_eq!(
            classify_argv(&argv(&["worktree", "add", managed, "existing"])).unwrap(),
            ShimCommand::WorktreeAdd {
                branch: "existing".to_owned(),
                path: managed.to_owned(),
                create_branch: false,
            }
        );
        assert_eq!(
            classify_argv(&argv(&["worktree", "remove", managed])).unwrap(),
            ShimCommand::WorktreeRemove {
                path: managed.to_owned()
            }
        );

        for values in [
            vec!["worktree"],
            vec!["worktree", "prune"],
            vec!["worktree", "move", "a", "b"],
            vec!["worktree", "repair"],
            vec!["worktree", "lock", managed],
            vec!["worktree", "unlock", managed],
            vec!["worktree", "list", "--verbose"],
            vec!["worktree", "list", "extra"],
            vec!["worktree", "add"],
            vec!["worktree", "add", managed],
            vec!["worktree", "add", "-b", "feat/x"],
            vec!["worktree", "add", "--detach", managed],
            vec!["worktree", "add", "--force", managed, "existing"],
            vec!["worktree", "add", "-b", "feat/x", managed, "extra"],
            vec!["worktree", "add", "-b", "-x", managed],
            vec!["worktree", "add", "-b", "feat/x", "-x"],
            vec!["worktree", "remove"],
            vec!["worktree", "remove", "--force", managed],
            vec!["worktree", "remove", managed, "extra"],
            vec!["worktree", "remove", "-x"],
            vec!["worktree", "add", "-b", "feat/x", ""],
            vec!["worktree", "add", "-b", "feat/x", "a\nb"],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
        // The path is never authoritative and never becomes a Git option.
        let overly_long = format!("/src/{}", "a".repeat(MAX_SHIM_WORKTREE_PATH_BYTES));
        assert!(classify_argv(&argv(&["worktree", "remove", &overly_long])).is_err());
    }

    fn managed_shim_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let repository = src_root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_repository(&repository);
        let repository = std::fs::canonicalize(&repository).unwrap();
        let managed_root = src_root.join("worktrees").join("repo");
        std::fs::create_dir_all(&managed_root).unwrap();
        let legacy = repository.join(".wt").join("legacy");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        run_host_git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                legacy.to_str().unwrap(),
                "-b",
                "legacy-branch",
            ],
        );
        (fixture, src_root, repository, managed_root)
    }

    #[tokio::test]
    async fn shim_worktree_forms_delegate_to_the_managed_broker() {
        let (_fixture, src_root, repository, managed_root) = managed_shim_fixture();
        let state = state_with_src_root(&repository, &src_root);
        let legacy = repository.join(".wt").join("legacy");
        let legacy_before = repository_snapshot(&legacy);

        // list: the managed inventory, without touching any worktree.
        let listed = handle_request(&state, request(&repository, &["worktree", "list"]))
            .await
            .unwrap();
        assert_eq!(listed.status, 0, "{}", listed.stderr);
        let inventory: Value = serde_json::from_str(&listed.stdout).unwrap();
        assert_eq!(inventory["repository"], "repo");
        assert_eq!(
            inventory["managed_root"],
            managed_root.to_string_lossy().as_ref()
        );

        // add: the destination must be the broker-derived managed path.
        let target = managed_root.join("feat-task");
        let created = handle_request(
            &state,
            request(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feat/task",
                    target.to_str().unwrap(),
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);
        assert_eq!(
            run_host_git(&target, &["branch", "--show-current"]),
            "feat/task"
        );

        // add: an arbitrary destination is rejected before any mutation.
        let escape = src_root.join("escape");
        let rejected = handle_request(
            &state,
            request(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feat/escape",
                    escape.to_str().unwrap(),
                ],
            ),
        )
        .await
        .unwrap();
        assert_ne!(rejected.status, 0);
        assert!(
            rejected.stderr.contains("Temote workspace policy"),
            "{}",
            rejected.stderr
        );
        assert!(!escape.exists());
        assert!(run_host_git(&repository, &["branch", "--list", "feat/escape"]).is_empty());

        // add: a managed-root path whose component does not match the branch
        // is rejected too.
        let mismatched = managed_root.join("other-task");
        let rejected = handle_request(
            &state,
            request(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feat/task2",
                    mismatched.to_str().unwrap(),
                ],
            ),
        )
        .await
        .unwrap();
        assert_ne!(rejected.status, 0);
        assert!(!mismatched.exists());

        // remove: delegated to the structured managed remove.
        let removed = handle_request_with_ownership_snapshots(
            &state,
            request(
                &repository,
                &["worktree", "remove", target.to_str().unwrap()],
            ),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(removed.status, 0, "{}", removed.stderr);
        assert!(!target.exists());
        assert!(
            !run_host_git(&repository, &["branch", "--list", "feat/task"]).is_empty(),
            "the branch ref must survive the shim worktree remove"
        );

        // Legacy and out-of-root worktrees are never adopted or removed.
        let rejected = handle_request_with_ownership_snapshots(
            &state,
            request(
                &repository,
                &["worktree", "remove", legacy.to_str().unwrap()],
            ),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_ne!(rejected.status, 0);
        assert_eq!(repository_snapshot(&legacy), legacy_before);
        assert!(legacy.join("tracked.txt").exists());
    }

    #[tokio::test]
    async fn shim_worktree_remove_rejects_dirty_worktrees_and_missing_src_authority() {
        let (_fixture, src_root, repository, managed_root) = managed_shim_fixture();
        let broker_state = state_with_src_root(&repository, &src_root);
        let target = managed_root.join("dirty-task");
        let created = handle_request(
            &broker_state,
            request(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "dirty/task",
                    target.to_str().unwrap(),
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);
        std::fs::write(target.join("unrelated.txt"), "keep\n").unwrap();

        let rejected = handle_request_with_ownership_snapshots(
            &broker_state,
            request(
                &repository,
                &["worktree", "remove", target.to_str().unwrap()],
            ),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_ne!(rejected.status, 0);
        assert!(
            rejected.stderr.contains("modified or untracked"),
            "{}",
            rejected.stderr
        );
        assert_eq!(
            std::fs::read_to_string(target.join("unrelated.txt")).unwrap(),
            "keep\n"
        );

        // Without the configured src authority every managed form fails closed.
        let unconfigured = state(&repository, Access::WorkspaceWrite);
        for values in [
            vec!["worktree", "list"],
            vec!["worktree", "add", "-b", "feat/x", "/tmp/x"],
            vec!["worktree", "remove", target.to_str().unwrap()],
        ] {
            let rejected = handle_request(&unconfigured, request(&repository, &values))
                .await
                .unwrap();
            assert_ne!(rejected.status, 0, "{values:?}");
            assert!(
                rejected.stderr.contains("src named root"),
                "{values:?}: {}",
                rejected.stderr
            );
        }
        assert!(target.exists());
    }

    #[test]
    fn network_classifier_accepts_only_configured_remote_forms() {
        assert_eq!(
            classify_argv(&argv(&["fetch"])).unwrap(),
            ShimCommand::Fetch { remote: None }
        );
        assert_eq!(
            classify_argv(&argv(&["fetch", "--prune"])).unwrap(),
            ShimCommand::Fetch { remote: None }
        );
        assert_eq!(
            classify_argv(&argv(&["fetch", "--prune", "origin"])).unwrap(),
            ShimCommand::Fetch {
                remote: Some("origin".to_owned())
            }
        );
        assert_eq!(
            classify_argv(&argv(&["fetch", "upstream"])).unwrap(),
            ShimCommand::Fetch {
                remote: Some("upstream".to_owned())
            }
        );
        assert_eq!(classify_argv(&argv(&["pull"])).unwrap(), ShimCommand::Pull);
        assert_eq!(
            classify_argv(&argv(&["pull", "--ff-only"])).unwrap(),
            ShimCommand::Pull
        );
        assert_eq!(
            classify_argv(&argv(&["push"])).unwrap(),
            ShimCommand::Push {
                remote: None,
                set_upstream: false
            }
        );
        assert_eq!(
            classify_argv(&argv(&["push", "origin"])).unwrap(),
            ShimCommand::Push {
                remote: Some("origin".to_owned()),
                set_upstream: false
            }
        );
        assert_eq!(
            classify_argv(&argv(&["push", "-u", "origin"])).unwrap(),
            ShimCommand::Push {
                remote: Some("origin".to_owned()),
                set_upstream: true
            }
        );
        assert_eq!(
            classify_argv(&argv(&["push", "--set-upstream", "origin"])).unwrap(),
            ShimCommand::Push {
                remote: Some("origin".to_owned()),
                set_upstream: true
            }
        );

        for values in [
            vec!["fetch", "--all"],
            vec!["fetch", "--tags"],
            vec!["fetch", "--prune", "--prune"],
            vec!["fetch", "https://github.com/example/repo.git"],
            vec!["fetch", "git@github.com:example/repo.git"],
            vec!["fetch", "origin", "main"],
            vec!["fetch", "origin", "refs/heads/main:refs/heads/main"],
            vec!["fetch", "--depth", "1"],
            vec!["fetch", "-c", "core.hooksPath=/tmp"],
            vec!["pull", "--rebase"],
            vec!["pull", "--no-ff"],
            vec!["pull", "origin"],
            vec!["pull", "--ff-only", "--ff-only", "extra"],
            vec!["push", "--force"],
            vec!["push", "-f"],
            vec!["push", "--force-with-lease"],
            vec!["push", "--all"],
            vec!["push", "--tags"],
            vec!["push", "--delete", "origin", "branch"],
            vec!["push", "origin", "main"],
            vec!["push", "origin", "HEAD:refs/heads/other"],
            vec!["push", "-u"],
            vec!["push", "--set-upstream"],
            vec!["push", "-u", "origin", "extra"],
            vec!["push", "--mirror"],
            vec!["push", "-c", "push.default=matching"],
            vec!["push", "https://github.com/example/repo.git"],
            vec!["fetch", "-origin"],
            vec!["push", "-u", "-origin"],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
    }

    fn network_shim_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let src_root = std::fs::canonicalize(fixture.path()).unwrap();
        let remote = src_root.join("remote.git");
        run_host_git(
            &src_root,
            &["init", "--quiet", "--bare", remote.to_str().unwrap()],
        );
        let repository = src_root.join("repo");
        std::fs::create_dir(&repository).unwrap();
        init_repository(&repository);
        run_host_git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_host_git(&repository, &["push", "--quiet", "-u", "origin", "main"]);
        run_host_git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let repository = std::fs::canonicalize(&repository).unwrap();
        (fixture, src_root, repository, remote)
    }

    #[tokio::test]
    async fn shim_network_forms_use_the_configured_remote_contract() {
        let (_fixture, src_root, repository, remote) = network_shim_fixture();
        let broker_state = state_with_src_root(&repository, &src_root);
        let state = BrokerState {
            src_root: None,
            ..broker_state
        };

        // push publishes only the current branch.
        std::fs::write(repository.join("tracked.txt"), "local-push\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "local push"]);
        let pushed = handle_request(&state, request(&repository, &["push"]))
            .await
            .unwrap();
        assert_eq!(pushed.status, 0, "{}", pushed.stderr);
        assert_eq!(
            run_host_git(&remote, &["rev-parse", "main"]),
            run_host_git(&repository, &["rev-parse", "HEAD"])
        );

        // A new branch can set its upstream through ordinary syntax.
        run_host_git(&repository, &["switch", "--quiet", "-c", "feature/network"]);
        std::fs::write(repository.join("feature.txt"), "feature\n").unwrap();
        run_host_git(&repository, &["add", "feature.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "feature"]);
        let pushed = handle_request(&state, request(&repository, &["push", "-u", "origin"]))
            .await
            .unwrap();
        assert_eq!(pushed.status, 0, "{}", pushed.stderr);
        assert_eq!(
            run_host_git(
                &repository,
                &["rev-parse", "--abbrev-ref", "feature/network@{upstream}"]
            ),
            "origin/feature/network"
        );
        run_host_git(&repository, &["switch", "--quiet", "main"]);

        // fetch --prune observes a concurrent update without changing local work.
        let other = src_root.join("other");
        run_host_git(
            &src_root,
            &[
                "clone",
                "--quiet",
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        std::fs::write(other.join("remote-only.txt"), "remote-update\n").unwrap();
        run_host_git(&other, &["add", "remote-only.txt"]);
        run_host_git(&other, &["commit", "--quiet", "-m", "remote update"]);
        run_host_git(&other, &["push", "--quiet", "origin", "main"]);
        let remote_tip = run_host_git(&other, &["rev-parse", "HEAD"]);
        std::fs::write(repository.join("tracked.txt"), "local-dirty\n").unwrap();

        let fetched = handle_request(&state, request(&repository, &["fetch", "--prune"]))
            .await
            .unwrap();
        assert_eq!(fetched.status, 0, "{}", fetched.stderr);
        assert_eq!(
            run_host_git(&repository, &["rev-parse", "refs/remotes/origin/main"]),
            remote_tip
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt")).unwrap(),
            "local-dirty\n",
            "fetch must never touch the working tree"
        );

        // An unconfigured remote name fails closed with the configured-remote
        // contract and no network access.
        let rejected = handle_request(
            &state,
            request(&repository, &["fetch", "--prune", "upstream"]),
        )
        .await
        .unwrap();
        assert_ne!(rejected.status, 0);
        assert!(
            rejected.stderr.contains("not configured"),
            "{}",
            rejected.stderr
        );

        // pull --ff-only fast-forwards the current branch from its upstream
        // while the local dirty file stays untouched.
        let pulled = handle_request(&state, request(&repository, &["pull", "--ff-only"]))
            .await
            .unwrap();
        assert_eq!(pulled.status, 0, "{}", pulled.stderr);
        assert_eq!(
            run_host_git(&repository, &["rev-parse", "HEAD"]),
            remote_tip
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt")).unwrap(),
            "local-dirty\n"
        );
    }

    #[test]
    fn network_command_shapes_never_carry_force_refspecs_or_urls() {
        let fetch = mcp::build_git_fetch_command("origin");
        assert_eq!(fetch.last().map(String::as_str), Some("origin"));
        assert!(fetch.iter().any(|token| token == "--prune"));
        let pull = mcp::build_git_pull_command();
        assert!(pull.iter().any(|token| token == "--ff-only"));
        let push = mcp::build_git_push_command(Some("origin".to_owned()), false);
        assert_eq!(
            push.iter().rev().take(2).cloned().collect::<Vec<_>>(),
            vec!["HEAD".to_owned(), "origin".to_owned()]
        );
        let push_upstream = mcp::build_git_push_command(Some("origin".to_owned()), true);
        assert!(push_upstream.iter().any(|token| token == "--set-upstream"));
        for command in [&fetch, &pull, &push, &push_upstream] {
            let rendered = command.join(" ");
            assert!(!rendered.contains("--force"), "{rendered}");
            assert!(!rendered.contains("--all"), "{rendered}");
            assert!(!rendered.contains("--tags"), "{rendered}");
            assert!(!rendered.contains("https://"), "{rendered}");
            assert!(!rendered.contains("refs/heads"), "{rendered}");
            assert!(!rendered.contains("--delete"), "{rendered}");
        }
    }

    #[test]
    fn classifier_accepts_only_the_bounded_switch_add_and_commit_forms() {
        assert_eq!(
            classify_argv(&argv(&["switch", "main"])).unwrap(),
            ShimCommand::SwitchExisting
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "feature/x"])).unwrap(),
            ShimCommand::SwitchExisting
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "-c", "feature/x"])).unwrap(),
            ShimCommand::SwitchCreate
        );
        assert_eq!(
            classify_argv(&argv(&["switch", "--create", "feature/x"])).unwrap(),
            ShimCommand::SwitchCreate
        );
        assert_eq!(
            classify_argv(&argv(&["add", "tracked.txt"])).unwrap(),
            ShimCommand::Add
        );
        assert_eq!(
            classify_argv(&argv(&["add", "src/main.rs", "docs/usage.md"])).unwrap(),
            ShimCommand::Add
        );
        assert_eq!(
            classify_argv(&argv(&["commit", "-m", "message"])).unwrap(),
            ShimCommand::Commit
        );
        assert_eq!(
            classify_argv(&argv(&["commit", "--message", "message"])).unwrap(),
            ShimCommand::Commit
        );
    }

    #[test]
    fn classifier_rejects_unsupported_options_config_and_subcommands() {
        for values in [
            vec![],
            vec!["switch"],
            vec!["switch", "-f"],
            vec!["switch", "--force"],
            vec!["switch", "-c"],
            vec!["switch", "-c", "extra", "branch"],
            vec!["switch", "-c", "-x"],
            vec!["switch", "-x"],
            vec!["switch", "--"],
            vec!["switch", "-"],
            vec!["switch", "--", "main"],
            vec!["switch", "main", "extra"],
            vec!["branch"],
            vec!["checkout", "main"],
            vec!["worktree", "add", "feature"],
            vec!["-c", "core.hooksPath=/tmp"],
            vec!["--config-env", "x=y", "true"],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn classifier_rejects_unbounded_add_and_commit_forms() {
        let overly_long_message = "x".repeat(mcp::MAX_GIT_COMMIT_MESSAGE_BYTES + 1);
        for values in [
            vec!["add"],
            vec!["add", "-A"],
            vec!["add", "--all"],
            vec!["add", "."],
            vec!["add", ".."],
            vec!["add", "/abs/path"],
            vec!["add", "-p"],
            vec!["add", "--", "path"],
            vec!["add", "../outside.txt"],
            vec!["add", "dir/../../outside.txt"],
            vec!["add", "tracked.txt", "/abs/other.txt"],
            vec!["add", "-tracked.txt"],
            vec!["add", "tracked*txt"],
            vec!["add", ":tracked.txt"],
            vec!["commit"],
            vec!["commit", "-m"],
            vec!["commit", "--message"],
            vec!["commit", "-a", "-m", "x"],
            vec!["commit", "--amend", "-m", "x"],
            vec!["commit", "-m", "x", "-m", "y"],
            vec!["commit", "--no-verify", "-m", "x"],
            vec!["commit", "-S", "-m", "x"],
            vec!["commit", "-m", "x", "--", "path"],
            vec!["commit", "-m", ""],
            vec!["commit", "--message", "   "],
            vec!["commit", "-m", overly_long_message.as_str()],
        ] {
            assert!(classify_argv(&argv(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn read_only_classifier_accepts_only_the_documented_forms() {
        for values in [
            vec!["status"],
            vec!["status", "--porcelain"],
            vec!["status", "--short", "--branch"],
            vec!["diff"],
            vec!["diff", "--stat"],
            vec!["diff", "--cached", "--name-only"],
            vec!["diff", "HEAD", "--", "src/lib.rs"],
            vec!["log"],
            vec!["log", "--oneline", "-n", "5"],
            vec!["log", "--max-count=10", "--stat"],
            vec!["show", "HEAD"],
            vec!["show", "--stat", "HEAD~1"],
            vec!["rev-parse", "HEAD"],
            vec!["rev-parse", "--verify", "HEAD^{commit}"],
            vec!["ls-files"],
            vec!["ls-files", "--others", "--exclude-standard"],
            vec!["ls-files", "--", "src"],
        ] {
            assert!(
                validate_read_only_git_argv(&argv(&values)).is_ok(),
                "{values:?}"
            );
        }
    }

    #[test]
    fn read_only_classifier_rejects_mutation_and_injection_shapes() {
        for values in [
            vec![],
            vec!["add", "tracked.txt"],
            vec!["commit", "-m", "x"],
            vec!["status", "--output=/tmp/x"],
            vec!["status", "-c"],
            vec!["diff", "--ext-diff"],
            vec!["diff", "--output=out.patch"],
            vec!["log", "--exec-path=/tmp"],
            vec!["show", "--git-dir=/tmp/other"],
            vec!["rev-parse", "--parseopt"],
            vec!["rev-parse", "-x"],
            vec!["ls-files", "/abs/path"],
            vec!["ls-files", "../outside"],
            vec!["ls-files", "--", "src/*.rs"],
            vec!["status", "extra"],
            vec!["log", "-n", "0"],
            vec!["log", "-n", "100000"],
            vec!["log", "--max-count=abc"],
            vec!["diff", "-c", "core.pager=less"],
            vec!["diff", "--config-env=x=y"],
            vec!["alias"],
            vec!["config", "user.name"],
            vec!["push"],
        ] {
            assert!(
                validate_read_only_git_argv(&argv(&values)).is_err(),
                "{values:?}"
            );
        }
    }

    #[tokio::test]
    async fn broker_rejects_cwd_outside_the_selected_workspace() {
        let repository = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(repository.path()).unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        let error = handle_request(
            &state(&repository, Access::WorkspaceWrite),
            request(&outside, &["switch", "main"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outside the selected workspace"));
    }

    #[tokio::test]
    async fn broker_rejects_unsupported_argv_before_touching_a_repository() {
        let repository = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(repository.path()).unwrap();
        init_repository(&repository);
        let error = handle_request(
            &state(&repository, Access::WorkspaceWrite),
            request(&repository, &["checkout", "main"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("unsupported Git shim command"));
    }

    #[tokio::test]
    async fn broker_rejects_mutations_for_read_only_access_and_preserves_the_repository() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        let head_before = run_host_git(&repository, &["rev-parse", "HEAD"]);
        let branch_before = run_host_git(&repository, &["branch", "--show-current"]);
        let index_before =
            std::fs::read(repository.join(".git/index")).expect("index must exist after commit");

        for values in [
            vec!["switch", "-c", "feature/read-only"],
            vec!["switch", "main"],
            vec!["add", "tracked.txt"],
            vec!["commit", "-m", "read-only"],
        ] {
            let error = handle_request(
                &state(&repository, Access::ReadOnly),
                request(&repository, &values),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("workspace_write access"),
                "{values:?}: {error}"
            );
        }

        assert_eq!(
            run_host_git(&repository, &["rev-parse", "HEAD"]),
            head_before
        );
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            branch_before
        );
        assert_eq!(
            std::fs::read(repository.join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt")).unwrap(),
            "base\n"
        );
    }

    #[tokio::test]
    async fn broker_rejects_requests_for_a_different_workspace_and_preserves_its_metadata() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        std::fs::create_dir_all(fixture.path().join("sibling")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        let sibling = std::fs::canonicalize(fixture.path().join("sibling")).unwrap();
        init_repository(&selected);
        init_repository(&sibling);
        std::fs::write(sibling.join("tracked.txt"), "sibling-dirty\n").unwrap();
        let sibling_head = run_host_git(&sibling, &["rev-parse", "HEAD"]);
        let sibling_branch = run_host_git(&sibling, &["branch", "--show-current"]);
        let sibling_index = std::fs::read(sibling.join(".git/index")).unwrap();

        let mut session = session(&selected);
        session.permitted_directories = vec![selected.clone(), sibling.clone()];
        let broker_state = BrokerState {
            access: Access::WorkspaceWrite,
            scope: BrokerScope::for_workspace(&selected).unwrap(),
            session,
            src_root: None,
        };

        let error = handle_request(
            &broker_state,
            request(&sibling, &["switch", "-c", "feature/other"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outside the selected workspace"));

        assert_eq!(run_host_git(&sibling, &["rev-parse", "HEAD"]), sibling_head);
        assert_eq!(
            run_host_git(&sibling, &["branch", "--show-current"]),
            sibling_branch
        );
        assert_eq!(
            std::fs::read(sibling.join(".git/index")).unwrap(),
            sibling_index
        );
        assert_eq!(
            std::fs::read_to_string(sibling.join("tracked.txt")).unwrap(),
            "sibling-dirty\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn broker_rejects_symlinked_cwd_and_paths_outside_the_workspace() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        std::fs::create_dir_all(fixture.path().join("sibling")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        let sibling = std::fs::canonicalize(fixture.path().join("sibling")).unwrap();
        init_repository(&selected);
        init_repository(&sibling);
        symlink(&sibling, selected.join("escape")).unwrap();
        std::fs::write(sibling.join("secret.txt"), "secret\n").unwrap();

        let error = handle_request(
            &state(&selected, Access::WorkspaceWrite),
            request(&selected.join("escape"), &["switch", "main"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outside the selected workspace"));

        let error = handle_request(
            &state(&selected, Access::WorkspaceWrite),
            request(&selected, &["add", "escape/secret.txt"]),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("outside the selected workspace"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(sibling.join("secret.txt")).unwrap(),
            "secret\n"
        );
    }

    #[tokio::test]
    async fn broker_rejects_nested_repositories_and_linked_worktrees() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        let nested = selected.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        init_repository(&selected);
        init_repository(&nested);

        let error = handle_request(
            &state(&selected, Access::WorkspaceWrite),
            request(&nested, &["switch", "-c", "feature/nested"]),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different repository than the selected workspace"),
            "{error}"
        );

        let worktree = fixture.path().join("linked");
        run_host_git(
            &selected,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
        );
        let linked = std::fs::canonicalize(&worktree).unwrap();
        let linked_head = run_host_git(&linked, &["rev-parse", "HEAD"]);
        let error = handle_request(
            &state(&selected, Access::WorkspaceWrite),
            request(&linked, &["switch", "-c", "feature/linked"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outside the selected workspace"));
        assert_eq!(run_host_git(&linked, &["rev-parse", "HEAD"]), linked_head);
    }

    #[tokio::test]
    async fn broker_rejects_a_workspace_that_is_not_a_git_worktree_root() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        let subdirectory = repository.join("sub");
        std::fs::create_dir(&subdirectory).unwrap();

        let error = handle_request(
            &state(&subdirectory, Access::WorkspaceWrite),
            request(&subdirectory, &["switch", "-c", "feature/sub"]),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("not a Git worktree root"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn broker_adds_and_commits_only_the_named_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);

        std::fs::write(repository.join("other.txt"), "base\n").unwrap();
        run_host_git(&repository, &["add", "other.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "add other"]);
        std::fs::write(repository.join("tracked.txt"), "updated\n").unwrap();
        std::fs::write(repository.join("other.txt"), "unrelated\n").unwrap();
        std::fs::write(repository.join("untracked.txt"), "new\n").unwrap();

        let broker_state = state(&repository, Access::WorkspaceWrite);

        let added = handle_request(&broker_state, request(&repository, &["add", "tracked.txt"]))
            .await
            .unwrap();
        assert_eq!(added.status, 0, "{}", added.stderr);

        let committed = handle_request(
            &broker_state,
            request(&repository, &["commit", "-m", "update tracked only"]),
        )
        .await
        .unwrap();
        assert_eq!(committed.status, 0, "{}", committed.stderr);

        assert_eq!(
            run_host_git(
                &repository,
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
            ),
            "tracked.txt"
        );
        assert_eq!(
            run_host_git(&repository, &["show", "HEAD:tracked.txt"]),
            "updated"
        );

        let status = run_host_git(&repository, &["status", "--porcelain"]);
        let status_lines = status
            .lines()
            .map(|line| line.trim_start().to_owned())
            .collect::<Vec<_>>();
        assert!(
            status_lines.iter().any(|line| line == "M other.txt"),
            "{status_lines:?}"
        );
        assert!(
            status_lines.iter().any(|line| line == "?? untracked.txt"),
            "{status_lines:?}"
        );
        assert!(
            !status_lines
                .iter()
                .any(|line| line.ends_with(" tracked.txt")),
            "{status_lines:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("other.txt")).unwrap(),
            "unrelated\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("untracked.txt")).unwrap(),
            "new\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn broker_rejects_add_paths_that_resolve_outside_the_selected_workspace() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path()).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), repository.join("link.txt"))
            .unwrap();

        let error = handle_request(
            &state(&repository, Access::WorkspaceWrite),
            request(&repository, &["add", "link.txt"]),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("outside the selected workspace"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn linked_worktree_root_accepts_scope_and_serves_supported_mutations() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        init_repository(&selected);
        let worktree = fixture.path().join("linked");
        run_host_git(
            &selected,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
        );
        let linked = std::fs::canonicalize(&worktree).unwrap();

        let broker_state = state(&linked, Access::WorkspaceWrite);
        let created = handle_request(
            &broker_state,
            request(&linked, &["switch", "-c", "feature/linked"]),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);

        std::fs::write(linked.join("tracked.txt"), "linked-update\n").unwrap();
        let added = handle_request(&broker_state, request(&linked, &["add", "tracked.txt"]))
            .await
            .unwrap();
        assert_eq!(added.status, 0, "{}", added.stderr);
        let committed = handle_request(
            &broker_state,
            request(&linked, &["commit", "-m", "linked update"]),
        )
        .await
        .unwrap();
        assert_eq!(committed.status, 0, "{}", committed.stderr);

        assert_eq!(
            run_host_git(&linked, &["branch", "--show-current"]),
            "feature/linked"
        );
        assert_eq!(
            run_host_git(&selected, &["branch", "--show-current"]),
            "main"
        );
        assert_eq!(
            std::fs::read_to_string(selected.join("tracked.txt")).unwrap(),
            "base\n"
        );
    }

    fn repository_snapshot(path: &Path) -> (String, String, String) {
        (
            run_host_git(path, &["branch", "--show-current"]),
            run_host_git(path, &["rev-parse", "HEAD"]),
            run_host_git(path, &["status", "--porcelain"]),
        )
    }

    #[tokio::test]
    async fn linked_worktree_scope_pins_metadata_below_the_primary_checkout() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        init_repository(&selected);
        let worktree = fixture.path().join("linked");
        run_host_git(
            &selected,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
        );
        let linked = std::fs::canonicalize(&worktree).unwrap();

        let scope = BrokerScope::for_workspace(&linked).unwrap();
        let identity = scope.repository.as_ref().unwrap();
        assert!(identity.linked_worktree());
        assert_eq!(identity.worktree_root, linked);
        assert!(
            identity
                .metadata_roots
                .iter()
                .all(|root| !root.starts_with(&linked)),
            "{:?}",
            identity.metadata_roots
        );
        assert_eq!(
            sandbox::git_primary_checkout(&linked).unwrap(),
            selected,
            "the primary checkout stays the repository identity anchor"
        );
        assert_eq!(
            sandbox::git_common_dir(&linked).unwrap(),
            selected.join(".git")
        );

        scope.resolve_cwd(&linked).unwrap();
        let error = scope.resolve_cwd(&selected).unwrap_err();
        assert!(
            error.to_string().contains("outside the selected workspace"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn broker_rejects_swapped_and_symlinked_linked_worktree_metadata() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("primary")).unwrap();
        let primary = std::fs::canonicalize(fixture.path().join("primary")).unwrap();
        init_repository(&primary);
        let worktree = fixture.path().join("linked");
        run_host_git(
            &primary,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
        );
        let linked = std::fs::canonicalize(&worktree).unwrap();
        let broker_state = state(&linked, Access::WorkspaceWrite);

        // A swapped `.git` pointer to another repository's structurally valid
        // private metadata must fail closed on the pinned identity. The other
        // repository's back-pointer is rewritten so only the identity check can
        // reject this.
        std::fs::create_dir_all(fixture.path().join("other")).unwrap();
        let other = std::fs::canonicalize(fixture.path().join("other")).unwrap();
        init_repository(&other);
        let other_worktree = fixture.path().join("other-linked");
        run_host_git(
            &other,
            &[
                "worktree",
                "add",
                "--quiet",
                other_worktree.to_str().unwrap(),
                "-b",
                "other",
            ],
        );
        let other_private = other.join(".git").join("worktrees").join("other-linked");
        std::fs::write(
            other_private.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();
        let primary_before = repository_snapshot(&primary);
        let other_before = repository_snapshot(&other);
        let linked_pointer = std::fs::read_to_string(linked.join(".git")).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", other_private.display()),
        )
        .unwrap();

        let error = handle_request(
            &broker_state,
            request(&linked, &["switch", "-c", "feature/swapped"]),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("different Git metadata"),
            "{error}"
        );

        std::fs::write(linked.join(".git"), &linked_pointer).unwrap();
        assert_eq!(repository_snapshot(&primary), primary_before);
        assert_eq!(repository_snapshot(&other), other_before);

        // A symlinked `.git` pointer fails closed before any Git process runs.
        std::fs::remove_file(linked.join(".git")).unwrap();
        symlink(primary.join(".git"), linked.join(".git")).unwrap();
        let error = handle_request(
            &broker_state,
            request(&linked, &["switch", "-c", "feature/symlinked"]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("symbolic-link"), "{error}");
        assert_eq!(repository_snapshot(&primary), primary_before);
        assert_eq!(repository_snapshot(&other), other_before);
    }

    /// Host acceptance (nested Linux sandbox required). The linked-worktree
    /// metadata scope fix must serve the same mutations in the default `agent`
    /// mode that the yolo path already served, while the primary checkout and
    /// every sibling worktree stay unchanged.
    #[tokio::test]
    #[ignore = "host acceptance: requires the nested Linux sandbox helper"]
    async fn agent_mode_linked_worktree_mutations_use_only_the_pinned_metadata() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fixture.path().join("selected")).unwrap();
        let selected = std::fs::canonicalize(fixture.path().join("selected")).unwrap();
        init_repository(&selected);
        std::fs::write(selected.join("primary-untracked.txt"), "keep\n").unwrap();
        let worktree = fixture.path().join("linked");
        let sibling = fixture.path().join("sibling");
        run_host_git(
            &selected,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
        );
        run_host_git(
            &selected,
            &[
                "worktree",
                "add",
                "--quiet",
                sibling.to_str().unwrap(),
                "-b",
                "sibling",
            ],
        );
        let linked = std::fs::canonicalize(&worktree).unwrap();
        let sibling = std::fs::canonicalize(&sibling).unwrap();
        let primary_before = repository_snapshot(&selected);
        let sibling_before = repository_snapshot(&sibling);

        let session = config::Session {
            permission_mode: config::PermissionMode::Agent,
            ..session(&linked)
        };
        let broker_state = BrokerState {
            session,
            access: Access::WorkspaceWrite,
            scope: BrokerScope::for_workspace(&linked).unwrap(),
            src_root: None,
        };
        assert!(
            broker_state
                .scope
                .repository
                .as_ref()
                .unwrap()
                .linked_worktree()
        );

        let created = handle_request(
            &broker_state,
            request(&linked, &["switch", "-c", "feature/agent"]),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);

        std::fs::write(linked.join("tracked.txt"), "agent-update\n").unwrap();
        let added = handle_request(&broker_state, request(&linked, &["add", "tracked.txt"]))
            .await
            .unwrap();
        assert_eq!(added.status, 0, "{}", added.stderr);
        let committed = handle_request(
            &broker_state,
            request(&linked, &["commit", "-m", "agent linked update"]),
        )
        .await
        .unwrap();
        assert_eq!(committed.status, 0, "{}", committed.stderr);

        assert_eq!(
            run_host_git(&linked, &["branch", "--show-current"]),
            "feature/agent"
        );
        assert_eq!(
            run_host_git(&linked, &["show", "HEAD:tracked.txt"]),
            "agent-update"
        );
        assert_eq!(repository_snapshot(&selected), primary_before);
        assert_eq!(repository_snapshot(&sibling), sibling_before);
    }

    #[tokio::test]
    async fn broker_creates_switches_and_preserves_a_dirty_worktree() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        let broker_state = state(&repository, Access::WorkspaceWrite);

        let created = handle_request(
            &broker_state,
            request(&repository, &["switch", "-c", "feature/x"]),
        )
        .await
        .unwrap();
        assert_eq!(created.status, 0, "{}", created.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "feature/x"
        );

        let switched = handle_request(&broker_state, request(&repository, &["switch", "main"]))
            .await
            .unwrap();
        assert_eq!(switched.status, 0, "{}", switched.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "main"
        );

        run_host_git(
            &repository,
            &["switch", "--quiet", "-c", "feature/conflict"],
        );
        std::fs::write(repository.join("tracked.txt"), "feature\n").unwrap();
        run_host_git(&repository, &["add", "tracked.txt"]);
        run_host_git(&repository, &["commit", "--quiet", "-m", "feature"]);
        let switched = handle_request(&broker_state, request(&repository, &["switch", "main"]))
            .await
            .unwrap();
        assert_eq!(switched.status, 0, "{}", switched.stderr);

        std::fs::write(repository.join("tracked.txt"), "dirty-main\n").unwrap();
        let conflicted = handle_request(
            &broker_state,
            request(&repository, &["switch", "feature/conflict"]),
        )
        .await
        .unwrap();
        assert_ne!(conflicted.status, 0);
        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt")).unwrap(),
            "dirty-main\n"
        );
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "main"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shim_and_broker_round_trip_over_the_private_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);

        let broker_root = tempfile::tempdir().unwrap();
        let broker_root = std::fs::canonicalize(broker_root.path()).unwrap();
        let (request_directory, response_directory) = broker_roots(&broker_root);
        let _broker = GitBroker::start(
            request_directory.clone(),
            response_directory.clone(),
            session(&repository),
            repository.clone(),
            Access::WorkspaceWrite,
        )
        .unwrap();

        let requests = request_directory.clone();
        let responses = response_directory.clone();
        let request_repository = repository.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            request_broker_with_timeout(
                &requests,
                &responses,
                &request_repository,
                &argv(&["switch", "-c", "feature/round-trip"]),
                REQUEST_TIMEOUT,
            )
        })
        .await
        .unwrap()
        .unwrap();
        let ShimOutcome::Completed(result) = outcome else {
            panic!("expected a completed round trip");
        };
        assert_eq!(result.status, 0, "{}", result.stderr);
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "feature/round-trip"
        );

        let requests = request_directory.clone();
        let responses = response_directory.clone();
        let request_repository = repository.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            request_broker_with_timeout(
                &requests,
                &responses,
                &request_repository,
                &argv(&["checkout", "main"]),
                REQUEST_TIMEOUT,
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(outcome, ShimOutcome::Rejected));
    }

    #[cfg(unix)]
    #[test]
    fn queue_rejects_symlinked_and_special_entries_without_touching_sentinels() {
        use std::os::unix::fs::symlink;

        let queue_root = tempfile::tempdir().unwrap();
        let queue_root = std::fs::canonicalize(queue_root.path()).unwrap();
        let (queue_directory, response_directory) = broker_roots(&queue_root);
        let queue = BrokerQueue::create(&queue_directory, &response_directory).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel.txt");
        std::fs::write(&sentinel, "sentinel\n").unwrap();

        symlink(
            &sentinel,
            queue_directory.join(REQUESTS_DIRECTORY).join("link.json"),
        )
        .unwrap();
        let error = queue.read_request("link.json").unwrap_err();
        assert!(
            error.to_string().contains("cannot open Git broker entry"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "sentinel\n");

        let fifo = queue_directory.join(REQUESTS_DIRECTORY).join("fifo.json");
        let fifo_c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let error = queue.read_request("fifo.json").unwrap_err();
        assert!(error.to_string().contains("not a regular file"));

        let oversized = queue_directory.join(REQUESTS_DIRECTORY).join("big.json");
        std::fs::write(&oversized, vec![b'x'; MAX_REQUEST_BYTES + 1]).unwrap();
        let error = queue.read_request("big.json").unwrap_err();
        assert!(error.to_string().contains("size limit"));

        symlink(&sentinel, response_directory.join("target.json")).unwrap();
        queue
            .publish_response("target.json", br#"{"schema":1,"status":0}"#)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "sentinel\n");
        assert_eq!(
            queue.read_response("target.json").unwrap().unwrap(),
            br#"{"schema":1,"status":0}"#
        );
        let response_metadata =
            std::fs::symlink_metadata(response_directory.join("target.json")).unwrap();
        assert!(response_metadata.file_type().is_file());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broker_survives_malformed_and_special_queue_entries() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);

        let broker_root = tempfile::tempdir().unwrap();
        let broker_root = std::fs::canonicalize(broker_root.path()).unwrap();
        let (request_directory, response_directory) = broker_roots(&broker_root);
        let _broker = GitBroker::start(
            request_directory.clone(),
            response_directory.clone(),
            session(&repository),
            repository.clone(),
            Access::WorkspaceWrite,
        )
        .unwrap();

        let requests = request_directory.join(REQUESTS_DIRECTORY);
        let fifo = requests.join("aaaa.json");
        let fifo_c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        std::fs::write(requests.join("bbbb.json"), b"not json").unwrap();
        std::fs::write(
            requests.join("cccc.json"),
            vec![b'x'; MAX_REQUEST_BYTES + 1],
        )
        .unwrap();

        let requests = request_directory.clone();
        let responses = response_directory.clone();
        let request_repository = repository.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            request_broker_with_timeout(
                &requests,
                &responses,
                &request_repository,
                &argv(&["switch", "-c", "feature/after-malformed"]),
                Duration::from_secs(5),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(outcome, ShimOutcome::Completed(_)));
        assert_eq!(
            run_host_git(&repository, &["branch", "--show-current"]),
            "feature/after-malformed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn queue_enumerates_a_bounded_number_of_entries() {
        let queue_root = tempfile::tempdir().unwrap();
        let queue_root = std::fs::canonicalize(queue_root.path()).unwrap();
        let (queue_directory, response_directory) = broker_roots(&queue_root);
        let queue = BrokerQueue::create(&queue_directory, &response_directory).unwrap();
        let requests = queue_directory.join(REQUESTS_DIRECTORY);
        for index in 0..(MAX_QUEUE_ENTRIES_SCANNED + 8) {
            std::fs::write(requests.join(format!("r{index:05}.json")), b"{}").unwrap();
        }
        let names = queue.enumerate_requests();
        assert!(names.len() <= MAX_QUEUE_ENTRIES_SCANNED);
        assert!(!names.is_empty());
    }

    #[test]
    fn published_responses_are_never_partially_visible() {
        let queue_root = tempfile::tempdir().unwrap();
        let queue_root = std::fs::canonicalize(queue_root.path()).unwrap();
        let (queue_directory, response_directory) = broker_roots(&queue_root);
        let queue = std::sync::Arc::new(
            BrokerQueue::create(&queue_directory, &response_directory).unwrap(),
        );
        let first = vec![b'a'; 1024 * 1024];
        let second = vec![b'b'; 512 * 1024];
        let names = [
            first.clone(),
            second.clone(),
            first.clone(),
            second.clone(),
            first.clone(),
        ];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader_queue = std::sync::Arc::clone(&queue);
        let reader_barrier = std::sync::Arc::clone(&barrier);
        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            reader_barrier.wait();
            for _ in 0..2_000 {
                if let Ok(Some(bytes)) = reader_queue.read_response("target.json") {
                    seen.push(bytes);
                }
            }
            seen
        });
        barrier.wait();
        for payload in &names {
            queue.publish_response("target.json", payload).unwrap();
        }
        let seen = reader.join().unwrap();
        assert!(
            seen.iter().all(|bytes| bytes == &first || bytes == &second),
            "a reader observed a partial response"
        );
    }

    #[test]
    fn shim_outcome_distinguishes_rejection_from_indeterminate() {
        let directory = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(directory.path()).unwrap();
        let (queue_directory, response_directory) = broker_roots(&directory);
        let queue = BrokerQueue::create(&queue_directory, &response_directory).unwrap();
        let cwd = std::env::temp_dir();
        let outcome = request_broker_with_timeout(
            &queue_directory,
            &response_directory,
            &cwd,
            &argv(&["switch", "main"]),
            Duration::from_millis(100),
        );
        assert!(matches!(outcome, Ok(ShimOutcome::Indeterminate)));
        let queued = std::fs::read_dir(queue_directory.join(REQUESTS_DIRECTORY))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .count();
        assert_eq!(
            queued, 1,
            "an indeterminate request must not be re-labeled as not executed"
        );
        drop(queue);
    }

    #[test]
    fn decode_response_accepts_only_the_fixed_broker_shapes() {
        let completed = br#"{"schema":1,"status":2,"stdout":"out","stderr":"err"}"#;
        match decode_response(completed).unwrap() {
            DecodedResponse::Completed(result) => {
                assert_eq!(result.status, 2);
                assert_eq!(result.stdout, "out");
                assert_eq!(result.stderr, "err");
            }
            DecodedResponse::Rejected => panic!("completed payload decoded as rejected"),
        }

        let rejected = serde_json::to_vec(&json!({
            "schema": BROKER_SCHEMA,
            "error": SHIM_REJECTION_MESSAGE,
        }))
        .unwrap();
        assert!(matches!(
            decode_response(&rejected).unwrap(),
            DecodedResponse::Rejected
        ));

        for payload in [
            json!({"schema": 1, "error": "forged"}),
            json!({"schema": 1, "error": SHIM_REJECTION_MESSAGE, "status": 0}),
            json!({"schema": 1, "status": "0", "stdout": "", "stderr": ""}),
            json!({"schema": 2, "status": 0, "stdout": "", "stderr": ""}),
            json!({"schema": 1, "status": 0, "stdout": "", "stderr": "", "extra": true}),
            json!({"schema": 1, "error": SHIM_REJECTION_MESSAGE, "extra": true}),
        ] {
            assert!(
                decode_response(&serde_json::to_vec(&payload).unwrap()).is_err(),
                "{payload}"
            );
        }
    }

    #[test]
    fn resolve_trusted_git_skips_the_private_shim() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = bin.join("git");
        std::fs::write(&shim, "shim").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let real_dir = root.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("git");
        std::fs::write(&real, "real").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([bin.as_os_str(), real_dir.as_os_str()]).unwrap();
        let path = path.to_str().unwrap();
        let shim_target = std::fs::canonicalize(&shim).unwrap();
        assert_eq!(
            resolve_trusted_git(Some(path), &shim_target),
            Some(std::fs::canonicalize(&real).unwrap())
        );
    }

    #[test]
    fn shim_routes_mutations_to_the_broker_and_read_only_commands_to_git() {
        let git = Path::new("/usr/bin/git");
        let broker = Path::new("/run/git-broker");
        let responses = Path::new("/run/git-responses");
        for values in [
            vec!["switch", "main"],
            vec!["switch", "-c", "feature/x"],
            vec!["add", "tracked.txt"],
            vec!["commit", "-m", "x"],
            vec!["fetch", "--prune"],
            vec!["pull", "--ff-only"],
            vec!["push"],
            vec!["push", "-u", "origin"],
            vec!["worktree", "list"],
            vec!["worktree", "remove", "/src/worktrees/repo/task"],
        ] {
            assert_eq!(
                shim_route(Some(broker), Some(responses), Some(git), &argv(&values)),
                ShimRoute::Mutation,
                "{values:?}"
            );
        }
        for values in [
            vec!["status"],
            vec!["status", "--porcelain"],
            vec!["diff", "--stat"],
            vec!["log", "--oneline", "-n", "3"],
            vec!["show", "HEAD"],
            vec!["rev-parse", "HEAD"],
            vec!["ls-files"],
        ] {
            assert_eq!(
                shim_route(Some(broker), Some(responses), Some(git), &argv(&values)),
                ShimRoute::ReadOnly,
                "{values:?}"
            );
        }
        assert_eq!(
            shim_route(None, None, Some(git), &argv(&["status"])),
            ShimRoute::ReadOnly,
            "read-only commands must not require a broker"
        );
        assert_eq!(
            shim_route(Some(broker), Some(responses), None, &argv(&["status"])),
            ShimRoute::Reject
        );
        assert_eq!(
            shim_route(None, None, Some(git), &argv(&["switch", "main"])),
            ShimRoute::Reject,
            "mutations require the broker queues"
        );
        assert_eq!(
            shim_route(Some(broker), None, Some(git), &argv(&["switch", "main"])),
            ShimRoute::Reject,
            "mutations require the response queue"
        );
        assert_eq!(
            shim_route(None, Some(responses), Some(git), &argv(&["switch", "main"])),
            ShimRoute::Reject,
            "mutations require the request queue"
        );
        for values in [
            vec!["push", "--force"],
            vec!["fetch", "--tags"],
            vec!["fetch", "https://github.com/example/repo.git"],
            vec!["config", "user.name"],
            vec!["status", "--output=/tmp/x"],
            vec!["diff", "--ext-diff"],
        ] {
            assert_eq!(
                shim_route(Some(broker), Some(responses), Some(git), &argv(&values)),
                ShimRoute::Reject,
                "{values:?}"
            );
        }
    }

    #[test]
    fn run_shim_executes_read_only_git_without_touching_the_broker() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        std::fs::write(repository.join("tracked.txt"), "changed\n").unwrap();

        let broker_root = tempfile::tempdir().unwrap();
        let broker_root = std::fs::canonicalize(broker_root.path()).unwrap();
        let (request_directory, response_directory) = broker_roots(&broker_root);
        let queue = BrokerQueue::create(&request_directory, &response_directory).unwrap();
        let git = resolve_trusted_git(
            std::env::var("PATH").ok().as_deref(),
            Path::new("/nonexistent-shim-target"),
        )
        .expect("host git must be resolvable for this test");

        let status = run_shim_with(
            Some(&request_directory),
            Some(&response_directory),
            Some(&git),
            &repository,
            &argv(&["status", "--porcelain"]),
        );
        assert_eq!(status, 0);
        assert!(
            std::fs::read_dir(request_directory.join(REQUESTS_DIRECTORY))
                .unwrap()
                .next()
                .is_none(),
            "read-only command must not enqueue a broker request"
        );

        assert_eq!(
            run_shim_with(
                Some(&request_directory),
                Some(&response_directory),
                None,
                &repository,
                &argv(&["rev-parse", "HEAD"])
            ),
            SHIM_EXIT_REJECTED
        );
        assert_eq!(
            run_shim_with(
                Some(&request_directory),
                Some(&response_directory),
                Some(&git),
                &repository,
                &argv(&["merge", "main"])
            ),
            SHIM_EXIT_REJECTED
        );
        assert_eq!(
            run_shim_with(
                Some(&request_directory),
                Some(&response_directory),
                Some(&git),
                &repository,
                &argv(&["push", "--force"])
            ),
            SHIM_EXIT_REJECTED
        );
        assert_eq!(
            run_shim_with(None, None, Some(&git), &repository, &argv(&["status"])),
            0,
            "read-only commands must not require a broker"
        );
        drop(queue);
    }

    #[test]
    fn read_only_git_executes_real_repository_commands() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = std::fs::canonicalize(fixture.path()).unwrap();
        init_repository(&repository);
        std::fs::write(repository.join("tracked.txt"), "changed\n").unwrap();

        let git = resolve_trusted_git(
            std::env::var("PATH").ok().as_deref(),
            Path::new("/nonexistent-shim-target"),
        )
        .expect("host git must be resolvable for this test");

        assert_eq!(
            run_read_only_git(&git, &argv(&["status", "--porcelain"]), &repository),
            0
        );
        assert_eq!(
            run_read_only_git(&git, &argv(&["diff", "--name-only"]), &repository),
            0
        );
        assert_eq!(
            run_read_only_git(&git, &argv(&["log", "--oneline", "-n", "1"]), &repository),
            0
        );
        assert_eq!(
            run_read_only_git(&git, &argv(&["show", "HEAD"]), &repository),
            0
        );
        assert_eq!(
            run_read_only_git(&git, &argv(&["rev-parse", "HEAD"]), &repository),
            0
        );
        assert_eq!(
            run_read_only_git(&git, &argv(&["ls-files"]), &repository),
            0
        );
    }
}
