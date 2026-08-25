use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::{config, sandbox};

const KINTONE_ENV_NAMES: &[&str] = &[
    "KINTONE_BASE_URL",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_GUEST_SPACE_ID",
    "HTTPS_PROXY",
    "https_proxy",
];
const CHILD_RUNTIME_ENV_NAMES: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];
const MAX_CUSTOMIZE_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = sandbox::MAX_COMMAND_OUTPUT_BYTES;
const FORBIDDEN_OPTIONS: &[&str] = &[
    "--base-url",
    "--username",
    "-u",
    "--password",
    "-p",
    "--api-token",
    "--basic-auth-username",
    "--basic-auth-password",
    "--proxy",
    "--pfx-file-path",
    "--pfx-file-password",
    "--guest-space-id",
];
const PATH_OPTIONS: &[(&str, PathMode)] = &[
    ("--attachments-dir", PathMode::ReadWriteDirectory),
    ("--file-path", PathMode::ReadFile),
    ("--input", PathMode::ReadFile),
    ("-i", PathMode::ReadFile),
    ("--output", PathMode::WriteFile),
    ("-o", PathMode::WriteFile),
];

#[derive(Clone, Copy)]
enum PathMode {
    ReadFile,
    WriteFile,
    ReadWriteDirectory,
}

pub struct Bridge {
    executable_override: Option<PathBuf>,
    environment: BTreeMap<String, String>,
}

impl Bridge {
    pub(crate) fn capture_from(source: &BTreeMap<String, String>) -> Self {
        let executable_override = source.get("TEMOTE_MCP_KINTONE_CLI").map(PathBuf::from);
        let environment = KINTONE_ENV_NAMES
            .iter()
            .chain(CHILD_RUNTIME_ENV_NAMES.iter())
            .filter_map(|name| {
                source
                    .get(*name)
                    .cloned()
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        Self {
            executable_override,
            environment,
        }
    }

    pub fn configured(&self) -> bool {
        self.environment
            .get("KINTONE_BASE_URL")
            .is_some_and(|value| !value.trim().is_empty())
            && self.auth_mode().is_some()
    }

    pub fn status(&self) -> Value {
        json!({
            "configured": self.configured(),
            "executable_found": self.executable_path().is_ok(),
            "auth_mode": self.auth_mode(),
            "basic_auth_configured": self.environment.contains_key("KINTONE_BASIC_AUTH_USERNAME")
                || self.environment.contains_key("KINTONE_BASIC_AUTH_PASSWORD"),
            "guest_space_configured": self.environment.contains_key("KINTONE_GUEST_SPACE_ID"),
            "proxy_configured": self.environment.contains_key("HTTPS_PROXY")
                || self.environment.contains_key("https_proxy"),
            "supported_commands": [
                "record export",
                "record import",
                "record delete",
                "customize export",
                "customize apply",
                "plugin upload"
            ],
            "notes": [
                "credentials and the base URL must come from the temote-mcp start environment, not CLI arguments",
                "record import/export supports attachment directories",
                "KINTONE_GUEST_SPACE_ID is forwarded for guest-space capable commands",
                "PFX client-certificate flags are intentionally unsupported because cli-kintone exposes their password only through argv"
            ]
        })
    }

    pub async fn run(
        &self,
        session: &config::Session,
        cwd: &Path,
        arguments: Vec<String>,
        stdout_path: Option<PathBuf>,
    ) -> Result<Value> {
        let cwd = config::resolve_cwd(session, Some(cwd))?;
        let command_kind = validate_command(&arguments)?;
        let environment = self.validated_environment(command_kind)?;
        let arguments = validate_and_rewrite_paths(session, &cwd, arguments)?;
        preflight_local_references(session, &cwd, command_kind, &arguments)?;
        let executable = self.executable_path()?;
        let mut command = vec![executable.display().to_string()];
        command.extend(arguments);
        let environment = environment.into_iter().collect::<HashMap<_, _>>();

        if let Some(stdout_path) = stdout_path {
            anyhow::ensure!(
                matches!(command_kind, CommandKind::RecordExport),
                "stdout_path is supported only for cli-kintone record export"
            );
            let target = resolve_write_path_from_cwd(session, &cwd, &stdout_path)?;
            return run_with_stdout_file(&command, &cwd, &environment, &target).await;
        }

        let output = sandbox::run_unrestricted_with_only_env(&command, &cwd, None, &environment)
            .await
            .context("failed to run cli-kintone")?;
        Ok(json!({
            "exit_code": output.status,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "truncated": output.truncated,
        }))
    }

    fn auth_mode(&self) -> Option<&'static str> {
        let username = self
            .environment
            .get("KINTONE_USERNAME")
            .is_some_and(|value| !value.trim().is_empty());
        let password = self
            .environment
            .get("KINTONE_PASSWORD")
            .is_some_and(|value| !value.trim().is_empty());
        if username && password {
            return Some("password");
        }
        self.environment
            .get("KINTONE_API_TOKEN")
            .is_some_and(|value| !value.trim().is_empty())
            .then_some("api_token")
    }

    fn validated_environment(&self, command: CommandKind) -> Result<BTreeMap<String, String>> {
        let base_url = self
            .environment
            .get("KINTONE_BASE_URL")
            .filter(|value| !value.trim().is_empty())
            .context(
                "cli-kintone is not configured; start the session with KINTONE_BASE_URL set",
            )?;
        anyhow::ensure!(
            base_url.starts_with("https://") || base_url.starts_with("http://"),
            "KINTONE_BASE_URL must be an http:// or https:// URL"
        );
        anyhow::ensure!(
            self.environment.contains_key("KINTONE_USERNAME")
                == self.environment.contains_key("KINTONE_PASSWORD"),
            "KINTONE_USERNAME and KINTONE_PASSWORD must be set together"
        );
        anyhow::ensure!(
            self.environment.contains_key("KINTONE_BASIC_AUTH_USERNAME")
                == self.environment.contains_key("KINTONE_BASIC_AUTH_PASSWORD"),
            "KINTONE_BASIC_AUTH_USERNAME and KINTONE_BASIC_AUTH_PASSWORD must be set together"
        );
        let mut environment = self.environment.clone();
        match command {
            CommandKind::RecordDelete => {
                anyhow::ensure!(
                    self.environment
                        .get("KINTONE_API_TOKEN")
                        .is_some_and(|value| !value.trim().is_empty()),
                    "cli-kintone record delete requires KINTONE_API_TOKEN authentication"
                );
                environment.remove("KINTONE_USERNAME");
                environment.remove("KINTONE_PASSWORD");
            }
            CommandKind::CustomizeExport
            | CommandKind::CustomizeApply
            | CommandKind::PluginUpload => {
                anyhow::ensure!(
                    self.environment.contains_key("KINTONE_USERNAME")
                        && self.environment.contains_key("KINTONE_PASSWORD"),
                    "this cli-kintone command requires KINTONE_USERNAME and KINTONE_PASSWORD authentication"
                );
                environment.remove("KINTONE_API_TOKEN");
            }
            CommandKind::RecordExport | CommandKind::RecordImport => {
                anyhow::ensure!(
                    self.auth_mode().is_some(),
                    "cli-kintone authentication is not configured; set KINTONE_USERNAME and KINTONE_PASSWORD, or KINTONE_API_TOKEN, when starting the session"
                );
                if self.auth_mode() == Some("password") {
                    environment.remove("KINTONE_API_TOKEN");
                }
            }
        }
        Ok(environment)
    }

    fn executable_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.executable_override {
            anyhow::ensure!(
                path.is_absolute(),
                "TEMOTE_MCP_KINTONE_CLI must be an absolute path"
            );
            anyhow::ensure!(
                path.is_file(),
                "cli-kintone executable not found: {}",
                path.display()
            );
            return Ok(path.clone());
        }
        let path = self
            .environment
            .get("PATH")
            .and_then(|path| find_on_path("cli-kintone", path));
        path.context(
            "cli-kintone was not found in PATH; install @kintone/cli globally or set TEMOTE_MCP_KINTONE_CLI to its absolute executable path",
        )
    }
}

#[derive(Clone, Copy)]
enum CommandKind {
    RecordExport,
    RecordImport,
    RecordDelete,
    CustomizeExport,
    CustomizeApply,
    PluginUpload,
}

fn validate_command(arguments: &[String]) -> Result<CommandKind> {
    anyhow::ensure!(
        arguments.len() >= 2,
        "cli-kintone arguments must begin with a supported command pair"
    );
    let kind = match (arguments[0].as_str(), arguments[1].as_str()) {
        ("record", "export") => CommandKind::RecordExport,
        ("record", "import") => CommandKind::RecordImport,
        ("record", "delete") => CommandKind::RecordDelete,
        ("customize", "export") => CommandKind::CustomizeExport,
        ("customize", "apply") => CommandKind::CustomizeApply,
        ("plugin", "upload") => CommandKind::PluginUpload,
        _ => anyhow::bail!(
            "unsupported cli-kintone command; supported commands are record export/import/delete, customize export/apply, and plugin upload"
        ),
    };
    for argument in &arguments[2..] {
        let option = argument
            .split_once('=')
            .map_or(argument.as_str(), |(name, _)| name);
        anyhow::ensure!(
            !FORBIDDEN_OPTIONS.contains(&option),
            "{option} must not be passed to kintone_cli_run; configure it on temote-mcp start"
        );
        anyhow::ensure!(
            option != "--watch",
            "--watch is not supported by kintone_cli_run"
        );
    }
    Ok(kind)
}

fn validate_and_rewrite_paths(
    session: &config::Session,
    cwd: &Path,
    mut arguments: Vec<String>,
) -> Result<Vec<String>> {
    let mut index = 2;
    while index < arguments.len() {
        let argument = arguments[index].clone();
        let (option, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let Some((_, mode)) = PATH_OPTIONS.iter().find(|(name, _)| *name == option) else {
            index += 1;
            continue;
        };

        if let Some(value) = inline_value {
            anyhow::ensure!(!value.is_empty(), "{option} path must not be empty");
            let resolved = resolve_path(session, cwd, Path::new(value), *mode)?;
            arguments[index] = format!("{option}={}", resolved.display());
            index += 1;
            continue;
        }

        anyhow::ensure!(
            index + 1 < arguments.len(),
            "{option} requires a path value"
        );
        let value = arguments[index + 1].clone();
        anyhow::ensure!(!value.starts_with('-'), "{option} requires a path value");
        let resolved = resolve_path(session, cwd, Path::new(&value), *mode)?;
        arguments[index + 1] = resolved.display().to_string();
        index += 2;
    }
    Ok(arguments)
}

fn resolve_path(
    session: &config::Session,
    cwd: &Path,
    path: &Path,
    mode: PathMode,
) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    match mode {
        PathMode::ReadFile => {
            let resolved = config::resolve_existing_path(session, &candidate)?;
            anyhow::ensure!(resolved.is_file(), "path must point to a file");
            Ok(resolved)
        }
        PathMode::WriteFile => resolve_write_path_from_cwd(session, cwd, path),
        PathMode::ReadWriteDirectory => {
            if candidate.exists() {
                let resolved = config::resolve_existing_path(session, &candidate)?;
                anyhow::ensure!(resolved.is_dir(), "path must point to a directory");
                Ok(resolved)
            } else {
                resolve_write_path_from_cwd(session, cwd, path)
            }
        }
    }
}

fn resolve_write_path_from_cwd(
    session: &config::Session,
    cwd: &Path,
    path: &Path,
) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    config::resolve_write_path(session, &candidate)
}

async fn run_with_stdout_file(
    command: &[String],
    cwd: &Path,
    environment: &HashMap<String, String>,
    target: &Path,
) -> Result<Value> {
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .context("stdout_path file name must be valid UTF-8")?;
    let temporary =
        target.with_file_name(format!(".{file_name}.temote-{}.tmp", uuid::Uuid::new_v4()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;

    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error).context("failed to run cli-kintone");
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = tokio::fs::remove_file(&temporary).await;
            anyhow::bail!("cli-kintone stderr was not captured");
        }
    };
    let result = async {
        let (stderr, status) =
            tokio::join!(read_bounded_stderr(stderr, MAX_STDERR_BYTES), child.wait(),);
        Result::<_>::Ok((stderr?, status?))
    }
    .await;
    let (stderr, status) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error).context("failed to run cli-kintone");
        }
    };
    let exit_code = status.code().unwrap_or(-1);
    if exit_code == 0 {
        if let Err(error) = tokio::fs::rename(&temporary, target).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error).with_context(|| format!("failed to publish {}", target.display()));
        }
    } else {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    Ok(json!({
        "exit_code": exit_code,
        "stdout": "",
        "stdout_path": if exit_code == 0 { Some(target.display().to_string()) } else { None },
        "stderr": String::from_utf8_lossy(&stderr.bytes).into_owned(),
        "truncated": stderr.truncated,
    }))
}

struct BoundedStderr {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded_stderr<R>(mut reader: R, limit: usize) -> std::io::Result<BoundedStderr>
where
    R: AsyncRead + Unpin,
{
    const CHUNK: usize = 8192;
    let mut bytes = Vec::with_capacity(limit.min(CHUNK));
    let mut truncated = false;
    let mut buffer = [0_u8; CHUNK];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let kept = remaining.min(read);
        bytes.extend_from_slice(&buffer[..kept]);
        if kept < read {
            truncated = true;
        }
    }
    Ok(BoundedStderr { bytes, truncated })
}

fn preflight_local_references(
    session: &config::Session,
    cwd: &Path,
    command: CommandKind,
    arguments: &[String],
) -> Result<()> {
    match command {
        CommandKind::CustomizeApply => {
            let input = option_value(arguments, &["--input", "-i"])
                .context("customize apply requires --input")?;
            validate_customize_manifest(session, Path::new(input))?;
        }
        CommandKind::CustomizeExport => {
            let output = option_value(arguments, &["--output", "-o"])
                .map(PathBuf::from)
                .unwrap_or_else(|| cwd.join("customize-manifest.json"));
            let output = if output.is_absolute() {
                output
            } else {
                cwd.join(output)
            };
            let parent = output
                .parent()
                .context("customize export output has no parent directory")?;
            for child in ["desktop", "mobile"] {
                let generated = parent.join(child);
                if generated.exists() || std::fs::symlink_metadata(&generated).is_ok() {
                    let resolved = config::resolve_existing_path(session, &generated)?;
                    anyhow::ensure!(
                        resolved.is_dir(),
                        "customize export target must be a directory: {}",
                        generated.display()
                    );
                }
            }
        }
        CommandKind::RecordImport => {
            if let Some(attachments_dir) = option_value(arguments, &["--attachments-dir"]) {
                let attachments_dir = Path::new(attachments_dir);
                anyhow::ensure!(
                    attachments_dir.is_dir(),
                    "record import --attachments-dir must point to an existing directory"
                );
                reject_symlinks_recursively(attachments_dir)?;
                let input = option_value(arguments, &["--file-path"])
                    .context("record import requires --file-path")?;
                reject_attachment_parent_traversal(Path::new(input))?;
            }
        }
        CommandKind::RecordExport | CommandKind::RecordDelete | CommandKind::PluginUpload => {}
    }
    Ok(())
}

fn option_value<'a>(arguments: &'a [String], names: &[&str]) -> Option<&'a str> {
    let mut index = 2;
    while index < arguments.len() {
        let argument = &arguments[index];
        for name in names {
            if argument == name {
                return arguments.get(index + 1).map(String::as_str);
            }
            if let Some(value) = argument.strip_prefix(&format!("{name}=")) {
                return Some(value);
            }
        }
        index += 1;
    }
    None
}

fn validate_customize_manifest(session: &config::Session, manifest_path: &Path) -> Result<()> {
    let manifest_path = config::resolve_existing_path(session, manifest_path)?;
    anyhow::ensure!(manifest_path.is_file(), "customize manifest must be a file");
    let manifest_bytes = read_bounded_regular_file(&manifest_path, MAX_CUSTOMIZE_MANIFEST_BYTES)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("invalid customize manifest: {}", manifest_path.display()))?;
    let base = manifest_path
        .parent()
        .context("customize manifest has no parent directory")?;
    for pointer in ["/desktop/js", "/desktop/css", "/mobile/js", "/mobile/css"] {
        let Some(entries) = manifest.pointer(pointer) else {
            continue;
        };
        let entries = entries
            .as_array()
            .with_context(|| format!("customize manifest {pointer} must be an array"))?;
        for entry in entries {
            let reference = entry
                .as_str()
                .with_context(|| format!("customize manifest {pointer} entries must be strings"))?;
            if reference.starts_with("https://") || reference.starts_with("http://") {
                continue;
            }
            let reference_path = Path::new(reference);
            let candidate = if reference_path.is_absolute() {
                reference_path.to_owned()
            } else {
                base.join(reference_path)
            };
            let resolved = config::resolve_existing_path(session, &candidate).with_context(|| {
                format!(
                    "customize manifest local reference is outside permitted roots or missing: {reference}"
                )
            })?;
            anyhow::ensure!(
                resolved.is_file(),
                "customize manifest local reference must be a file: {reference}"
            );
        }
    }
    Ok(())
}

fn read_bounded_regular_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let mut file = open_readonly_nofollow(path)?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "path is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() <= max_bytes as u64,
        "file exceeds {max_bytes} bytes: {}",
        path.display()
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= max_bytes,
        "file exceeds {max_bytes} bytes: {}",
        path.display()
    );
    Ok(bytes)
}

fn open_readonly_nofollow(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("failed to open {} safely", path.display()))
}

fn reject_symlinks_recursively(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    let mut inspected = 0usize;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("failed to inspect {}", directory.display()))?
        {
            let entry = entry?;
            inspected = inspected.saturating_add(1);
            anyhow::ensure!(
                inspected <= 100_000,
                "attachment directory contains too many entries to validate safely"
            );
            let file_type = entry.file_type()?;
            anyhow::ensure!(
                !file_type.is_symlink(),
                "record import attachment directory must not contain symlinks: {}",
                entry.path().display()
            );
            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn reject_attachment_parent_traversal(csv_path: &Path) -> Result<()> {
    let file = open_readonly_nofollow(csv_path)
        .with_context(|| format!("failed to read record import CSV {}", csv_path.display()))?;
    anyhow::ensure!(
        !reader_contains_parent_traversal(file)?,
        "record import CSV may not contain parent-directory traversal while --attachments-dir is used"
    );
    Ok(())
}

fn reader_contains_parent_traversal(mut reader: impl Read) -> std::io::Result<bool> {
    let mut buffer = [0u8; 8192];
    let mut previous = [0u8; 2];
    let mut seen = 0usize;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(false);
        }
        for &byte in &buffer[..read] {
            if seen >= 2 && previous == *b".." && matches!(byte, b'/' | b'\\') {
                return Ok(true);
            }
            if seen == 0 {
                previous[0] = byte;
            } else if seen == 1 {
                previous[1] = byte;
            } else {
                previous[0] = previous[1];
                previous[1] = byte;
            }
            seen = seen.saturating_add(1);
        }
    }
}

fn find_on_path(executable: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn generated_bounded_stderr_matches_prefix_model() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        test_support::run(0x4b49_4e54_5354_4445, 512, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 0..=128);
            let len = noprop::sample_usize_in(ctx, 0..=256);
            let input = (0..len).map(|_| noprop::sample_u8(ctx)).collect::<Vec<_>>();
            runtime.block_on(async {
                use tokio::io::AsyncWriteExt as _;
                let (mut writer, reader) = tokio::io::duplex(input.len().max(1));
                writer.write_all(&input).await.unwrap();
                writer.shutdown().await.unwrap();
                let captured = read_bounded_stderr(reader, limit).await.unwrap();
                assert_eq!(captured.bytes, input[..input.len().min(limit)]);
                assert_eq!(captured.truncated, input.len() > limit);
            });
            Ok(())
        })
    }

    #[tokio::test]
    async fn stdout_file_spawn_failure_leaves_no_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("export.jsonl");
        let command = vec![
            root.path()
                .join("missing-cli-kintone")
                .display()
                .to_string(),
            "record".to_owned(),
            "export".to_owned(),
        ];
        let error = run_with_stdout_file(&command, root.path(), &HashMap::new(), &target)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed to run cli-kintone"));
        assert!(!target.exists());
        let leftovers = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.contains(".temote-") && name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "orphan temp files: {leftovers:?}");
    }

    fn bridge(environment: &[(&str, &str)]) -> Bridge {
        Bridge {
            executable_override: None,
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn session(root: &Path) -> config::Session {
        let root = config::canonical_directory(root).unwrap();
        config::Session {
            id: "kintone-cli-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        }
    }

    #[test]
    fn status_never_exposes_kintone_values() {
        let bridge = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_API_TOKEN", "secret-token"),
            ("KINTONE_GUEST_SPACE_ID", "123"),
        ]);
        let rendered = bridge.status().to_string();
        assert!(!rendered.contains("example.cybozu.com"));
        assert!(!rendered.contains("secret-token"));
        assert!(!rendered.contains("123"));
    }

    #[test]
    fn requires_command_specific_authentication() {
        let token = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_API_TOKEN", "secret-token"),
        ]);
        assert!(
            token
                .validated_environment(CommandKind::RecordDelete)
                .is_ok()
        );
        assert!(
            token
                .validated_environment(CommandKind::CustomizeApply)
                .is_err()
        );

        let password = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_USERNAME", "user"),
            ("KINTONE_PASSWORD", "secret"),
        ]);
        assert!(
            password
                .validated_environment(CommandKind::CustomizeApply)
                .is_ok()
        );
        assert!(
            password
                .validated_environment(CommandKind::RecordDelete)
                .is_err()
        );

        let both = bridge(&[
            ("KINTONE_BASE_URL", "https://example.cybozu.com"),
            ("KINTONE_USERNAME", "user"),
            ("KINTONE_PASSWORD", "secret"),
            ("KINTONE_API_TOKEN", "token"),
        ]);
        let delete_env = both
            .validated_environment(CommandKind::RecordDelete)
            .unwrap();
        assert!(delete_env.contains_key("KINTONE_API_TOKEN"));
        assert!(!delete_env.contains_key("KINTONE_USERNAME"));
        assert!(!delete_env.contains_key("KINTONE_PASSWORD"));
        let customize_env = both
            .validated_environment(CommandKind::CustomizeApply)
            .unwrap();
        assert!(customize_env.contains_key("KINTONE_USERNAME"));
        assert!(customize_env.contains_key("KINTONE_PASSWORD"));
        assert!(!customize_env.contains_key("KINTONE_API_TOKEN"));
    }

    #[test]
    fn rejects_secret_bearing_or_unknown_commands() {
        assert!(
            validate_command(&[
                "record".into(),
                "export".into(),
                "--api-token=secret".into(),
            ])
            .is_err()
        );
        assert!(validate_command(&["plugin".into(), "pack".into()]).is_err());
        assert!(validate_command(&["plugin".into(), "upload".into(), "--watch".into(),]).is_err());
    }

    #[test]
    fn command_validation_matches_allowlist_and_secret_option_model() -> noprop::TestResult {
        const PAIRS: [(&str, &str, bool); 10] = [
            ("record", "export", true),
            ("record", "import", true),
            ("record", "delete", true),
            ("customize", "export", true),
            ("customize", "apply", true),
            ("plugin", "upload", true),
            ("plugin", "pack", false),
            ("record", "watch", false),
            ("customize", "delete", false),
            ("unknown", "export", false),
        ];

        test_support::run(0x4b49_4e54_434d_4401, test_support::DEFAULT_CASES, |ctx| {
            let (group, command, pair_allowed) =
                PAIRS[noprop::sample_usize_in(ctx, 0..PAIRS.len())];
            let option_mode = noprop::sample_usize_in(ctx, 0..4);
            let mut arguments = vec![group.to_owned(), command.to_owned()];
            let option_allowed = match option_mode {
                0 => true,
                1 => {
                    let option =
                        FORBIDDEN_OPTIONS[noprop::sample_usize_in(ctx, 0..FORBIDDEN_OPTIONS.len())];
                    arguments.push(format!("{option}=pbt-secret-{}", noprop::sample_u64(ctx)));
                    false
                }
                2 => {
                    arguments.push("--watch".to_owned());
                    false
                }
                _ => {
                    arguments.push(format!("--app={}", noprop::sample_u16(ctx)));
                    true
                }
            };
            assert_eq!(
                validate_command(&arguments).is_ok(),
                pair_allowed && option_allowed,
                "cli-kintone command validation mismatch for {arguments:?}"
            );
            Ok(())
        })
    }

    #[test]
    fn authentication_matrix_matches_command_policy() -> noprop::TestResult {
        test_support::run(0x4b49_4e54_4155_5448, test_support::DEFAULT_CASES, |ctx| {
            let base_mode = noprop::sample_usize_in(ctx, 0..4);
            let username = noprop::sample_bool(ctx);
            let password = noprop::sample_bool(ctx);
            let token = noprop::sample_bool(ctx);
            let basic_username = noprop::sample_bool(ctx);
            let basic_password = noprop::sample_bool(ctx);
            let mut environment = Vec::new();
            match base_mode {
                0 => environment.push(("KINTONE_BASE_URL", "https://pbt.example.invalid")),
                1 => environment.push(("KINTONE_BASE_URL", "http://pbt.example.invalid")),
                2 => environment.push(("KINTONE_BASE_URL", "ftp://pbt.example.invalid")),
                _ => {}
            }
            if username {
                environment.push(("KINTONE_USERNAME", "pbt-user"));
            }
            if password {
                environment.push(("KINTONE_PASSWORD", "pbt-password"));
            }
            if token {
                environment.push(("KINTONE_API_TOKEN", "pbt-token"));
            }
            if basic_username {
                environment.push(("KINTONE_BASIC_AUTH_USERNAME", "pbt-basic-user"));
            }
            if basic_password {
                environment.push(("KINTONE_BASIC_AUTH_PASSWORD", "pbt-basic-password"));
            }
            let bridge = bridge(&environment);
            let kind_index = noprop::sample_usize_in(ctx, 0..6);
            let kind = match kind_index {
                0 => CommandKind::RecordExport,
                1 => CommandKind::RecordImport,
                2 => CommandKind::RecordDelete,
                3 => CommandKind::CustomizeExport,
                4 => CommandKind::CustomizeApply,
                _ => CommandKind::PluginUpload,
            };

            let common = matches!(base_mode, 0 | 1)
                && username == password
                && basic_username == basic_password;
            let password_auth = username && password;
            let expected = common
                && match kind_index {
                    0 | 1 => password_auth || token,
                    2 => token,
                    _ => password_auth,
                };
            let actual = bridge.validated_environment(kind);
            assert_eq!(
                actual.is_ok(),
                expected,
                "authentication policy mismatch: kind={kind_index}, base={base_mode}, username={username}, password={password}, token={token}, basic_username={basic_username}, basic_password={basic_password}"
            );

            if let Ok(environment) = actual {
                match kind_index {
                    2 => {
                        assert!(environment.contains_key("KINTONE_API_TOKEN"));
                        assert!(!environment.contains_key("KINTONE_USERNAME"));
                        assert!(!environment.contains_key("KINTONE_PASSWORD"));
                    }
                    3..=5 => {
                        assert!(environment.contains_key("KINTONE_USERNAME"));
                        assert!(environment.contains_key("KINTONE_PASSWORD"));
                        assert!(!environment.contains_key("KINTONE_API_TOKEN"));
                    }
                    _ if password_auth => {
                        assert!(!environment.contains_key("KINTONE_API_TOKEN"));
                    }
                    _ => {
                        assert!(environment.contains_key("KINTONE_API_TOKEN"));
                    }
                }
            }
            Ok(())
        })
    }

    #[test]
    fn status_never_exposes_generated_kintone_values() -> noprop::TestResult {
        test_support::run(0x4b49_4e54_5345_4352, 512, |ctx| {
            let nonce = noprop::sample_u64(ctx);
            let host = format!("pbt-secret-host-{nonce}.example.invalid");
            let token = format!("pbt-secret-token-{nonce}");
            let username = format!("pbt-secret-user-{nonce}");
            let password = format!("pbt-secret-password-{nonce}");
            let guest = format!("pbt-secret-guest-{nonce}");
            let environment = [
                ("KINTONE_BASE_URL", format!("https://{host}")),
                ("KINTONE_USERNAME", username.clone()),
                ("KINTONE_PASSWORD", password.clone()),
                ("KINTONE_API_TOKEN", token.clone()),
                ("KINTONE_GUEST_SPACE_ID", guest.clone()),
            ];
            let bridge = Bridge {
                executable_override: None,
                environment: environment
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            };
            let rendered = bridge.status().to_string();
            for secret in [&host, &token, &username, &password, &guest] {
                assert!(
                    !rendered.contains(secret),
                    "status leaked configured value {secret:?}: {rendered}"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn normal_sessions_reject_cli_paths_outside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let input = outside.path().join("records.csv");
        std::fs::write(&input, "record_number\n1\n").unwrap();
        let session = session(root.path());
        let result = validate_and_rewrite_paths(
            &session,
            &session.cwd,
            vec![
                "record".into(),
                "import".into(),
                "--file-path".into(),
                input.display().to_string(),
            ],
        );
        assert!(result.is_err());
    }

    #[test]
    fn customize_manifest_rejects_local_reference_outside_permitted_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.js");
        std::fs::write(&outside_file, "secret").unwrap();
        let manifest = root.path().join("customize-manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "scope": "ALL",
                "desktop": {"js": [outside_file.display().to_string()], "css": []},
                "mobile": {"js": [], "css": []}
            }))
            .unwrap(),
        )
        .unwrap();
        let session = session(root.path());
        assert!(validate_customize_manifest(&session, &manifest).is_err());
    }

    #[test]
    fn customize_manifest_accepts_permitted_local_files_and_urls() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("desktop/js")).unwrap();
        std::fs::write(root.path().join("desktop/js/app.js"), "console.log('ok')").unwrap();
        let manifest = root.path().join("customize-manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "scope": "ALL",
                "desktop": {
                    "js": ["https://example.invalid/library.js", "desktop/js/app.js"],
                    "css": []
                },
                "mobile": {"js": [], "css": []}
            }))
            .unwrap(),
        )
        .unwrap();
        let session = session(root.path());
        validate_customize_manifest(&session, &manifest).unwrap();
    }

    #[test]
    fn generated_bounded_regular_file_matches_size_limit() -> noprop::TestResult {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("bounded.bin");
        test_support::run(0x424f_554e_4445_4446, 512, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=128);
            let max = noprop::sample_usize_in(ctx, 0..=96);
            let bytes = (0..len).map(|_| noprop::sample_u8(ctx)).collect::<Vec<_>>();
            std::fs::write(&path, &bytes).unwrap();
            let result = read_bounded_regular_file(&path, max);
            assert_eq!(
                result.is_ok(),
                len <= max,
                "bounded file mismatch: len={len} max={max}"
            );
            if let Ok(actual) = result {
                assert_eq!(actual, bytes);
            }
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn bounded_regular_file_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.bin");
        let link = root.path().join("link.bin");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_bounded_regular_file(&link, 1024).is_err());
    }

    #[test]
    fn customize_manifest_rejects_oversized_file() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("customize-manifest.json");
        let file = std::fs::File::create(&manifest).unwrap();
        file.set_len(MAX_CUSTOMIZE_MANIFEST_BYTES as u64 + 1)
            .unwrap();
        let session = session(root.path());
        let error = validate_customize_manifest(&session, &manifest)
            .err()
            .unwrap();
        assert!(error.to_string().contains("failed to read"));
        assert!(format!("{error:#}").contains("file exceeds"));
    }

    struct ChunkedReader<'a> {
        bytes: &'a [u8],
        offset: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.offset >= self.bytes.len() {
                return Ok(0);
            }
            let len = self
                .chunk
                .min(buffer.len())
                .min(self.bytes.len() - self.offset);
            buffer[..len].copy_from_slice(&self.bytes[self.offset..self.offset + len]);
            self.offset += len;
            Ok(len)
        }
    }

    #[test]
    fn generated_attachment_traversal_scanner_matches_naive_reference() -> noprop::TestResult {
        test_support::run(0x4154_5441_4348_4353, test_support::DEFAULT_CASES, |ctx| {
            let len = noprop::sample_usize_in(ctx, 0..=256);
            let mut bytes = (0..len).map(|_| noprop::sample_u8(ctx)).collect::<Vec<_>>();
            if bytes.len() >= 3 && noprop::sample_bool(ctx) {
                let index = noprop::sample_usize_in(ctx, 0..=bytes.len() - 3);
                bytes[index..index + 3].copy_from_slice(if noprop::sample_bool(ctx) {
                    b"../"
                } else {
                    b"..\\"
                });
            }
            let expected = bytes
                .windows(3)
                .any(|window| window == b"../" || window == b"..\\");
            let chunk = noprop::sample_usize_in(ctx, 1..=17);
            let actual = reader_contains_parent_traversal(ChunkedReader {
                bytes: &bytes,
                offset: 0,
                chunk,
            })
            .unwrap();
            assert_eq!(
                actual, expected,
                "traversal scanner mismatch: bytes={bytes:?} chunk={chunk}"
            );
            Ok(())
        })
    }

    #[test]
    fn attachment_preflight_rejects_parent_traversal_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let csv = root.path().join("records.csv");
        std::fs::write(&csv, "file\n../outside/secret.txt\n").unwrap();
        assert!(reject_attachment_parent_traversal(&csv).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let attachments = root.path().join("attachments");
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir(&attachments).unwrap();
            symlink(outside.path(), attachments.join("escape")).unwrap();
            assert!(reject_symlinks_recursively(&attachments).is_err());

            let safe_csv = root.path().join("safe.csv");
            let csv_link = root.path().join("records-link.csv");
            std::fs::write(&safe_csv, "file\nattachments/a.txt\n").unwrap();
            symlink(&safe_csv, &csv_link).unwrap();
            assert!(reject_attachment_parent_traversal(&csv_link).is_err());
        }
    }

    #[test]
    fn rewrites_permitted_relative_input_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("records.csv"), "record_number\n1\n").unwrap();
        let session = session(root.path());
        let arguments = validate_and_rewrite_paths(
            &session,
            &session.cwd,
            vec![
                "record".into(),
                "import".into(),
                "--file-path=records.csv".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            arguments[2],
            format!("--file-path={}", session.cwd.join("records.csv").display())
        );
    }
}
