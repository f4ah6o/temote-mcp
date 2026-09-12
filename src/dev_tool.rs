#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;
use serde_json::Value;

pub const MAX_DEV_TOOL_OPERATION_BYTES: usize = 64;
pub const MAX_DEV_TOOL_ARGUMENT_BYTES: usize = 8 * 1024;
pub const MAX_DEV_TOOL_ARGUMENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevTool {
    Cargo,
    Vp,
}

impl DevTool {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "cargo" => Ok(Self::Cargo),
            "vp" => Ok(Self::Vp),
            _ => anyhow::bail!("unsupported developer tool {value:?}; expected cargo or vp"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Vp => "vp",
        }
    }

    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Vp => "vp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevToolClass {
    DevOffline,
    DependencyNetwork,
    ArbitraryCodeSensitive,
    Rejected,
}

impl DevToolClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DevOffline => "dev_offline",
            Self::DependencyNetwork => "dependency_network",
            Self::ArbitraryCodeSensitive => "arbitrary_code_sensitive",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevToolClassification {
    pub class: DevToolClass,
    pub reason: &'static str,
}

impl DevToolClassification {
    fn new(class: DevToolClass, reason: &'static str) -> Self {
        Self { class, reason }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevToolRequest {
    tool: DevTool,
    operation: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

impl DevToolRequest {
    pub fn new(
        tool: DevTool,
        operation: impl Into<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> Result<Self> {
        let operation = operation.into();
        validate_operation(&operation)?;
        validate_arguments(&args)?;
        Ok(Self {
            tool,
            operation,
            args,
            cwd,
        })
    }

    pub fn tool(&self) -> DevTool {
        self.tool
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub fn classification(&self) -> DevToolClassification {
        classify(self.tool, &self.operation)
    }
}

fn validate_operation(operation: &str) -> Result<()> {
    anyhow::ensure!(
        !operation.is_empty(),
        "developer tool operation must not be empty"
    );
    anyhow::ensure!(
        operation.len() <= MAX_DEV_TOOL_OPERATION_BYTES,
        "developer tool operation exceeds {MAX_DEV_TOOL_OPERATION_BYTES} bytes"
    );
    anyhow::ensure!(
        operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "developer tool operation accepts only ASCII letters, digits, '-' and '_'"
    );
    anyhow::ensure!(
        operation
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric),
        "developer tool operation must start with an ASCII letter or digit"
    );
    Ok(())
}

fn validate_arguments(args: &[String]) -> Result<()> {
    anyhow::ensure!(
        args.len() <= MAX_DEV_TOOL_ARGUMENTS,
        "developer tool request exceeds {MAX_DEV_TOOL_ARGUMENTS} arguments"
    );
    for argument in args {
        anyhow::ensure!(
            !argument.contains('\0'),
            "developer tool arguments must not contain NUL"
        );
        anyhow::ensure!(
            argument.len() <= MAX_DEV_TOOL_ARGUMENT_BYTES,
            "developer tool argument exceeds {MAX_DEV_TOOL_ARGUMENT_BYTES} bytes"
        );
    }
    Ok(())
}

pub fn classify(tool: DevTool, operation: &str) -> DevToolClassification {
    let class = match (tool, operation) {
        (DevTool::Cargo, "fmt" | "check" | "clippy" | "test" | "build") => DevToolClass::DevOffline,
        (DevTool::Cargo, "fetch" | "install" | "update") => DevToolClass::DependencyNetwork,
        (DevTool::Vp, "check" | "lint" | "fmt" | "format" | "test" | "build" | "pack") => {
            DevToolClass::DevOffline
        }
        (DevTool::Vp, "install" | "add" | "update" | "outdated" | "info" | "rebuild") => {
            DevToolClass::DependencyNetwork
        }
        (DevTool::Vp, "run" | "exec" | "dlx") => DevToolClass::ArbitraryCodeSensitive,
        (DevTool::Vp, "upgrade" | "implode") => DevToolClass::Rejected,
        _ => DevToolClass::Rejected,
    };
    let reason = match class {
        DevToolClass::DevOffline => {
            "offline development command; the sandbox still contains project code execution"
        }
        DevToolClass::DependencyNetwork => {
            "dependency or registry operation; requires the explicit network profile"
        }
        DevToolClass::ArbitraryCodeSensitive => {
            "arbitrary code execution; never treated as safe because the executable is vp"
        }
        DevToolClass::Rejected => "operation is not part of the developer-tool contract",
    };
    DevToolClassification::new(class, reason)
}

pub(crate) struct PreparedDevToolRun {
    tool: DevTool,
    class: DevToolClass,
    operation: String,
    cwd: PathBuf,
    command: Vec<String>,
    environment: HashMap<String, String>,
    writable_roots: Vec<PathBuf>,
}

impl PreparedDevToolRun {
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn network_access(&self) -> bool {
        self.class == DevToolClass::DependencyNetwork
    }

    pub(crate) fn activity_label(&self) -> String {
        format!("{} {}", self.tool.as_str(), self.operation)
    }

    pub(crate) fn approval_detail(&self) -> String {
        format!(
            "tool: {}; operation: {}; argv: {:?}",
            self.tool.as_str(),
            self.operation,
            self.command
        )
    }

    pub(crate) fn revalidate(&self, session: &config::Session) -> Result<()> {
        let cwd = crate::local_agent::resolve_cwd(session, Some(&self.cwd))?;
        anyhow::ensure!(
            cwd == self.cwd,
            "developer-tool cwd changed since validation"
        );
        Ok(())
    }
}

pub(crate) fn prepare(args: &Value, session: &config::Session) -> Result<PreparedDevToolRun> {
    prepare_with_executable(args, session, None)
}

pub(crate) fn prepare_with_executable(
    args: &Value,
    session: &config::Session,
    executable: Option<&Path>,
) -> Result<PreparedDevToolRun> {
    const KEYS: &[&str] = &["session_id", "tool", "operation", "args", "cwd"];
    let object = args
        .as_object()
        .context("dev_tool_run arguments must be an object")?;
    for key in object.keys() {
        anyhow::ensure!(
            KEYS.contains(&key.as_str()),
            "unsupported dev_tool_run argument: {key}"
        );
    }
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id")?;
    anyhow::ensure!(session_id == session.id, "session ID mismatch");
    let tool = DevTool::parse(
        args.get("tool")
            .and_then(Value::as_str)
            .context("missing tool")?,
    )?;
    let operation = args
        .get("operation")
        .and_then(Value::as_str)
        .context("missing operation")?;
    let arguments = match args.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .context("dev_tool_run args entries must be strings")
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => anyhow::bail!("dev_tool_run args must be an array of strings"),
    };
    let cwd = match args.get("cwd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(PathBuf::from(
            value.as_str().context("cwd must be a string")?,
        )),
    };
    let request = DevToolRequest::new(tool, operation, arguments, cwd)?;
    let classification = request.classification();
    anyhow::ensure!(
        matches!(
            classification.class,
            DevToolClass::DevOffline | DevToolClass::DependencyNetwork
        ),
        "developer tool operation rejected: {}",
        classification.reason
    );
    let cwd = crate::local_agent::resolve_cwd(session, request.cwd().map(PathBuf::as_path))?;
    let writable_roots = tool_state_roots(tool);
    let program = match executable {
        Some(path) => path.to_string_lossy().into_owned(),
        None => tool.executable_name().to_owned(),
    };
    let mut command = vec![program, request.operation().to_owned()];
    command.extend(request.args().iter().cloned());
    let environment = crate::local_agent::filtered_environment()?;
    Ok(PreparedDevToolRun {
        tool,
        class: classification.class,
        operation: request.operation().to_owned(),
        cwd,
        command,
        environment,
        writable_roots,
    })
}

pub(crate) async fn run(prepared: PreparedDevToolRun) -> Result<crate::sandbox::Output> {
    let scope = crate::sandbox::DeveloperToolScope {
        writable_roots: &prepared.writable_roots,
        network_access: prepared.class == DevToolClass::DependencyNetwork,
    };
    crate::sandbox::run_developer_tool(
        &prepared.command,
        &prepared.cwd,
        scope,
        None,
        &prepared.environment,
    )
    .await
}

fn tool_state_roots(tool: DevTool) -> Vec<PathBuf> {
    let Some(home) = crate::platform_paths::home_dir() else {
        return Vec::new();
    };
    let candidates: &[&str] = match tool {
        DevTool::Cargo => &[".cargo", ".rustup"],
        DevTool::Vp => &[
            ".vite-plus",
            ".bun",
            ".npm",
            ".pnpm-store",
            ".local/share/pnpm",
            ".cache/vite-plus",
            ".cache/pnpm",
        ],
    };
    candidates
        .iter()
        .map(|relative| home.join(relative))
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use serde_json::json;

    #[test]
    fn cargo_operations_are_classified_by_table() {
        for operation in ["fmt", "check", "clippy", "test", "build"] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::DevOffline,
                "cargo {operation}"
            );
        }
        for operation in ["fetch", "install", "update"] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::DependencyNetwork,
                "cargo {operation}"
            );
        }
        for operation in [
            "run", "bench", "publish", "login", "owner", "fmt ", "FMT", "",
        ] {
            assert_eq!(
                classify(DevTool::Cargo, operation).class,
                DevToolClass::Rejected,
                "cargo {operation:?}"
            );
        }
    }

    #[test]
    fn vite_plus_operations_are_classified_by_table() {
        for operation in ["check", "lint", "fmt", "format", "test", "build", "pack"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::DevOffline,
                "vp {operation}"
            );
        }
        for operation in ["install", "add", "update", "outdated", "info", "rebuild"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::DependencyNetwork,
                "vp {operation}"
            );
        }
        for operation in ["run", "exec", "dlx"] {
            let classification = classify(DevTool::Vp, operation);
            assert_eq!(
                classification.class,
                DevToolClass::ArbitraryCodeSensitive,
                "vp {operation}"
            );
            assert!(classification.reason.contains("arbitrary code"));
        }
        for operation in ["upgrade", "implode"] {
            assert_eq!(
                classify(DevTool::Vp, operation).class,
                DevToolClass::Rejected,
                "vp {operation} must stay outside the initial contract"
            );
        }
    }

    #[test]
    fn unknown_operations_fail_closed_for_every_tool() {
        for tool in [DevTool::Cargo, DevTool::Vp] {
            assert_eq!(
                classify(tool, "definitely-not-a-subcommand").class,
                DevToolClass::Rejected
            );
        }
    }

    #[test]
    fn tool_names_reject_paths_and_unknown_values() {
        assert_eq!(DevTool::parse("cargo").unwrap(), DevTool::Cargo);
        assert_eq!(DevTool::parse("vp").unwrap(), DevTool::Vp);
        for value in [
            "/usr/bin/cargo",
            "./cargo",
            "cargo extra",
            "sh",
            "",
            "CARGO",
            "cargo\0",
        ] {
            assert!(
                DevTool::parse(value).is_err(),
                "tool selection accepted {value:?}"
            );
        }
    }

    #[test]
    fn operation_names_reject_paths_and_shell_fragments() {
        for operation in [
            "../cargo",
            "build;rm",
            "build&&rm",
            "-p",
            "build/../test",
            "build test",
        ] {
            assert!(
                DevToolRequest::new(DevTool::Cargo, operation, Vec::new(), None).is_err(),
                "operation accepted {operation:?}"
            );
        }
    }

    #[test]
    fn arguments_reject_nul_oversize_and_excessive_counts() {
        let exactly_max = "a".repeat(MAX_DEV_TOOL_ARGUMENT_BYTES);
        assert!(
            DevToolRequest::new(DevTool::Cargo, "build", vec![exactly_max.clone()], None).is_ok()
        );
        assert!(
            DevToolRequest::new(
                DevTool::Cargo,
                "build",
                vec![format!("{exactly_max}a")],
                None,
            )
            .is_err()
        );
        assert!(
            DevToolRequest::new(DevTool::Cargo, "build", vec!["a\0b".to_owned()], None).is_err()
        );
        let too_many = vec!["a".to_owned(); MAX_DEV_TOOL_ARGUMENTS + 1];
        assert!(DevToolRequest::new(DevTool::Cargo, "build", too_many, None).is_err());
    }

    #[test]
    fn request_keeps_arguments_and_cwd_without_inference() {
        let cwd = PathBuf::from("/tmp/project");
        let request = DevToolRequest::new(
            DevTool::Vp,
            "check",
            vec!["--reporter".to_owned(), "dot".to_owned()],
            Some(cwd.clone()),
        )
        .unwrap();
        assert_eq!(request.tool(), DevTool::Vp);
        assert_eq!(request.operation(), "check");
        assert_eq!(request.args(), ["--reporter", "dot"]);
        assert_eq!(request.cwd(), Some(&cwd));
        assert_eq!(request.classification().class, DevToolClass::DevOffline);
    }

    #[test]
    fn generated_operations_classify_deterministically_and_fail_closed() -> noprop::TestResult {
        test_support::run(0x4445_5654_4f4f_4c01, test_support::DEFAULT_CASES, |ctx| {
            let operation = test_support::ascii_string(ctx, 80);
            for tool in [DevTool::Cargo, DevTool::Vp] {
                let first = classify(tool, &operation);
                assert_eq!(
                    first,
                    classify(tool, &operation),
                    "classification must be pure"
                );
                if first.class == DevToolClass::Rejected {
                    continue;
                }
                let well_formed = !operation.is_empty()
                    && operation.len() <= MAX_DEV_TOOL_OPERATION_BYTES
                    && operation
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && operation
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
                assert!(
                    well_formed,
                    "classified operation must be a valid name: {operation:?}",
                );
                assert!(
                    DevToolRequest::new(tool, operation.clone(), Vec::new(), None).is_ok(),
                    "classified operation must pass request validation: {operation:?}",
                );
            }
            Ok(())
        })
    }

    fn test_session(cwd: &Path) -> config::Session {
        config::Session {
            id: "dev-tool-test-session".to_owned(),
            cwd: cwd.to_path_buf(),
            permitted_directories: vec![cwd.to_path_buf()],
            started_at: 0,
            process_id: 0,
            permission_mode: config::PermissionMode::Agent,
        }
    }

    #[test]
    fn prepare_builds_exact_argv_without_arbitrary_executables() {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let fake = root.path().join("fake-cargo");
        let session = test_session(&cwd);
        let args = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check",
            "args": ["--workspace", "--quiet"]
        });
        let prepared = prepare_with_executable(&args, &session, Some(&fake)).unwrap();
        assert_eq!(
            prepared.command,
            vec![
                fake.to_string_lossy().into_owned(),
                "check".to_owned(),
                "--workspace".to_owned(),
                "--quiet".to_owned()
            ]
        );
        assert_eq!(prepared.cwd, cwd);
        assert!(prepared.approval_detail().contains("cargo"));
        assert!(!prepared.approval_detail().contains("dev-tool-test-session"));
    }

    #[test]
    fn prepare_selects_the_classified_network_profile() {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let session = test_session(&cwd);
        let offline = json!({
            "session_id": "dev-tool-test-session",
            "tool": "vp",
            "operation": "build"
        });
        let prepared = prepare(&offline, &session).unwrap();
        assert_eq!(prepared.class, DevToolClass::DevOffline);
        assert!(!prepared.network_access());
        assert_eq!(prepared.command[0], "vp");

        let network = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "fetch"
        });
        let prepared = prepare(&network, &session).unwrap();
        assert_eq!(prepared.class, DevToolClass::DependencyNetwork);
        assert!(prepared.network_access());
    }

    #[test]
    fn prepare_rejects_dangerous_operations_and_unexpected_arguments() {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let session = test_session(&cwd);

        for (tool, operation) in [
            ("vp", "run"),
            ("vp", "exec"),
            ("vp", "dlx"),
            ("vp", "upgrade"),
            ("vp", "implode"),
            ("cargo", "run"),
            ("cargo", "publish"),
            ("cargo", "login"),
            ("cargo", "bench"),
        ] {
            let args = json!({
                "session_id": "dev-tool-test-session",
                "tool": tool,
                "operation": operation
            });
            assert!(
                prepare(&args, &session).is_err(),
                "{tool} {operation} must be rejected"
            );
        }

        let executable_key = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check",
            "executable": "/bin/sh"
        });
        assert!(prepare(&executable_key, &session).is_err());

        let wrong_args_type = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check",
            "args": "check --all"
        });
        assert!(prepare(&wrong_args_type, &session).is_err());

        let oversized = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check",
            "args": ["x".repeat(MAX_DEV_TOOL_ARGUMENT_BYTES + 1)]
        });
        assert!(prepare(&oversized, &session).is_err());

        let unknown_tool = json!({
            "session_id": "dev-tool-test-session",
            "tool": "make",
            "operation": "check"
        });
        assert!(prepare(&unknown_tool, &session).is_err());
    }

    #[test]
    fn prepare_rejects_cwd_outside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let session = test_session(&cwd);
        let args = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check",
            "cwd": outside.path().to_string_lossy()
        });
        assert!(prepare(&args, &session).is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn run_executes_in_the_bounded_sandbox_and_denies_outside_writes() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let fake = root.path().join("fake-cargo");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf 'argv=%s\\n' \"$*\"\ntouch \"$HOME/dev-tool-outside-marker\" 2>/dev/null || true\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = test_session(&cwd);
        let args = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "test",
            "args": ["--lib"]
        });
        let prepared = prepare_with_executable(&args, &session, Some(&fake)).unwrap();
        let output = run(prepared).await.unwrap();
        assert_eq!(output.status, 0, "stderr: {}", output.stderr);
        assert!(output.stdout.contains("argv=test --lib"));
        assert!(
            !crate::platform_paths::home_dir()
                .unwrap()
                .join("dev-tool-outside-marker")
                .exists()
        );
    }

    #[cfg(target_os = "macos")]
    #[ignore = "live acceptance: requires a real cargo installation"]
    #[tokio::test]
    async fn live_cargo_check_in_the_developer_sandbox() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"temote-dev-tool-smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "pub fn ok() -> bool { true }\n",
        )
        .unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let session = test_session(&cwd);
        let args = json!({
            "session_id": "dev-tool-test-session",
            "tool": "cargo",
            "operation": "check"
        });
        let prepared = prepare(&args, &session).unwrap();
        let output = run(prepared).await.unwrap();
        assert_eq!(output.status, 0, "stderr: {}", output.stderr);
        assert!(
            output.stderr.contains("Checking") || output.stdout.contains("Checking"),
            "cargo did not actually run: stdout={:?} stderr={:?}",
            output.stdout,
            output.stderr
        );
    }
}
