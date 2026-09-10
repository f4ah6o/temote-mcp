use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{approvals, child_env, config, sandbox};

pub(crate) const MAX_TASK_BYTES: usize = 1024 * 1024;
const MAX_CWD_BYTES: usize = 4096;
const MAX_MODEL_BYTES: usize = 256;
const MAX_PROFILE_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 32 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 128 * 1024;
const AGENT_STATE_DIRECTORY_PREFIX: &str = "temote-mcp-local-agent-";

const SAFE_ENV_NAMES: &[&str] = &["HOME", "PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM"];

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
}

impl AgentState {
    fn create(agent: Agent, forbidden_roots: &[PathBuf]) -> Result<Self> {
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

        let directories = match agent {
            Agent::Codex => ["tmp", "home", "codex"].as_slice(),
            Agent::OpenCode => ["tmp", "home", "config", "data", "cache", "state"].as_slice(),
        };
        for directory in directories {
            let path = root.join(directory);
            fs::create_dir(&path).with_context(|| {
                format!("could not create local agent directory {}", path.display())
            })?;
            set_private_permissions(&path)?;
        }
        Ok(Self { root })
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
            }
            Agent::OpenCode => {
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

pub(crate) struct PreparedRun {
    pub(crate) agent: Agent,
    pub(crate) access: Access,
    pub(crate) cwd: PathBuf,
    command: Vec<String>,
    environment: HashMap<String, String>,
    task_bytes: usize,
    task_hash: String,
    _state: AgentState,
}

impl PreparedRun {
    pub(crate) fn approval_detail(&self) -> String {
        format!(
            "agent: {}\ncwd: {}\naccess: {}\ntask_bytes: {}\ntask_hash: {}\ntask_input: omitted",
            self.agent.as_str(),
            self.cwd.display(),
            self.access.as_str(),
            self.task_bytes,
            self.task_hash
        )
    }

    pub(crate) fn activity_label(&self) -> String {
        format!(
            "local_agent_run agent={} cwd={} access={} task_hash={}",
            self.agent.as_str(),
            self.cwd.display(),
            self.access.as_str(),
            self.task_hash
        )
    }

    pub(crate) fn approval_metadata(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("provenance".to_owned(), "local_agent_run".to_owned()),
            ("source".to_owned(), "local_agent_run".to_owned()),
            ("agent".to_owned(), self.agent.as_str().to_owned()),
            ("access".to_owned(), self.access.as_str().to_owned()),
            ("scope".to_owned(), "session_cwd".to_owned()),
            ("task_input".to_owned(), "omitted".to_owned()),
            ("task_hash".to_owned(), self.task_hash.clone()),
        ])
    }

    pub(crate) fn revalidate(&self, session: &config::Session) -> Result<()> {
        let cwd = resolve_cwd(session, Some(&self.cwd))?;
        anyhow::ensure!(
            cwd == self.cwd,
            "local agent cwd changed while approval was pending"
        );
        let executable = resolve_executable(self.agent, &self.environment, session)?;
        anyhow::ensure!(
            self.command
                .first()
                .is_some_and(|value| value == executable.to_string_lossy().as_ref()),
            "local agent executable changed while approval was pending"
        );
        self._state
            .ensure_outside_permitted_roots(&session.permitted_directories)
    }
}

pub(crate) fn prepare(args: &Value, session: &config::Session) -> Result<PreparedRun> {
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
    validate_task(task)?;

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
    let executable = resolve_executable(agent, &environment, session)?;
    let state = AgentState::create(agent, &session.permitted_directories)?;
    state.apply_to_environment(agent, &mut environment);
    let executable = path_argument(&executable, "agent executable")?;
    let cwd_argument = path_argument(&cwd, "agent cwd")?;
    let command = match agent {
        Agent::Codex => build_codex_command(
            &executable,
            &cwd_argument,
            access,
            task,
            model.as_deref(),
            profile.as_deref(),
        ),
        Agent::OpenCode => {
            environment.insert(
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                opencode_config(access).to_string(),
            );
            environment.insert("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "true".to_owned());
            environment.insert("OPENCODE_DISABLE_PRUNE".to_owned(), "true".to_owned());
            build_opencode_command(
                &executable,
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
        environment,
        task_bytes: task.len(),
        task_hash: task_hash(task.as_bytes()),
        _state: state,
    })
}

pub(crate) async fn run(prepared: PreparedRun) -> Result<sandbox::Output> {
    sandbox::run_unrestricted_with_only_env(
        &prepared.command,
        &prepared.cwd,
        None,
        &prepared.environment,
    )
    .await
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

fn validate_task(task: &str) -> Result<()> {
    anyhow::ensure!(
        task.len() <= MAX_TASK_BYTES,
        "task exceeds {MAX_TASK_BYTES} bytes"
    );
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

fn resolve_executable(
    agent: Agent,
    environment: &HashMap<String, String>,
    session: &config::Session,
) -> Result<PathBuf> {
    let path = environment
        .get("PATH")
        .context("PATH is unavailable for executable resolution")?;
    for directory in env::split_paths(path) {
        if !directory.is_absolute() {
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
        let inside_session_root = session.permitted_directories.iter().any(|root| {
            fs::canonicalize(root)
                .map(|root| canonical == root || canonical.starts_with(root))
                .unwrap_or(false)
        });
        if inside_session_root {
            continue;
        }
        return Ok(canonical);
    }
    anyhow::bail!(
        "{} executable was not found on an absolute PATH entry outside the session roots",
        agent.executable_name()
    )
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
    task: &str,
    model: Option<&str>,
    profile: Option<&str>,
) -> Vec<String> {
    let mut command = vec![
        executable.to_owned(),
        "exec".to_owned(),
        "--ignore-user-config".to_owned(),
        "--ignore-rules".to_owned(),
        "--ephemeral".to_owned(),
        "--skip-git-repo-check".to_owned(),
        "--color".to_owned(),
        "never".to_owned(),
        "--sandbox".to_owned(),
        match access {
            Access::ReadOnly => "read-only".to_owned(),
            Access::WorkspaceWrite => "workspace-write".to_owned(),
        },
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
    command.extend(["--".to_owned(), task.to_owned()]);
    command
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

fn opencode_config(access: Access) -> Value {
    let edit = if access == Access::WorkspaceWrite {
        json!({
            "*": "allow",
            ".git": "deny",
            ".git/**": "deny",
            "**/.git": "deny",
            "**/.git/**": "deny",
            ".agents": "deny",
            ".agents/**": "deny",
            "**/.agents": "deny",
            "**/.agents/**": "deny",
            ".codex": "deny",
            ".codex/**": "deny",
            "**/.codex": "deny",
            "**/.codex/**": "deny"
        })
    } else {
        json!("deny")
    };
    json!({
        "permission": {
            "edit": edit,
            "bash": "deny",
            "external_directory": "deny",
            "webfetch": "allow",
            "websearch": "allow"
        }
    })
}

fn task_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
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
        assert!(validate_task("").is_err());
        assert!(validate_task(&"x".repeat(MAX_TASK_BYTES + 1)).is_err());
        assert!(validate_task("first line\nsecond line").is_ok());
        assert!(validate_task("task\0injection").is_err());
        assert!(optional_profile(&json!({"profile": "../escape"})).is_err());
        assert!(validate_text_argument("line\nfeed", "model", MAX_MODEL_BYTES).is_err());
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
            "task --dangerously-bypass-approvals-and-sandbox",
            Some("gpt-test"),
            Some("luna-max"),
        );
        assert_eq!(read_only[0], "/usr/bin/codex");
        assert!(
            read_only
                .windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(read_only.contains(&"--ignore-user-config".to_owned()));
        assert!(read_only.contains(&"--ephemeral".to_owned()));
        assert!(!read_only.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(read_only.contains(&"task --dangerously-bypass-approvals-and-sandbox".to_owned()));
    }

    #[test]
    fn read_only_and_workspace_write_have_distinct_agent_contracts() {
        let read_only = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::ReadOnly,
            "task",
            None,
            None,
        );
        let write = build_codex_command(
            "/usr/bin/codex",
            "/workspace",
            Access::WorkspaceWrite,
            "task",
            None,
            None,
        );
        assert!(
            read_only
                .windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert!(
            write
                .windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
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
            opencode_config(Access::WorkspaceWrite)["permission"]["bash"],
            "deny"
        );
        assert_eq!(
            opencode_config(Access::WorkspaceWrite)["permission"]["edit"]["**/.git/**"],
            "deny"
        );
        assert_eq!(
            opencode_config(Access::ReadOnly)["permission"]["edit"],
            "deny"
        );
    }

    #[test]
    fn approval_and_activity_summaries_omit_task_and_environment_values() {
        let state = AgentState::create(Agent::Codex, &[]).unwrap();
        let prepared = PreparedRun {
            agent: Agent::Codex,
            access: Access::ReadOnly,
            cwd: PathBuf::from("/workspace"),
            command: vec!["/usr/bin/codex".to_owned()],
            environment: HashMap::from([("OPENAI_API_KEY".to_owned(), "secret".to_owned())]),
            task_bytes: 32,
            task_hash: task_hash(b"SENTINEL-TASK-SECRET"),
            _state: state,
        };
        assert!(!prepared.approval_detail().contains("SENTINEL-TASK-SECRET"));
        assert!(!prepared.activity_label().contains("SENTINEL-TASK-SECRET"));
        assert!(
            !serde_json::to_string(&prepared.approval_metadata())
                .unwrap()
                .contains("secret")
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
            task_bytes: 0,
            task_hash: String::new(),
            _state: state,
        };
        let output = run(prepared).await.unwrap();
        assert!(output.stdout.len() + output.stderr.len() <= sandbox::MAX_COMMAND_OUTPUT_BYTES);
        assert!(output.truncated);
    }

    #[test]
    #[cfg(unix)]
    fn executable_resolution_skips_relative_and_session_path_entries() {
        let root = tempfile::tempdir().unwrap();
        let safe = tempfile::tempdir().unwrap();
        let session = session(root.path());
        let workspace_binary = root.path().join("codex");
        make_executable(&workspace_binary);
        let safe_binary = safe.path().join("codex");
        make_executable(&safe_binary);
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
        assert_eq!(
            resolve_executable(Agent::Codex, &environment, &session).unwrap(),
            fs::canonicalize(safe_binary).unwrap()
        );
    }
}
