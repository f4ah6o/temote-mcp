use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{approvals, child_env, config, sandbox};

pub(crate) const MAX_TASK_BYTES: usize = 1024 * 1024;
// OpenCode exposes its prompt only as a positional `message..` argument in
// the verified CLI contract. Keep that argument well below Linux's per-string
// exec limit instead of claiming the Codex stdin budget for it.
pub(crate) const MAX_OPENCODE_TASK_BYTES: usize = 64 * 1024;
const MAX_CWD_BYTES: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;
const MAX_PROFILE_BYTES: usize = 128;
const MAX_TASK_PREVIEW_BYTES: usize = 2048;
const MAX_TASK_PREVIEW_CHARS: usize = 384;
const MAX_TASK_PREVIEW_LINES: usize = 8;
const TASK_PREVIEW_TRUNCATION_MARKER: &str = "… [truncated]";
const MAX_APPROVAL_DETAIL_BYTES: usize = 16 * 1024;
const MAX_ENV_VALUE_BYTES: usize = 32 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 128 * 1024;
const MAX_IMPORTED_AUTH_BYTES: u64 = 1024 * 1024;
const AGENT_STATE_DIRECTORY_PREFIX: &str = "temote-mcp-local-agent-";
const CODEX_PERMISSION_PROFILE_NAME: &str = "temote_local_agent";

const SAFE_ENV_NAMES: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "CODEX_HOME",
    "XDG_DATA_HOME",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Agent {
    Codex,
    OpenCode,
}

impl Agent {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            _ => anyhow::bail!("unsupported local agent; supported agents are codex and opencode"),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    const fn executable_name(self) -> &'static str {
        self.as_str()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    ReadOnly,
    WorkspaceWrite,
}

impl Access {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "workspace_write" => Ok(Self::WorkspaceWrite),
            _ => anyhow::bail!(
                "unsupported local agent access; supported values are read_only and workspace_write"
            ),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
        }
    }
}

struct AgentState {
    root: PathBuf,
    hidden_roots: Vec<PathBuf>,
    read_only_paths: Vec<PathBuf>,
}

impl AgentState {
    #[cfg(test)]
    fn create(agent: Agent, forbidden_roots: &[PathBuf]) -> Result<Self> {
        Self::create_with_source_home(agent, forbidden_roots, None)
    }

    #[cfg(test)]
    fn create_with_source_home(
        agent: Agent,
        forbidden_roots: &[PathBuf],
        source_home: Option<&Path>,
    ) -> Result<Self> {
        Self::create_with_auth_sources(agent, forbidden_roots, source_home, None, None)
    }

    fn create_with_auth_sources(
        agent: Agent,
        forbidden_roots: &[PathBuf],
        source_home: Option<&Path>,
        source_codex_home: Option<&Path>,
        source_xdg_data_home: Option<&Path>,
    ) -> Result<Self> {
        let base = fs::canonicalize(env::temp_dir())
            .context("could not resolve the system temporary directory")?;
        anyhow::ensure!(
            base.is_dir(),
            "system temporary directory is not a directory: {}",
            base.display()
        );
        ensure_outside_permitted_roots(
            &base,
            forbidden_roots,
            "local agent state directory would be inside a permitted session root",
        )?;
        let root = base.join(format!("{AGENT_STATE_DIRECTORY_PREFIX}{}", Uuid::new_v4()));
        fs::create_dir(&root).with_context(|| {
            format!(
                "could not create local agent state directory {}",
                root.display()
            )
        })?;
        set_private_permissions(&root)?;

        let mut hidden_roots = vec![base];
        if let Some(home) = source_home {
            anyhow::ensure!(
                home != Path::new("/"),
                "local agent authentication HOME must not be the filesystem root"
            );
            ensure_outside_permitted_roots(
                home,
                forbidden_roots,
                "local agent authentication source is inside a permitted session root",
            )?;
            hidden_roots.push(home.to_owned());
        }
        for (source, name) in [
            (source_codex_home, "CODEX_HOME"),
            (source_xdg_data_home, "XDG_DATA_HOME"),
        ] {
            let Some(source) = source else {
                continue;
            };
            let Ok(source) = fs::canonicalize(source) else {
                continue;
            };
            anyhow::ensure!(
                source != Path::new("/"),
                "local agent {name} authentication source must not be the filesystem root"
            );
            ensure_outside_permitted_roots(
                &source,
                forbidden_roots,
                "local agent authentication source is inside a permitted session root",
            )?;
            hidden_roots.push(source);
        }
        hidden_roots.sort();
        hidden_roots.dedup();

        let mut state = Self {
            root,
            hidden_roots,
            read_only_paths: Vec::new(),
        };
        let directories = match agent {
            Agent::Codex => ["tmp", "home", "codex"].as_slice(),
            Agent::OpenCode => ["tmp", "home", "config", "data", "cache", "state"].as_slice(),
        };
        for directory in directories {
            let path = state.root.join(directory);
            fs::create_dir(&path).with_context(|| {
                format!("could not create local agent directory {}", path.display())
            })?;
            set_private_permissions(&path)?;
        }
        let read_only_paths = source_home
            .map(|home| {
                import_authentication_file(
                    agent,
                    home,
                    source_codex_home,
                    source_xdg_data_home,
                    &state.root,
                )
            })
            .transpose()?
            .unwrap_or_default();
        state.read_only_paths = read_only_paths;
        Ok(state)
    }

    fn ensure_outside_permitted_roots(&self, permitted_roots: &[PathBuf]) -> Result<()> {
        ensure_outside_permitted_roots(
            &self.root,
            permitted_roots,
            "local agent state directory is inside a permitted session root",
        )
    }

    fn apply_to_environment(&self, agent: Agent, environment: &mut HashMap<String, String>) {
        let temporary = self.root.join("tmp").to_string_lossy().into_owned();
        environment.insert(
            "HOME".to_owned(),
            self.root.join("home").to_string_lossy().into_owned(),
        );
        environment.insert("TMPDIR".to_owned(), temporary.clone());
        environment.insert("TMP".to_owned(), temporary.clone());
        environment.insert("TEMP".to_owned(), temporary);

        match agent {
            Agent::Codex => {
                environment.insert(
                    "CODEX_HOME".to_owned(),
                    self.root.join("codex").to_string_lossy().into_owned(),
                );
                environment.remove("XDG_DATA_HOME");
            }
            Agent::OpenCode => {
                environment.remove("CODEX_HOME");
                environment.insert(
                    "XDG_CONFIG_HOME".to_owned(),
                    self.root.join("config").to_string_lossy().into_owned(),
                );
                environment.insert(
                    "XDG_DATA_HOME".to_owned(),
                    self.root.join("data").to_string_lossy().into_owned(),
                );
                environment.insert(
                    "XDG_CACHE_HOME".to_owned(),
                    self.root.join("cache").to_string_lossy().into_owned(),
                );
                environment.insert(
                    "XDG_STATE_HOME".to_owned(),
                    self.root.join("state").to_string_lossy().into_owned(),
                );
            }
        }
    }

    fn temporary_root(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn hidden_roots(&self) -> &[PathBuf] {
        &self.hidden_roots
    }

    fn read_only_paths(&self) -> &[PathBuf] {
        &self.read_only_paths
    }
}

impl Drop for AgentState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect local agent directory {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn import_authentication_file(
    agent: Agent,
    source_home: &Path,
    source_codex_home: Option<&Path>,
    source_xdg_data_home: Option<&Path>,
    state_root: &Path,
) -> Result<Vec<PathBuf>> {
    let (source, destination) = match agent {
        Agent::Codex => {
            let home = source_codex_home
                .map(Path::to_owned)
                .unwrap_or_else(|| source_home.join(".codex"));
            (home.join("auth.json"), state_root.join("codex/auth.json"))
        }
        Agent::OpenCode => {
            let data = source_xdg_data_home
                .map(Path::to_owned)
                .unwrap_or_else(|| source_home.join(".local/share"));
            (
                data.join("opencode/auth.json"),
                state_root.join("data/opencode/auth.json"),
            )
        }
    };
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not inspect local agent auth file {}",
                    source.display()
                )
            });
        }
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "local agent auth path is not a regular file: {}",
        source.display()
    );
    anyhow::ensure!(
        metadata.len() <= MAX_IMPORTED_AUTH_BYTES,
        "local agent auth file exceeds {MAX_IMPORTED_AUTH_BYTES} bytes: {}",
        source.display()
    );

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut input = options
        .open(&source)
        .with_context(|| format!("could not open local agent auth file {}", source.display()))?;
    let input_metadata = input.metadata()?;
    anyhow::ensure!(
        input_metadata.file_type().is_file() && input_metadata.len() <= MAX_IMPORTED_AUTH_BYTES,
        "local agent auth file changed while it was being opened: {}",
        source.display()
    );
    let mut bytes = Vec::with_capacity(input_metadata.len() as usize);
    std::io::Read::by_ref(&mut input)
        .take(MAX_IMPORTED_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read local agent auth file {}", source.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_IMPORTED_AUTH_BYTES,
        "local agent auth file exceeds {MAX_IMPORTED_AUTH_BYTES} bytes: {}",
        source.display()
    );

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create local agent auth directory {}",
                parent.display()
            )
        })?;
        set_private_permissions(parent)?;
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .with_context(|| {
            format!(
                "could not create private local agent auth file {}",
                destination.display()
            )
        })?;
    std::io::Write::write_all(&mut output, &bytes)?;
    set_read_only_private_file_permissions(&destination)?;
    Ok(vec![destination])
}

#[cfg(unix)]
fn set_read_only_private_file_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o400))
        .with_context(|| format!("could not protect local agent auth file {}", path.display()))
}

#[cfg(not(unix))]
fn set_read_only_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) struct PreparedRun {
    pub(crate) agent: Agent,
    pub(crate) access: Access,
    pub(crate) cwd: PathBuf,
    command: Vec<String>,
    executable_target: PathBuf,
    environment: HashMap<String, String>,
    session_roots: Vec<PathBuf>,
    task: String,
    task_bytes: usize,
    task_sha256: String,
    task_preview: String,
    state: AgentState,
}

impl PreparedRun {
    pub(crate) fn approval_detail(&self) -> String {
        let detail = format!(
            "agent: {}\ncwd: {}\naccess: {}\ntask_bytes: {}\ntask_sha256: {}\ntask_preview:\n{}",
            self.agent.as_str(),
            self.cwd.display(),
            self.access.as_str(),
            self.task_bytes,
            self.task_sha256,
            self.task_preview,
        );
        debug_assert!(detail.len() <= MAX_APPROVAL_DETAIL_BYTES);
        detail
    }

    pub(crate) fn activity_label(&self) -> String {
        format!(
            "local_agent_run agent={} cwd={} access={} task_bytes={} task_sha256={}",
            self.agent.as_str(),
            self.cwd.display(),
            self.access.as_str(),
            self.task_bytes,
            self.task_sha256
        )
    }

    pub(crate) fn approval_metadata(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("provenance".to_owned(), "local_agent_run".to_owned()),
            ("source".to_owned(), "local_agent_run".to_owned()),
            ("agent".to_owned(), self.agent.as_str().to_owned()),
            ("access".to_owned(), self.access.as_str().to_owned()),
            ("cwd".to_owned(), self.cwd.display().to_string()),
            ("scope".to_owned(), "session_cwd".to_owned()),
            ("task_bytes".to_owned(), self.task_bytes.to_string()),
            ("task_sha256".to_owned(), self.task_sha256.clone()),
        ])
    }

    pub(crate) fn revalidate(&self, session: &config::Session) -> Result<()> {
        self.revalidate_with_resolver(session, resolve_executable_details)
    }

    pub(crate) fn revalidate_with_executable(
        &self,
        session: &config::Session,
        executable: &Path,
    ) -> Result<()> {
        let executable = executable.to_owned();
        self.revalidate_with_resolver(session, move |agent, _environment, session| {
            resolve_explicit_executable(agent, &executable, session)
        })
    }

    fn revalidate_with_resolver<F>(&self, session: &config::Session, resolve: F) -> Result<()>
    where
        F: FnOnce(Agent, &HashMap<String, String>, &config::Session) -> Result<ResolvedExecutable>,
    {
        let cwd = resolve_cwd(session, Some(&self.cwd))?;
        anyhow::ensure!(
            cwd == self.cwd,
            "local agent cwd changed while approval was pending"
        );
        let executable = resolve(self.agent, &self.environment, session)?;
        anyhow::ensure!(
            self.command
                .first()
                .is_some_and(|value| value == executable.runtime.to_string_lossy().as_ref()),
            "local agent executable changed while approval was pending"
        );
        anyhow::ensure!(
            executable.canonical == self.executable_target,
            "local agent executable target changed while approval was pending"
        );
        let current_roots = canonical_session_roots(session)?;
        anyhow::ensure!(
            current_roots == self.session_roots,
            "local agent session roots changed while approval was pending"
        );
        self.state.ensure_outside_permitted_roots(&current_roots)
    }
}

pub(crate) fn prepare(args: &Value, session: &config::Session) -> Result<PreparedRun> {
    prepare_with_resolver(args, session, resolve_executable_details)
}

/// Test-only dependency injection for approval-path tests.
///
/// The public MCP schema never accepts an executable path. Production callers
/// use `prepare`, which resolves the fixed agent name from the captured PATH.
pub(crate) fn prepare_with_executable(
    args: &Value,
    session: &config::Session,
    executable: &Path,
) -> Result<PreparedRun> {
    let executable = executable.to_owned();
    prepare_with_resolver(args, session, move |agent, _environment, session| {
        resolve_explicit_executable(agent, &executable, session)
    })
}

fn prepare_with_resolver<F>(
    args: &Value,
    session: &config::Session,
    resolve: F,
) -> Result<PreparedRun>
where
    F: FnOnce(Agent, &HashMap<String, String>, &config::Session) -> Result<ResolvedExecutable>,
{
    let object = args
        .as_object()
        .context("local_agent_run arguments must be an object")?;
    anyhow::ensure!(
        object.keys().all(|key| {
            matches!(
                key.as_str(),
                "session_id" | "agent" | "task" | "cwd" | "access" | "model" | "profile"
            )
        }),
        "local_agent_run accepts only session_id, agent, task, cwd, access, model, and profile"
    );

    let requested_session_id = object
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing or non-string session_id")?;
    anyhow::ensure!(requested_session_id == session.id, "session ID mismatch");

    let agent = Agent::parse(required_string(args, "agent")?)?;
    let access = Access::parse(required_string(args, "access")?)?;
    let task = required_string(args, "task")?;
    validate_task(task, max_task_bytes(agent))?;

    let cwd = match object.get("cwd") {
        Some(value) => {
            let value = value.as_str().context("cwd must be a string")?;
            validate_text_argument(value, "cwd", MAX_CWD_BYTES)?;
            resolve_cwd(session, Some(Path::new(value)))?
        }
        None => resolve_cwd(session, None)?,
    };
    let model = optional_bounded_string(args, "model", MAX_MODEL_BYTES)?;
    let profile = optional_profile(args)?;

    let mut environment = filtered_environment()?;
    let executable = resolve(agent, &environment, session)?;
    let source_home = environment
        .get("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable for local agent authentication")?;
    anyhow::ensure!(
        source_home.is_absolute(),
        "HOME must be an absolute path for local agent authentication"
    );
    let source_home = fs::canonicalize(&source_home).with_context(|| {
        format!(
            "could not resolve local agent HOME {}",
            source_home.display()
        )
    })?;
    anyhow::ensure!(
        source_home.is_dir(),
        "local agent HOME is not a directory: {}",
        source_home.display()
    );
    let source_codex_home = optional_source_directory(environment.get("CODEX_HOME"), "CODEX_HOME")?;
    let source_xdg_data_home =
        optional_source_directory(environment.get("XDG_DATA_HOME"), "XDG_DATA_HOME")?;
    let state = AgentState::create_with_auth_sources(
        agent,
        &session.permitted_directories,
        Some(&source_home),
        source_codex_home.as_deref(),
        source_xdg_data_home.as_deref(),
    )?;
    state.apply_to_environment(agent, &mut environment);
    let executable_runtime = path_argument(&executable.runtime, "agent executable")?;
    let executable_target = executable.canonical;
    let cwd_argument = path_argument(&cwd, "agent cwd")?;
    let command = match agent {
        Agent::Codex => build_codex_command(
            &executable_runtime,
            &cwd_argument,
            access,
            model.as_deref(),
            profile.as_deref(),
            state.read_only_paths(),
        )?,
        Agent::OpenCode => {
            environment.insert(
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                opencode_config(access, state.read_only_paths())?.to_string(),
            );
            environment.insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "true".to_owned());
            environment.insert("OPENCODE_DISABLE_PRUNE".to_owned(), "true".to_owned());
            build_opencode_command(
                &executable_runtime,
                &cwd_argument,
                task,
                model.as_deref(),
                profile.as_deref(),
            )
        }
    };

    Ok(PreparedRun {
        agent,
        access,
        cwd,
        command,
        executable_target,
        environment,
        session_roots: canonical_session_roots(session)?,
        task: task.to_owned(),
        task_bytes: task.len(),
        task_sha256: task_sha256(task.as_bytes()),
        task_preview: task_preview(task),
        state,
    })
}

pub(crate) async fn run(prepared: PreparedRun) -> Result<sandbox::Output> {
    let state_root = prepared.state.root.clone();
    let mut writable_roots = Vec::new();
    if prepared.access == Access::WorkspaceWrite {
        // `session_roots` authorizes cwd selection and is revalidated across
        // approval, but it must not widen the selected workspace's write
        // capability to sibling permitted roots.
        writable_roots.push(prepared.cwd.clone());
    }
    writable_roots.push(state_root);
    let temporary_roots = [prepared.state.temporary_root()];
    let mut read_only_roots = executable_read_only_roots(&prepared)?;
    if prepared.access == Access::ReadOnly {
        read_only_roots.push(prepared.cwd.clone());
    }
    read_only_roots.sort();
    read_only_roots.dedup();
    let stdin = (prepared.agent == Agent::Codex).then_some(prepared.task.as_bytes());
    sandbox::run_local_agent(
        &prepared.command,
        &prepared.cwd,
        sandbox::LocalAgentScope {
            writable_roots: &writable_roots,
            temporary_roots: &temporary_roots,
            read_only_paths: prepared.state.read_only_paths(),
            read_only_roots: &read_only_roots,
            hidden_roots: prepared.state.hidden_roots(),
        },
        stdin,
        &prepared.environment,
    )
    .await
}

fn executable_read_only_roots(prepared: &PreparedRun) -> Result<Vec<PathBuf>> {
    let runtime = prepared
        .command
        .first()
        .context("local agent command has no executable")?;
    let runtime_parent = Path::new(runtime)
        .parent()
        .context("local agent executable has no parent")?;
    let target_parent = prepared
        .executable_target
        .parent()
        .context("local agent executable target has no parent")?;
    [runtime_parent, target_parent]
        .into_iter()
        .map(|path| {
            fs::canonicalize(path).with_context(|| {
                format!(
                    "could not resolve local agent executable directory {}",
                    path.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()
}

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing or non-string {name}"))
}

fn optional_bounded_string(args: &Value, name: &str, maximum: usize) -> Result<Option<String>> {
    let Some(value) = args.get(name) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .with_context(|| format!("{name} must be a string"))?;
    validate_text_argument(value, name, maximum)?;
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(Some(value.to_owned()))
}

fn optional_profile(args: &Value) -> Result<Option<String>> {
    let profile = optional_bounded_string(args, "profile", MAX_PROFILE_BYTES)?;
    if let Some(profile) = profile.as_deref() {
        anyhow::ensure!(
            profile
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '.'))
                && !profile.starts_with('.')
                && !profile.contains(".."),
            "profile must be a simple agent profile name"
        );
    }
    Ok(profile)
}

fn max_task_bytes(agent: Agent) -> usize {
    match agent {
        Agent::Codex => MAX_TASK_BYTES,
        Agent::OpenCode => MAX_OPENCODE_TASK_BYTES,
    }
}

fn validate_task(task: &str, maximum: usize) -> Result<()> {
    anyhow::ensure!(task.len() <= maximum, "task exceeds {maximum} bytes");
    anyhow::ensure!(!task.as_bytes().contains(&0), "task contains a NUL byte");
    anyhow::ensure!(!task.is_empty(), "task must not be empty");
    Ok(())
}

fn validate_text_argument(value: &str, name: &str, maximum: usize) -> Result<()> {
    anyhow::ensure!(value.len() <= maximum, "{name} exceeds {maximum} bytes");
    anyhow::ensure!(!value.as_bytes().contains(&0), "{name} contains a NUL byte");
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "{name} must not contain control characters"
    );
    Ok(())
}

fn resolve_cwd(session: &config::Session, path: Option<&Path>) -> Result<PathBuf> {
    let candidate = path
        .map(|path| {
            if path.is_absolute() {
                path.to_owned()
            } else {
                session.cwd.join(path)
            }
        })
        .unwrap_or_else(|| session.cwd.clone());
    let resolved = config::canonical_directory(&candidate)?;
    let permitted = session
        .permitted_directories
        .iter()
        .map(|root| config::canonical_directory(root))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .any(|root| resolved == root || resolved.starts_with(root));
    anyhow::ensure!(
        permitted,
        "local agent cwd is outside the permitted sandbox roots: {}",
        resolved.display()
    );
    anyhow::ensure!(
        !is_protected_metadata_location(&resolved),
        "local agent cwd must not be inside protected metadata: {}",
        resolved.display()
    );
    Ok(resolved)
}

fn ensure_outside_permitted_roots(
    path: &Path,
    permitted_roots: &[PathBuf],
    message: &str,
) -> Result<()> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("could not resolve local agent path {}", path.display()))?;
    for root in permitted_roots {
        let root = config::canonical_directory(root)?;
        anyhow::ensure!(
            path != root && !path.starts_with(&root),
            "{message}: {}",
            path.display()
        );
    }
    Ok(())
}

fn canonical_session_roots(session: &config::Session) -> Result<Vec<PathBuf>> {
    let mut roots = session
        .permitted_directories
        .iter()
        .map(|root| config::canonical_directory(root))
        .collect::<Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn optional_source_directory(value: Option<&String>, name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    anyhow::ensure!(path.is_absolute(), "{name} must be an absolute path");
    match fs::canonicalize(&path) {
        Ok(canonical) => {
            anyhow::ensure!(
                canonical.is_dir(),
                "{name} is not a directory: {}",
                path.display()
            );
            Ok(Some(canonical))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(path)),
        Err(error) => Err(error)
            .with_context(|| format!("could not resolve local agent {name} {}", path.display())),
    }
}

fn is_protected_metadata_location(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        matches!(name.to_str(), Some(".git" | ".agents" | ".codex"))
    })
}

fn filtered_environment() -> Result<HashMap<String, String>> {
    let captured = approvals::CapturedStartEnvironment::capture();
    captured.validate()?;
    filtered_environment_values(captured.values())
}

fn filtered_environment_values(
    captured: &BTreeMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut environment = HashMap::new();
    let mut total = 0usize;
    for name in SAFE_ENV_NAMES {
        if child_env::SENSITIVE_ENV_NAMES.contains(name) || name.starts_with("TEMOTE_MCP_") {
            continue;
        }
        let Some(value) = captured.get(*name) else {
            continue;
        };
        anyhow::ensure!(
            value.len() <= MAX_ENV_VALUE_BYTES,
            "local agent environment value {name} exceeds {MAX_ENV_VALUE_BYTES} bytes"
        );
        total = total
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .context("local agent environment size overflow")?;
        anyhow::ensure!(
            total <= MAX_ENV_TOTAL_BYTES,
            "local agent environment exceeds {MAX_ENV_TOTAL_BYTES} bytes"
        );
        environment.insert((*name).to_owned(), value.clone());
    }
    anyhow::ensure!(
        environment.contains_key("PATH"),
        "PATH is unavailable for local agent execution"
    );
    anyhow::ensure!(
        environment.contains_key("HOME"),
        "HOME is unavailable for local agent execution"
    );
    environment.insert("NO_COLOR".to_owned(), "1".to_owned());
    environment.insert("TEMOTE_MCP_LOCAL_AGENT".to_owned(), "1".to_owned());
    Ok(environment)
}

#[derive(Clone, Debug)]
struct ResolvedExecutable {
    runtime: PathBuf,
    canonical: PathBuf,
}

#[cfg(test)]
fn resolve_executable(
    agent: Agent,
    environment: &HashMap<String, String>,
    session: &config::Session,
) -> Result<PathBuf> {
    Ok(resolve_executable_details(agent, environment, session)?.runtime)
}

fn resolve_executable_details(
    agent: Agent,
    environment: &HashMap<String, String>,
    session: &config::Session,
) -> Result<ResolvedExecutable> {
    let path = environment
        .get("PATH")
        .context("PATH is unavailable for executable resolution")?;
    let session_roots = canonical_session_roots(session)?;
    for directory in env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let Ok(canonical_directory) = fs::canonicalize(&directory) else {
            continue;
        };
        if session_roots
            .iter()
            .any(|root| canonical_directory == *root || canonical_directory.starts_with(root))
        {
            continue;
        }
        let candidate = directory.join(agent.executable_name());
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        let Ok(metadata) = fs::metadata(&canonical) else {
            continue;
        };
        if !metadata.is_file() || !is_executable(&metadata) {
            continue;
        }
        if session_roots
            .iter()
            .any(|root| canonical == *root || canonical.starts_with(root))
        {
            continue;
        }
        return Ok(ResolvedExecutable {
            runtime: candidate,
            canonical,
        });
    }
    anyhow::bail!(
        "{} executable was not found on an absolute PATH entry outside the session roots",
        agent.executable_name()
    )
}

fn resolve_explicit_executable(
    agent: Agent,
    executable: &Path,
    session: &config::Session,
) -> Result<ResolvedExecutable> {
    anyhow::ensure!(
        executable.is_absolute(),
        "injected local agent executable must be absolute"
    );
    anyhow::ensure!(
        executable.file_name().and_then(|name| name.to_str()) == Some(agent.executable_name()),
        "injected local agent executable has the wrong fixed name"
    );
    let canonical = fs::canonicalize(executable).with_context(|| {
        format!(
            "could not resolve agent executable {}",
            executable.display()
        )
    })?;
    let metadata = fs::metadata(&canonical).with_context(|| {
        format!(
            "could not inspect agent executable {}",
            executable.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_file() && is_executable(&metadata),
        "agent executable is not an executable regular file: {}",
        executable.display()
    );
    let session_roots = canonical_session_roots(session)?;
    let candidate_parent = executable
        .parent()
        .context("injected local agent executable has no parent")?;
    let canonical_parent = fs::canonicalize(candidate_parent).with_context(|| {
        format!(
            "could not resolve injected agent executable directory {}",
            candidate_parent.display()
        )
    })?;
    let candidate_inside_session_root = session_roots
        .iter()
        .any(|root| canonical_parent == *root || canonical_parent.starts_with(root));
    let target_inside_session_root = session_roots
        .iter()
        .any(|root| canonical == *root || canonical.starts_with(root));
    anyhow::ensure!(
        !candidate_inside_session_root && !target_inside_session_root,
        "agent executable path or target is inside a permitted session root: {}",
        executable.display()
    );
    Ok(ResolvedExecutable {
        runtime: executable.to_owned(),
        canonical,
    })
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

fn path_argument(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))
}

fn build_codex_command(
    executable: &str,
    cwd: &str,
    access: Access,
    model: Option<&str>,
    profile: Option<&str>,
    auth_paths: &[PathBuf],
) -> Result<Vec<String>> {
    let mut command = vec![
        executable.to_owned(),
        "exec".to_owned(),
        "--ignore-user-config".to_owned(),
        "--ignore-rules".to_owned(),
        "--ephemeral".to_owned(),
        "--skip-git-repo-check".to_owned(),
        // The broker supplies a permission profile with an exact deny rule
        // for imported auth. `--strict-config` makes an older or incompatible
        // Codex fail closed instead of silently falling back to broad reads.
        "--strict-config".to_owned(),
        "--color".to_owned(),
        "never".to_owned(),
        "--cd".to_owned(),
        cwd.to_owned(),
        "--json".to_owned(),
    ];
    if let Some(model) = model {
        command.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(profile) = profile {
        command.extend(["--profile".to_owned(), profile.to_owned()]);
    }
    command.extend([
        "--config".to_owned(),
        format!(
            "default_permissions={}",
            toml_basic_string(CODEX_PERMISSION_PROFILE_NAME)
        ),
        "--config".to_owned(),
        codex_permission_profile_filesystem(cwd, access, auth_paths)?,
        "--config".to_owned(),
        format!("permissions.{CODEX_PERMISSION_PROFILE_NAME}.network={{enabled=true}}"),
    ]);
    // Codex formally accepts `-` as the prompt source for stdin. Keep the
    // task body out of argv and the process list so the full task byte limit
    // is independent of execve's per-argument limit.
    command.extend(["--".to_owned(), "-".to_owned()]);
    Ok(command)
}

fn build_opencode_command(
    executable: &str,
    cwd: &str,
    task: &str,
    model: Option<&str>,
    profile: Option<&str>,
) -> Vec<String> {
    let mut command = vec![
        executable.to_owned(),
        "run".to_owned(),
        "--pure".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        "--dir".to_owned(),
        cwd.to_owned(),
    ];
    if let Some(model) = model {
        command.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(profile) = profile {
        command.extend(["--agent".to_owned(), profile.to_owned()]);
    }
    command.extend(["--".to_owned(), task.to_owned()]);
    command
}

fn opencode_config(access: Access, auth_paths: &[PathBuf]) -> Result<Value> {
    let edit = if access == Access::WorkspaceWrite {
        let mut rules = serde_json::Map::from_iter([
            ("*".to_owned(), json!("allow")),
            (".git".to_owned(), json!("deny")),
            (".git/**".to_owned(), json!("deny")),
            ("**/.git".to_owned(), json!("deny")),
            ("**/.git/**".to_owned(), json!("deny")),
            (".agents".to_owned(), json!("deny")),
            (".agents/**".to_owned(), json!("deny")),
            ("**/.agents".to_owned(), json!("deny")),
            ("**/.agents/**".to_owned(), json!("deny")),
            (".codex".to_owned(), json!("deny")),
            (".codex/**".to_owned(), json!("deny")),
            ("**/.codex".to_owned(), json!("deny")),
            ("**/.codex/**".to_owned(), json!("deny")),
        ]);
        for path in auth_paths {
            rules.insert(path_argument(path, "local agent auth path")?, json!("deny"));
        }
        Value::Object(rules)
    } else {
        json!("deny")
    };

    let read = if auth_paths.is_empty() {
        json!("allow")
    } else {
        let mut rules = serde_json::Map::new();
        rules.insert("*".to_owned(), json!("allow"));
        for path in auth_paths {
            rules.insert(path_argument(path, "local agent auth path")?, json!("deny"));
        }
        Value::Object(rules)
    };
    Ok(json!({
        "permission": {
            "read": read,
            "edit": edit,
            "bash": "deny",
            "external_directory": "deny",
            "webfetch": "allow",
            "websearch": "allow"
        }
    }))
}

fn codex_permission_profile_filesystem(
    cwd: &str,
    access: Access,
    auth_paths: &[PathBuf],
) -> Result<String> {
    let mut entries = vec![
        format!("{}=\"read\"", toml_basic_string(":root")),
        format!("{}=\"read\"", toml_basic_string(":minimal")),
        format!("{}=\"write\"", toml_basic_string(":tmpdir")),
    ];
    for path in auth_paths {
        let path = path.to_str().with_context(|| {
            format!(
                "local agent auth path is not valid UTF-8: {}",
                path.display()
            )
        })?;
        entries.push(format!("{}=\"deny\"", toml_basic_string(path)));
    }
    entries.push(format!(
        "{}=\"{}\"",
        toml_basic_string(cwd),
        match access {
            Access::ReadOnly => "read",
            Access::WorkspaceWrite => "write",
        }
    ));
    Ok(format!(
        "permissions.{CODEX_PERMISSION_PROFILE_NAME}.filesystem={{{}}}",
        entries.join(",")
    ))
}

fn toml_basic_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                write!(quoted, "\\u{:04x}", character as u32)
                    .expect("writing a TOML string to String cannot fail");
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn task_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn task_preview(task: &str) -> String {
    let truncation_suffix_len = TASK_PREVIEW_TRUNCATION_MARKER.len() + "\n  ".len();
    let body_limit = MAX_TASK_PREVIEW_BYTES.saturating_sub(truncation_suffix_len);
    let mut preview = String::from("  ");
    let mut character_count = 0usize;
    let mut line_count = 1usize;
    let mut truncated = false;

    for character in task.chars() {
        if character_count >= MAX_TASK_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        if character == '\n' {
            if line_count >= MAX_TASK_PREVIEW_LINES || preview.len() + "\n  ".len() > body_limit {
                truncated = true;
                break;
            }
            preview.push('\n');
            preview.push_str("  ");
            line_count += 1;
            character_count += 1;
            continue;
        }

        let rendered = if character.is_control() {
            "�".to_owned()
        } else {
            character.to_string()
        };
        if preview.len() + rendered.len() > body_limit {
            truncated = true;
            break;
        }
        preview.push_str(&rendered);
        character_count += 1;
    }

    if truncated {
        preview.push_str("\n  ");
        preview.push_str(TASK_PREVIEW_TRUNCATION_MARKER);
    }
    debug_assert!(preview.len() <= MAX_TASK_PREVIEW_BYTES);
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn make_executable_with_contents(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn session(root: &Path) -> config::Session {
        let root = fs::canonicalize(root).unwrap();
        config::Session {
            id: "local-agent-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        }
    }

    fn args(agent: &str, access: &str) -> Value {
        json!({
            "session_id": "local-agent-test",
            "agent": agent,
            "task": "change one bounded file",
            "access": access
        })
    }

    #[test]
    fn unknown_agent_is_rejected() {
        assert!(Agent::parse("shell").is_err());
        assert!(Agent::parse("/usr/bin/codex").is_err());
    }

    #[test]
    fn arbitrary_executable_input_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path());
        let mut value = args("codex", "workspace_write");
        value["executable"] = Value::String("/bin/sh".to_owned());
        assert!(prepare(&value, &session).is_err());
    }

    #[test]
    fn cwd_outside_session_root_is_rejected_even_for_yolo() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut session = session(root.path());
        session.yolo = true;
        let mut value = args("codex", "workspace_write");
        value["cwd"] = Value::String(outside.path().to_string_lossy().into_owned());
        assert!(resolve_cwd(&session, Some(Path::new(value["cwd"].as_str().unwrap()))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cwd_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("escape");
        symlink(outside.path(), &link).unwrap();
        let session = session(root.path());
        assert!(resolve_cwd(&session, Some(&link)).is_err());
    }

    #[test]
    fn task_and_profile_bounds_fail_closed() {
        assert!(validate_task("", MAX_TASK_BYTES).is_err());
        assert!(validate_task(&"x".repeat(MAX_TASK_BYTES), MAX_TASK_BYTES).is_ok());
        assert!(validate_task(&"x".repeat(MAX_TASK_BYTES + 1), MAX_TASK_BYTES).is_err());
        assert!(validate_task("first line\nsecond line", MAX_TASK_BYTES).is_ok());
        assert!(validate_task("task\0injection", MAX_TASK_BYTES).is_err());
        assert!(
            validate_task(
                &"x".repeat(MAX_OPENCODE_TASK_BYTES),
                MAX_OPENCODE_TASK_BYTES
            )
            .is_ok()
        );
        assert!(
            validate_task(
                &"x".repeat(MAX_OPENCODE_TASK_BYTES + 1),
                MAX_OPENCODE_TASK_BYTES
            )
            .is_err()
        );
        assert!(optional_profile(&json!({"profile": "../escape"})).is_err());
        assert!(validate_text_argument("line\nfeed", "model", MAX_MODEL_BYTES).is_err());
    }

    #[test]
    fn task_transport_keeps_codex_body_out_of_argv_and_bounds_opencode_message() {
        let codex_task = "x".repeat(MAX_TASK_BYTES);
        let codex_command = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::ReadOnly,
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(codex_command.last(), Some(&"-".to_owned()));
        assert!(!codex_command.iter().any(|argument| argument == &codex_task));
        assert_eq!(codex_task.len(), MAX_TASK_BYTES);

        let opencode_task = "y".repeat(MAX_OPENCODE_TASK_BYTES);
        let opencode_command = build_opencode_command(
            "/usr/bin/opencode",
            "/workspace",
            &opencode_task,
            None,
            None,
        );
        assert_eq!(opencode_command.last(), Some(&opencode_task));
        assert_eq!(opencode_task.len(), max_task_bytes(Agent::OpenCode));
        assert!(opencode_task.len() < 128 * 1024);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_task_is_delivered_exactly_over_stdin() -> Result<()> {
        let root = tempfile::tempdir().unwrap();
        let state = AgentState::create(Agent::Codex, &[]).unwrap();
        let task = "x".repeat(MAX_TASK_BYTES);
        let task_sha = task_sha256(task.as_bytes());
        let prepared = PreparedRun {
            agent: Agent::Codex,
            access: Access::ReadOnly,
            cwd: root.path().canonicalize().unwrap(),
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), "wc -c".to_owned()],
            executable_target: PathBuf::from("/bin/sh"),
            environment: {
                let mut environment = HashMap::new();
                state.apply_to_environment(Agent::Codex, &mut environment);
                environment
            },
            session_roots: Vec::new(),
            task: task.clone(),
            task_bytes: task.len(),
            task_sha256: task_sha.clone(),
            task_preview: task_preview(&task),
            state,
        };
        assert_eq!(prepared.task_sha256, task_sha);
        assert!(!prepared.command.iter().any(|argument| argument == &task));

        let prepared_sha = prepared.task_sha256.clone();
        let output = run(prepared).await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert_eq!(output.stdout.trim(), task.len().to_string());
        assert_eq!(prepared_sha, task_sha);
        assert!(!output.truncated);
        Ok(())
    }

    #[test]
    fn environment_allowlist_excludes_credentials_and_proxy_values() {
        let captured = BTreeMap::from([
            ("HOME".to_owned(), "/home/test".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("LC_ALL".to_owned(), "C".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "secret-a".to_owned()),
            ("OP_SERVICE_ACCOUNT_TOKEN".to_owned(), "secret-b".to_owned()),
            (
                "HTTPS_PROXY".to_owned(),
                "https://user:secret@example.invalid".to_owned(),
            ),
        ]);
        let environment = filtered_environment_values(&captured).unwrap();
        assert_eq!(environment.get("HOME"), Some(&"/home/test".to_owned()));
        assert_eq!(environment.get("LC_ALL"), Some(&"C".to_owned()));
        assert!(!environment.contains_key("OPENAI_API_KEY"));
        assert!(!environment.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(!environment.contains_key("HTTPS_PROXY"));
    }

    #[test]
    fn codex_access_contract_has_no_bypass_or_caller_executable() {
        let read_only = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::ReadOnly,
            Some("gpt-test"),
            Some("luna-max"),
            &[],
        )
        .unwrap();
        assert_eq!(read_only[0], "/usr/bin/codex");
        assert!(read_only.contains(&"--ignore-user-config".to_owned()));
        assert!(read_only.contains(&"--strict-config".to_owned()));
        assert!(read_only.contains(&"--ephemeral".to_owned()));
        assert!(!read_only.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(
            read_only
                .iter()
                .all(|value| !value.contains("dangerously-bypass-approvals-and-sandbox"))
        );
        assert_eq!(read_only.last(), Some(&"-".to_owned()));
        assert!(
            read_only
                .iter()
                .any(|value| { value.contains("default_permissions=\"temote_local_agent\"") })
        );
    }

    #[test]
    fn read_only_and_workspace_write_have_distinct_agent_contracts() {
        let read_only = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::ReadOnly,
            None,
            None,
            &[],
        )
        .unwrap();
        let write = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::WorkspaceWrite,
            None,
            None,
            &[],
        )
        .unwrap();
        assert!(
            read_only
                .iter()
                .any(|value| value.contains("\"/workspace\"=\"read\""))
        );
        assert!(
            write
                .iter()
                .any(|value| value.contains("\"/workspace\"=\"write\""))
        );
    }

    #[test]
    fn opencode_contract_uses_json_and_denies_shell() {
        let command = build_opencode_command(
            "/usr/bin/opencode",
            "/workspace",
            "task",
            Some("provider/model"),
            Some("default"),
        );
        assert_eq!(command[0], "/usr/bin/opencode");
        assert!(command.contains(&"run".to_owned()));
        assert!(command.windows(2).any(|pair| pair == ["--format", "json"]));
        assert!(!command.contains(&"--dangerously-skip-permissions".to_owned()));
        assert_eq!(
            opencode_config(Access::WorkspaceWrite, &[]).unwrap()["permission"]["bash"],
            "deny"
        );
        assert_eq!(
            opencode_config(Access::WorkspaceWrite, &[]).unwrap()["permission"]["edit"]["**/.git/**"],
            "deny"
        );
        assert_eq!(
            opencode_config(Access::ReadOnly, &[]).unwrap()["permission"]["edit"],
            "deny"
        );
    }

    #[test]
    fn approval_preview_is_bounded_and_durable_summaries_omit_task_body() {
        let state = AgentState::create(Agent::Codex, &[]).unwrap();
        let task = format!(
            "visible task\n{}\nSENTINEL-TASK-SECRET",
            "x".repeat(MAX_TASK_PREVIEW_CHARS + 32)
        );
        let prepared = PreparedRun {
            agent: Agent::Codex,
            access: Access::ReadOnly,
            cwd: PathBuf::from("/workspace"),
            command: vec!["/usr/bin/codex".to_owned()],
            executable_target: PathBuf::from("/usr/bin/codex"),
            environment: HashMap::from([("OPENAI_API_KEY".to_owned(), "secret".to_owned())]),
            session_roots: Vec::new(),
            task: String::new(),
            task_bytes: task.len(),
            task_sha256: task_sha256(task.as_bytes()),
            task_preview: task_preview(&task),
            state,
        };
        let detail = prepared.approval_detail();
        assert!(detail.contains("task_preview:"));
        assert!(detail.contains("visible task"));
        assert!(detail.contains("[truncated]"));
        assert!(!detail.contains("SENTINEL-TASK-SECRET"));
        assert!(detail.len() <= MAX_APPROVAL_DETAIL_BYTES);
        assert!(!prepared.activity_label().contains("visible task"));
        assert!(!prepared.activity_label().contains("SENTINEL-TASK-SECRET"));
        let metadata = prepared.approval_metadata();
        assert_eq!(metadata["task_bytes"], task.len().to_string());
        assert_eq!(metadata["task_sha256"], task_sha256(task.as_bytes()));
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("SENTINEL-TASK-SECRET")
        );
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("visible task")
        );
    }

    #[test]
    fn task_preview_sanitizes_controls_and_sha256_is_deterministic() {
        let preview = task_preview("line\t\u{1b}[31m\nsecond line\r\nthird");
        assert!(preview.contains("line��[31m"));
        assert!(preview.contains("\n  second line�\n"));
        assert!(!preview.contains('\t'));
        assert!(!preview.contains('\r'));
        assert!(!preview.contains('\u{1b}'));
        assert_eq!(
            task_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(task_sha256(b"same"), task_sha256(b"same"));
    }

    #[test]
    fn codex_auth_permission_profile_denies_only_imported_auth() {
        let auth_path = PathBuf::from("/tmp/temote-local-agent/codex/auth.json");
        let command = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::WorkspaceWrite,
            None,
            None,
            std::slice::from_ref(&auth_path),
        )
        .unwrap();
        let profile = command
            .iter()
            .find(|value| value.starts_with("permissions.temote_local_agent.filesystem="))
            .unwrap();
        assert!(profile.contains("\"/tmp/temote-local-agent/codex/auth.json\"=\"deny\""));
        assert!(profile.contains("\"/workspace\"=\"write\""));
        assert!(command.contains(&"--strict-config".to_owned()));
    }

    #[test]
    fn opencode_auth_permission_rules_deny_imported_auth() {
        let auth_path = PathBuf::from("/tmp/temote-local-agent/data/opencode/auth.json");
        let config =
            opencode_config(Access::WorkspaceWrite, std::slice::from_ref(&auth_path)).unwrap();
        assert_eq!(
            config["permission"]["read"][auth_path.to_string_lossy().as_ref()],
            "deny"
        );
        assert_eq!(
            config["permission"]["edit"][auth_path.to_string_lossy().as_ref()],
            "deny"
        );
    }

    #[test]
    fn state_directory_is_private_and_agent_specific() {
        let state = AgentState::create(Agent::Codex, &[]).unwrap();
        assert!(state.root.join("codex").is_dir());
        assert!(state.root.join("home").is_dir());
        assert!(state.root.join("tmp").is_dir());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&state.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_authentication_is_imported_without_exposing_user_state() {
        let source = tempfile::tempdir().unwrap();
        let codex_home = source.path().join(".codex");
        fs::create_dir(&codex_home).unwrap();
        let source_auth = codex_home.join("auth.json");
        fs::write(&source_auth, br#"{"provider":"test"}"#).unwrap();
        fs::set_permissions(&source_auth, fs::Permissions::from_mode(0o600)).unwrap();

        let state =
            AgentState::create_with_source_home(Agent::Codex, &[], Some(source.path())).unwrap();
        let mut environment = HashMap::from([
            ("HOME".to_owned(), "/unused".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]);
        state.apply_to_environment(Agent::Codex, &mut environment);

        let private_auth = state.root.join("codex/auth.json");
        assert_eq!(
            fs::read(&private_auth).unwrap(),
            fs::read(&source_auth).unwrap()
        );
        assert_eq!(
            environment["HOME"],
            state.root.join("home").to_string_lossy()
        );
        assert_eq!(
            environment["CODEX_HOME"],
            state.root.join("codex").to_string_lossy()
        );
        assert_eq!(state.read_only_paths(), std::slice::from_ref(&private_auth));
        assert_eq!(
            fs::metadata(&private_auth).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert!(
            fs::OpenOptions::new()
                .write(true)
                .open(&private_auth)
                .is_err()
        );
        assert_eq!(fs::read(&source_auth).unwrap(), br#"{"provider":"test"}"#);

        let opencode_data = source.path().join(".local/share/opencode");
        fs::create_dir_all(&opencode_data).unwrap();
        let source_opencode_auth = opencode_data.join("auth.json");
        fs::write(&source_opencode_auth, br#"{"provider":"test"}"#).unwrap();
        fs::set_permissions(&source_opencode_auth, fs::Permissions::from_mode(0o600)).unwrap();
        let opencode_state =
            AgentState::create_with_source_home(Agent::OpenCode, &[], Some(source.path())).unwrap();
        let mut opencode_environment = HashMap::from([
            ("HOME".to_owned(), "/unused".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ]);
        opencode_state.apply_to_environment(Agent::OpenCode, &mut opencode_environment);
        let private_opencode_auth = opencode_state.root.join("data/opencode/auth.json");
        assert_eq!(
            fs::read(&private_opencode_auth).unwrap(),
            fs::read(&source_opencode_auth).unwrap()
        );
        assert_eq!(
            opencode_environment["XDG_DATA_HOME"],
            opencode_state.root.join("data").to_string_lossy()
        );
        assert_eq!(
            opencode_state.read_only_paths(),
            std::slice::from_ref(&private_opencode_auth)
        );

        let custom_codex_home = source.path().join("custom-codex");
        fs::create_dir(&custom_codex_home).unwrap();
        let custom_auth = custom_codex_home.join("auth.json");
        fs::write(&custom_auth, br#"{"provider":"custom"}"#).unwrap();
        let custom_state = AgentState::create_with_auth_sources(
            Agent::Codex,
            &[],
            Some(source.path()),
            Some(&custom_codex_home),
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read(custom_state.root.join("codex/auth.json")).unwrap(),
            fs::read(&custom_auth).unwrap()
        );
        assert!(
            custom_state
                .hidden_roots()
                .contains(&fs::canonicalize(&custom_codex_home).unwrap())
        );

        let missing_custom_home = source.path().join("missing-codex");
        let missing_custom_state = AgentState::create_with_auth_sources(
            Agent::Codex,
            &[],
            Some(source.path()),
            Some(&missing_custom_home),
            None,
        )
        .unwrap();
        assert!(!missing_custom_state.root.join("codex/auth.json").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_uses_the_shared_bounded_capture() {
        let root = tempfile::tempdir().unwrap();
        let state = AgentState::create(Agent::Codex, &[]).unwrap();
        let prepared = PreparedRun {
            agent: Agent::Codex,
            access: Access::ReadOnly,
            cwd: root.path().canonicalize().unwrap(),
            command: vec![
                "/usr/bin/head".to_owned(),
                "-c".to_owned(),
                (sandbox::MAX_COMMAND_OUTPUT_BYTES + 1).to_string(),
                "/dev/zero".to_owned(),
            ],
            environment: HashMap::from([
                (
                    "HOME".to_owned(),
                    state.root.join("home").to_string_lossy().into_owned(),
                ),
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ]),
            executable_target: PathBuf::from("/usr/bin/head"),
            session_roots: Vec::new(),
            task: String::new(),
            task_bytes: 0,
            task_sha256: String::new(),
            task_preview: String::new(),
            state,
        };
        let output = run(prepared).await.unwrap();
        assert!(output.stdout.len() + output.stderr.len() <= sandbox::MAX_COMMAND_OUTPUT_BYTES);
        assert!(output.truncated);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn local_agent_workspace_visibility_and_write_scope_is_limited_to_selected_cwd() {
        #[cfg(target_os = "macos")]
        if std::env::var_os("NIX_BUILD_TOP").is_some()
            || std::env::var_os("TEMOTE_MCP_SANDBOX").is_some()
        {
            return;
        }
        let fixture_parent_path = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let fixture_parent = tempfile::tempdir_in(&fixture_parent_path).unwrap();
        let root_a = tempfile::tempdir_in(fixture_parent.path()).unwrap();
        let selected = root_a.path().join("selected");
        let sibling = root_a.path().join("sibling");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&sibling).unwrap();
        let root_b = tempfile::tempdir_in(fixture_parent.path()).unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        let executable = agent_dir.path().join("codex");
        let selected_input = selected.join("selected-input");
        let sibling_input = sibling.join("sibling-input");
        let extra_input = root_b.path().join("extra-input");
        fs::write(&selected_input, "selected").unwrap();
        fs::write(&sibling_input, "sibling").unwrap();
        fs::write(&extra_input, "extra").unwrap();
        let selected_marker = selected.join("selected-marker");
        let sibling_marker = sibling.join("sibling-marker");
        let extra_marker = root_b.path().join("extra-marker");
        make_executable_with_contents(
            &executable,
            &format!(
                "#!/bin/sh\n\
                 test \"$(/usr/bin/cat \"{}\" 2>/dev/null)\" = selected || exit 10\n\
                 if /usr/bin/cat \"{}\" >/dev/null 2>&1; then exit 11; fi\n\
                 if /usr/bin/cat \"{}\" >/dev/null 2>&1; then exit 12; fi\n\
                 case \"$1\" in\n\
                   workspace_write)\n\
                     /usr/bin/touch \"{}\" || exit 13\n\
                     if /usr/bin/touch \"{}\" 2>/dev/null; then exit 14; fi\n\
                     if /usr/bin/touch \"{}\" 2>/dev/null; then exit 15; fi\n\
                     ;;\n\
                   read_only)\n\
                     if /usr/bin/touch \"{}\" 2>/dev/null; then exit 16; fi\n\
                     if /usr/bin/touch \"{}\" 2>/dev/null; then exit 17; fi\n\
                     if /usr/bin/touch \"{}\" 2>/dev/null; then exit 18; fi\n\
                     ;;\n\
                   *) exit 19 ;;\n\
                 esac\n\
                 exit 0\n",
                selected_input.display(),
                sibling_input.display(),
                extra_input.display(),
                selected_marker.display(),
                sibling_marker.display(),
                extra_marker.display(),
                selected_marker.display(),
                sibling_marker.display(),
                extra_marker.display(),
            ),
        );

        let prepare = |access| {
            let state =
                AgentState::create_with_source_home(Agent::Codex, &[], Some(fixture_parent.path()))
                    .unwrap();
            let mut environment = HashMap::new();
            state.apply_to_environment(Agent::Codex, &mut environment);
            PreparedRun {
                agent: Agent::Codex,
                access,
                cwd: selected.canonicalize().unwrap(),
                executable_target: executable.canonicalize().unwrap(),
                environment,
                session_roots: vec![
                    root_a.path().canonicalize().unwrap(),
                    root_b.path().canonicalize().unwrap(),
                ],
                command: vec![
                    executable.to_string_lossy().into_owned(),
                    access.as_str().to_owned(),
                ],
                task: "test".to_owned(),
                task_bytes: 4,
                task_sha256: task_sha256(b"test"),
                task_preview: task_preview("test"),
                state,
            }
        };

        let output = run(prepare(Access::WorkspaceWrite)).await.unwrap();
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert!(selected_marker.is_file());
        assert!(!sibling_marker.exists());
        assert!(!extra_marker.exists());

        fs::remove_file(&selected_marker).unwrap();
        let output = run(prepare(Access::ReadOnly)).await.unwrap();
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert!(!selected_marker.exists());
        assert!(!sibling_marker.exists());
        assert!(!extra_marker.exists());
    }

    #[test]
    #[cfg(unix)]
    fn executable_resolution_preserves_symlink_candidate_and_validates_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let safe = tempfile::tempdir().unwrap();
        let session = session(root.path());
        let workspace_binary = root.path().join("codex");
        make_executable(&workspace_binary);
        let multicall_target = safe.path().join("vp");
        make_executable(&multicall_target);
        let safe_binary = safe.path().join("codex");
        symlink(&multicall_target, &safe_binary).unwrap();
        let path = env::join_paths([
            OsString::from("."),
            root.path().as_os_str().to_owned(),
            safe.path().as_os_str().to_owned(),
        ])
        .unwrap()
        .to_string_lossy()
        .into_owned();
        let environment = HashMap::from([
            (
                "HOME".to_owned(),
                safe.path().to_string_lossy().into_owned(),
            ),
            ("PATH".to_owned(), path),
        ]);
        let resolved = resolve_executable(Agent::Codex, &environment, &session).unwrap();
        assert_eq!(resolved, safe_binary);
        assert_eq!(
            fs::canonicalize(&resolved).unwrap(),
            fs::canonicalize(multicall_target).unwrap()
        );
    }

    #[test]
    #[cfg(unix)]
    fn executable_resolution_rejects_a_candidate_in_a_session_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let safe = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let target = safe.path().join("vp");
        make_executable(&target);
        symlink(&target, bin.join("codex")).unwrap();

        let environment = HashMap::from([
            (
                "HOME".to_owned(),
                safe.path().to_string_lossy().into_owned(),
            ),
            ("PATH".to_owned(), bin.to_string_lossy().into_owned()),
        ]);
        let error = resolve_executable_details(Agent::Codex, &environment, &session(root.path()))
            .unwrap_err();
        assert!(error.to_string().contains("outside the session roots"));
    }

    #[test]
    #[cfg(unix)]
    fn approval_revalidation_rejects_a_changed_symlink_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let safe = tempfile::tempdir().unwrap();
        let target_a = safe.path().join("vp-a");
        let target_b = safe.path().join("vp-b");
        make_executable(&target_a);
        make_executable(&target_b);
        let candidate = safe.path().join("codex");
        symlink(&target_a, &candidate).unwrap();

        let session = session(root.path());
        let prepared =
            prepare_with_executable(&args("codex", "read_only"), &session, &candidate).unwrap();
        std::fs::remove_file(&candidate).unwrap();
        symlink(&target_b, &candidate).unwrap();

        let error = prepared
            .revalidate_with_executable(&session, &candidate)
            .unwrap_err();
        assert!(error.to_string().contains("target changed"));
    }
}
