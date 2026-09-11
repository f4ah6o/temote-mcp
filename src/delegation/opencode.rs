use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde_json::{Map, Value, json};

use super::*;

pub(super) fn default_binary() -> PathBuf {
    PathBuf::from(OPENCODE_BINARY_NAME)
}

const MAX_OPENCODE_PROMPT_BYTES: usize = 64 * 1024;
const OPENCODE_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OPENCODE_DIAGNOSTIC_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const OPENCODE_DIAGNOSTIC_MODELS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DIAGNOSTIC_VERSION_BYTES: usize = 128;
const MAX_DIAGNOSTIC_LISTING_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_ERROR_BYTES: usize = 4096;
const OPENCODE_BINARY_NAME: &str = "opencode";
pub(super) const OPENCODE_BIN_ENV: &str = "TEMOTE_OPENCODE_BIN";
const MAX_OPENCODE_BIN_PATH_BYTES: usize = 4096;
const MAX_JSON_OBJECT_CANDIDATES: usize = 16;
const MAX_JSON_SCAN_ATTEMPTS: usize = 64;
const MAX_REPORT_SCAN_BYTES: usize = 64 * 1024;
const SUMMARY_TRUNCATION_MARKER: &str = " …[truncated]";

const OPENCODE_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LOGNAME",
    "NO_PROXY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

const OPENCODE_REPORT_INSTRUCTIONS: &str = r#"You are a delegated implementation worker running non-interactively. You must not ask interactive questions, and you must finish the task below before answering.

When finished, respond with ONLY one JSON object and nothing else: no markdown, no code fences, no text before or after the JSON.
The JSON object must contain exactly these fields:
{"status":"completed|failed|blocked|needs_decision","summary":"short summary, at most 1200 characters","base_commit":"","changed_files":[],"checks":[],"unresolved":[],"requested_model":__REQUESTED_MODEL__,"requested_effort":__REQUESTED_EFFORT__,"observed_model":null,"observed_effort":null}
Rules:
- All string values are plain strings; changed_files, checks, and unresolved are arrays of strings (use [] when empty).
- Set "requested_model" to __REQUESTED_MODEL__ and "requested_effort" to __REQUESTED_EFFORT__.
- Set "observed_model"/"observed_effort" only when you can actually observe them; otherwise keep null.
- Do not include any other fields.

Task:
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenCodeExecutableStatus {
    Available,
    Unavailable,
}

impl OpenCodeExecutableStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenCodeExecutableSource {
    EnvOverride,
    PathLookup,
    InvalidOverride,
}

impl OpenCodeExecutableSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::EnvOverride => "env_override",
            Self::PathLookup => "path",
            Self::InvalidOverride => "invalid_override",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OpenCodeExecutableError {
    Empty,
    InvalidValue,
    TooLong,
    NotAbsolute,
    Missing,
    NotAFile,
    NotExecutable,
}

impl OpenCodeExecutableError {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::InvalidValue => "invalid_value",
            Self::TooLong => "too_long",
            Self::NotAbsolute => "not_absolute",
            Self::Missing => "not_found",
            Self::NotAFile => "not_a_file",
            Self::NotExecutable => "not_executable",
        }
    }

    fn message(self) -> String {
        match self {
            Self::Empty => {
                "TEMOTE_OPENCODE_BIN is set but empty; unset it to use PATH lookup".to_owned()
            }
            Self::InvalidValue => "TEMOTE_OPENCODE_BIN is not a valid path value".to_owned(),
            Self::TooLong => format!(
                "TEMOTE_OPENCODE_BIN exceeds the {MAX_OPENCODE_BIN_PATH_BYTES}-byte path limit"
            ),
            Self::NotAbsolute => "TEMOTE_OPENCODE_BIN must be an absolute path".to_owned(),
            Self::Missing => "TEMOTE_OPENCODE_BIN does not point to an existing file".to_owned(),
            Self::NotAFile => "TEMOTE_OPENCODE_BIN must point to a regular file".to_owned(),
            Self::NotExecutable => {
                "TEMOTE_OPENCODE_BIN does not point to an executable file".to_owned()
            }
        }
    }

    pub(super) fn delegation_message(self) -> String {
        format!("OpenCode backend unavailable: {}", self.message())
    }
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedOpenCodeExecutable {
    source: OpenCodeExecutableSource,
    binary: PathBuf,
}

impl ResolvedOpenCodeExecutable {
    fn source(&self) -> OpenCodeExecutableSource {
        self.source
    }

    fn binary(&self) -> &Path {
        &self.binary
    }

    pub(super) fn into_path(self) -> PathBuf {
        self.binary
    }
}

pub(super) fn bin_override_value() -> Result<Option<String>, OpenCodeExecutableError> {
    match std::env::var(OPENCODE_BIN_ENV) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(OpenCodeExecutableError::InvalidValue),
    }
}

pub(super) fn resolve_opencode_executable(
    override_value: Option<&str>,
    fallback: &Path,
) -> Result<ResolvedOpenCodeExecutable, OpenCodeExecutableError> {
    let Some(value) = override_value else {
        return Ok(ResolvedOpenCodeExecutable {
            source: OpenCodeExecutableSource::PathLookup,
            binary: fallback.to_path_buf(),
        });
    };

    if value.is_empty() {
        return Err(OpenCodeExecutableError::Empty);
    }
    if value.contains('\0') {
        return Err(OpenCodeExecutableError::InvalidValue);
    }
    if value.len() > MAX_OPENCODE_BIN_PATH_BYTES {
        return Err(OpenCodeExecutableError::TooLong);
    }
    if !Path::new(value).is_absolute() {
        return Err(OpenCodeExecutableError::NotAbsolute);
    }

    let canonical = fs::canonicalize(value).map_err(|_| OpenCodeExecutableError::Missing)?;
    let metadata = fs::metadata(&canonical).map_err(|_| OpenCodeExecutableError::Missing)?;
    if !metadata.is_file() {
        return Err(OpenCodeExecutableError::NotAFile);
    }
    if !is_executable_path_metadata(&metadata) {
        return Err(OpenCodeExecutableError::NotExecutable);
    }

    Ok(ResolvedOpenCodeExecutable {
        source: OpenCodeExecutableSource::EnvOverride,
        binary: canonical,
    })
}

#[cfg(unix)]
fn is_executable_path_metadata(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_path_metadata(_metadata: &fs::Metadata) -> bool {
    true
}

fn resolve_default_opencode_executable()
-> Result<ResolvedOpenCodeExecutable, OpenCodeExecutableError> {
    let override_value = bin_override_value()?;
    resolve_opencode_executable(override_value.as_deref(), &default_binary())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenCodeVersionStatus {
    Ready,
    Unavailable,
    Failed,
    Timeout,
}

impl OpenCodeVersionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenCodeModelsStatus {
    Ready,
    Unavailable,
    Unsupported,
    Failed,
    Timeout,
}

impl OpenCodeModelsStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenCodeRequestedModelStatus {
    Present,
    Absent,
    Unknown,
    NotChecked,
}

impl OpenCodeRequestedModelStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
            Self::NotChecked => "not_checked",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct OpenCodeDiagnosticTimeouts {
    version: Duration,
    models: Duration,
}

impl Default for OpenCodeDiagnosticTimeouts {
    fn default() -> Self {
        Self {
            version: OPENCODE_DIAGNOSTIC_VERSION_TIMEOUT,
            models: OPENCODE_DIAGNOSTIC_MODELS_TIMEOUT,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OpenCodeDiagnostics {
    executable: OpenCodeExecutableStatus,
    executable_source: OpenCodeExecutableSource,
    executable_reason: Option<&'static str>,
    version: OpenCodeVersionStatus,
    version_value: Option<String>,
    models: OpenCodeModelsStatus,
    model_count: Option<usize>,
    models_truncated: bool,
    requested_model: Option<String>,
    requested_model_status: OpenCodeRequestedModelStatus,
}

#[derive(Debug)]
struct OpenCodeProbe {
    executable: OpenCodeExecutableStatus,
    timed_out: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

#[derive(Debug, Default)]
struct OpenCodeModelListing {
    count: usize,
    requested_present: bool,
    saw_non_empty_line: bool,
}

pub(super) fn diagnose_default(
    requested_model: Option<&str>,
    timeouts: OpenCodeDiagnosticTimeouts,
) -> Result<OpenCodeDiagnostics, String> {
    match resolve_default_opencode_executable() {
        Ok(executable) => opencode_diagnostics(&executable, requested_model, timeouts),
        Err(error) => Ok(invalid_override_diagnostics(error, requested_model)),
    }
}

fn invalid_override_diagnostics(
    error: OpenCodeExecutableError,
    requested_model: Option<&str>,
) -> OpenCodeDiagnostics {
    let requested_model = requested_model.map(str::to_owned);
    let requested_model_status = if requested_model.is_some() {
        OpenCodeRequestedModelStatus::Unknown
    } else {
        OpenCodeRequestedModelStatus::NotChecked
    };
    OpenCodeDiagnostics {
        executable: OpenCodeExecutableStatus::Unavailable,
        executable_source: OpenCodeExecutableSource::InvalidOverride,
        executable_reason: Some(error.reason()),
        version: OpenCodeVersionStatus::Unavailable,
        version_value: None,
        models: OpenCodeModelsStatus::Unavailable,
        model_count: None,
        models_truncated: false,
        requested_model,
        requested_model_status,
    }
}

pub(super) fn opencode_diagnostics(
    executable: &ResolvedOpenCodeExecutable,
    requested_model: Option<&str>,
    timeouts: OpenCodeDiagnosticTimeouts,
) -> Result<OpenCodeDiagnostics, String> {
    let binary = executable.binary();
    let version_probe = run_opencode_probe(
        binary,
        &["--version"],
        timeouts.version,
        MAX_DIAGNOSTIC_VERSION_BYTES,
    )?;
    if version_probe.executable == OpenCodeExecutableStatus::Unavailable {
        let requested_model = requested_model.map(str::to_owned);
        let requested_model_status = if requested_model.is_some() {
            OpenCodeRequestedModelStatus::Unknown
        } else {
            OpenCodeRequestedModelStatus::NotChecked
        };
        return Ok(OpenCodeDiagnostics {
            executable: OpenCodeExecutableStatus::Unavailable,
            executable_source: executable.source(),
            executable_reason: None,
            version: OpenCodeVersionStatus::Unavailable,
            version_value: None,
            models: OpenCodeModelsStatus::Unavailable,
            model_count: None,
            models_truncated: false,
            requested_model,
            requested_model_status,
        });
    }

    let (version, version_value) = classify_version_probe(&version_probe);
    let models_probe = run_opencode_probe(
        binary,
        &["models", "--pure"],
        timeouts.models,
        MAX_DIAGNOSTIC_LISTING_BYTES,
    )?;
    let listing = summarize_opencode_models(&models_probe.stdout, requested_model);
    let models = classify_models_probe(&models_probe, &listing);
    let models_truncated = models_probe.truncated;
    let model_count = (models == OpenCodeModelsStatus::Ready).then_some(listing.count);
    let requested_model = requested_model.map(str::to_owned);
    let requested_model_status = match (&requested_model, models, models_truncated) {
        (None, _, _) => OpenCodeRequestedModelStatus::NotChecked,
        (Some(_), OpenCodeModelsStatus::Ready, false) => {
            if listing.requested_present {
                OpenCodeRequestedModelStatus::Present
            } else {
                OpenCodeRequestedModelStatus::Absent
            }
        }
        (Some(_), _, _) => OpenCodeRequestedModelStatus::Unknown,
    };

    Ok(OpenCodeDiagnostics {
        executable: OpenCodeExecutableStatus::Available,
        executable_source: executable.source(),
        executable_reason: None,
        version,
        version_value,
        models,
        model_count,
        models_truncated,
        requested_model,
        requested_model_status,
    })
}

fn classify_version_probe(probe: &OpenCodeProbe) -> (OpenCodeVersionStatus, Option<String>) {
    if probe.timed_out {
        return (OpenCodeVersionStatus::Timeout, None);
    }
    if probe.exit_code != Some(0) {
        return (OpenCodeVersionStatus::Failed, None);
    }
    match parse_opencode_version(&probe.stdout) {
        Some(version) => (OpenCodeVersionStatus::Ready, Some(version)),
        None => (OpenCodeVersionStatus::Failed, None),
    }
}

fn parse_opencode_version(bytes: &[u8]) -> Option<String> {
    let line = first_bounded_line(bytes, MAX_DIAGNOSTIC_VERSION_BYTES)?;
    if line.is_empty()
        || !line.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
        })
        || !line.chars().any(|character| character.is_ascii_digit())
    {
        return None;
    }
    Some(line)
}

fn first_bounded_line(bytes: &[u8], max_bytes: usize) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.len() > max_bytes {
        return None;
    }
    Some(line.to_owned())
}

fn classify_models_probe(
    probe: &OpenCodeProbe,
    listing: &OpenCodeModelListing,
) -> OpenCodeModelsStatus {
    if probe.executable == OpenCodeExecutableStatus::Unavailable {
        return OpenCodeModelsStatus::Unavailable;
    }
    if probe.timed_out {
        return OpenCodeModelsStatus::Timeout;
    }
    if probe.exit_code != Some(0) {
        return if stderr_indicates_unknown_command(&probe.stderr) {
            OpenCodeModelsStatus::Unsupported
        } else {
            OpenCodeModelsStatus::Failed
        };
    }
    if listing.count > 0 {
        return OpenCodeModelsStatus::Ready;
    }
    if listing.saw_non_empty_line {
        OpenCodeModelsStatus::Failed
    } else {
        OpenCodeModelsStatus::Unavailable
    }
}

fn summarize_opencode_models(bytes: &[u8], requested_model: Option<&str>) -> OpenCodeModelListing {
    let text = String::from_utf8_lossy(bytes);
    let mut listing = OpenCodeModelListing::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        listing.saw_non_empty_line = true;
        if !is_opencode_model_identifier(line) {
            continue;
        }
        listing.count += 1;
        if requested_model == Some(line) {
            listing.requested_present = true;
        }
    }
    listing
}

fn is_opencode_model_identifier(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > MAX_EVIDENCE_STRING_BYTES {
        return false;
    }
    candidate.contains('/')
        && candidate.split('/').all(|part| {
            !part.is_empty()
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '.' | '-' | '_' | '+' | ':' | '@')
                })
        })
}

fn stderr_indicates_unknown_command(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    [
        "unknown command",
        "unknown subcommand",
        "unrecognized command",
        "not a valid command",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

fn run_opencode_probe(
    binary: &Path,
    args: &[&str],
    timeout: Duration,
    stdout_limit: usize,
) -> Result<OpenCodeProbe, String> {
    let artifacts = create_artifacts()?;
    let paths = artifacts.paths.clone();
    let mut command = build_opencode_probe_command(binary, args, std::env::vars_os());
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_dir_all(&paths.directory);
            return Ok(OpenCodeProbe {
                executable: OpenCodeExecutableStatus::Unavailable,
                timed_out: false,
                exit_code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            });
        }
    };

    let captured = wait_with_bounded_artifacts_timeout(
        &mut child,
        artifacts.events_file,
        artifacts.stderr_file,
        timeout,
    );
    let (outcome, stdout_capture_truncated, stderr_capture_truncated) = match captured {
        Ok(captured) => captured,
        Err(error) => {
            let _ = fs::remove_dir_all(&paths.directory);
            return Err(error);
        }
    };
    let status = match child.try_wait() {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = fs::remove_dir_all(&paths.directory);
            return Err("OpenCode diagnostics child exited without a process status".to_owned());
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&paths.directory);
            return Err(format!(
                "could not inspect OpenCode diagnostics status: {error}"
            ));
        }
    };
    secure_artifact_files(&paths);
    let (stdout, stdout_read_truncated) =
        read_probe_artifact(&paths.events, stdout_limit).unwrap_or_default();
    let (stderr, stderr_read_truncated) =
        read_probe_artifact(&paths.stderr, MAX_DIAGNOSTIC_ERROR_BYTES).unwrap_or_default();
    let _ = fs::remove_dir_all(&paths.directory);

    Ok(OpenCodeProbe {
        executable: OpenCodeExecutableStatus::Available,
        timed_out: outcome == WaitOutcome::TimedOut,
        exit_code: status.code(),
        stdout,
        stderr,
        truncated: stdout_capture_truncated
            || stderr_capture_truncated
            || stdout_read_truncated
            || stderr_read_truncated,
    })
}

fn read_probe_artifact(path: &Path, max_bytes: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    BufReader::new(file)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok((bytes, truncated))
}

fn build_opencode_probe_command<I>(binary: &Path, args: &[&str], environment: I) -> Command
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut command = Command::new(binary);
    command.args(args);
    configure_opencode_environment(&mut command, environment);
    command
}

pub(super) fn diagnostics_to_json(diagnostics: &OpenCodeDiagnostics) -> Value {
    let mut version = Map::new();
    version.insert(
        "status".to_owned(),
        Value::from(diagnostics.version.as_str()),
    );
    if let Some(value) = &diagnostics.version_value {
        version.insert("value".to_owned(), Value::from(value.clone()));
    }

    let mut models = Map::new();
    models.insert(
        "status".to_owned(),
        Value::from(diagnostics.models.as_str()),
    );
    if let Some(count) = diagnostics.model_count {
        models.insert("count".to_owned(), Value::from(count));
    }
    models.insert(
        "truncated".to_owned(),
        Value::from(diagnostics.models_truncated),
    );

    let mut requested = Map::new();
    requested.insert(
        "status".to_owned(),
        Value::from(diagnostics.requested_model_status.as_str()),
    );
    if let Some(model) = &diagnostics.requested_model {
        requested.insert("value".to_owned(), Value::from(model.clone()));
    }

    let mut executable = Map::new();
    executable.insert(
        "status".to_owned(),
        Value::from(diagnostics.executable.as_str()),
    );
    executable.insert(
        "resolved".to_owned(),
        Value::from(diagnostics.executable == OpenCodeExecutableStatus::Available),
    );
    executable.insert(
        "source".to_owned(),
        Value::from(diagnostics.executable_source.as_str()),
    );
    if let Some(reason) = diagnostics.executable_reason {
        executable.insert("reason".to_owned(), Value::from(reason));
    }

    json!({
        "backend": DelegationBackend::OpenCode.name(),
        "executable": Value::Object(executable),
        "version": Value::Object(version),
        "models": Value::Object(models),
        "requested_model": Value::Object(requested),
    })
}

pub(super) fn validate_options(options: &Options) -> Result<(), String> {
    if let Some(variant) = &options.variant
        && (variant.is_empty() || variant.len() > MAX_ARGUMENT_BYTES || variant.contains('\0'))
    {
        return Err(format!(
            "OpenCode variant must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    opencode_effective_prompt(options)?;
    Ok(())
}

#[cfg(test)]
static CLEANED_ARTIFACT_DIRECTORIES: AtomicUsize = AtomicUsize::new(0);

struct ArtifactCleanup {
    directory: PathBuf,
    keep: bool,
}

impl ArtifactCleanup {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            keep: false,
        }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for ArtifactCleanup {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        #[cfg(test)]
        CLEANED_ARTIFACT_DIRECTORIES.fetch_add(1, Ordering::Relaxed);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

pub(super) fn run_opencode(options: Options) -> Result<DelegationResult, String> {
    let artifacts = create_artifacts()?;
    let paths = artifacts.paths.clone();
    let mut cleanup = ArtifactCleanup::new(paths.directory.clone());
    let effective_prompt = opencode_effective_prompt(&options)?;

    let working_directory =
        std::env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|error| {
                format!("could not resolve OpenCode delegation working directory: {error}")
            })?;
    let mut command = build_opencode_command(
        &options,
        &working_directory,
        &effective_prompt,
        std::env::vars_os(),
    );
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| opencode_launch_error(&error))?;

    let timeout = options.timeout.unwrap_or(OPENCODE_RUN_TIMEOUT);
    let (wait_outcome, stdout_truncated, stderr_truncated) = wait_with_bounded_artifacts_timeout(
        &mut child,
        artifacts.events_file,
        artifacts.stderr_file,
        timeout,
    )?;
    let status = child
        .try_wait()
        .map_err(|error| format!("could not inspect OpenCode delegation status: {error}"))?
        .ok_or_else(|| "OpenCode delegation exited without a process status".to_owned())?;
    secure_artifact_files(&paths);

    let (evidence, report_state) = collect_opencode_output(&paths.events)
        .unwrap_or((Evidence::default(), ReportState::Missing));
    let exit_code = status.code();
    let requested_variant = options.variant.clone().unwrap_or_default();

    if wait_outcome == WaitOutcome::TimedOut {
        cleanup.keep();
        return Ok(DelegationResult {
            backend: DelegationBackend::OpenCode,
            status: Status::ProcessTimeout,
            requested_model: options.model,
            requested_reasoning_effort: requested_variant,
            observed_model: evidence.observed_model.clone(),
            observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
            report: None,
            evidence,
            exit_code,
            artifacts_truncated: stdout_truncated || stderr_truncated,
            artifacts: paths,
        });
    }

    if !status.success() {
        cleanup.keep();
        return Ok(DelegationResult {
            backend: DelegationBackend::OpenCode,
            status: Status::ProcessNonzeroExit,
            requested_model: options.model,
            requested_reasoning_effort: requested_variant,
            observed_model: evidence.observed_model.clone(),
            observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
            report: None,
            evidence,
            exit_code,
            artifacts_truncated: stdout_truncated || stderr_truncated,
            artifacts: paths,
        });
    }

    let (report_status, report) = match report_state {
        ReportState::Valid(value) => match normalize_opencode_report(value, &options) {
            Some(report) if validate_report_schema(&report) => {
                if canonical_report_fits(&report) {
                    persist_opencode_report(&paths, &report)?;
                    (Status::Success, Some(report))
                } else {
                    (Status::OversizedReport, None)
                }
            }
            _ => (Status::InvalidReportSchema, None),
        },
        ReportState::Missing => (Status::MissingReport, None),
        ReportState::InvalidJson => (Status::InvalidJson, None),
        ReportState::InvalidSchema => (Status::InvalidReportSchema, None),
        ReportState::Oversized => (Status::OversizedReport, None),
    };
    cleanup.keep();

    Ok(DelegationResult {
        backend: DelegationBackend::OpenCode,
        status: report_status,
        requested_model: options.model,
        requested_reasoning_effort: requested_variant,
        observed_model: evidence.observed_model.clone(),
        observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
        report,
        evidence,
        exit_code,
        artifacts_truncated: stdout_truncated || stderr_truncated,
        artifacts: paths,
    })
}

fn opencode_effective_prompt(options: &Options) -> Result<String, String> {
    let requested_model = serde_json::to_string(&options.model)
        .map_err(|error| format!("could not encode OpenCode requested model: {error}"))?;
    let requested_effort = serde_json::to_string(options.variant.as_deref().unwrap_or(""))
        .map_err(|error| format!("could not encode OpenCode requested variant: {error}"))?;
    let instructions = OPENCODE_REPORT_INSTRUCTIONS
        .replace("__REQUESTED_MODEL__", &requested_model)
        .replace("__REQUESTED_EFFORT__", &requested_effort);
    let prompt = format!("{instructions}{}", options.prompt);
    if prompt.len() > MAX_OPENCODE_PROMPT_BYTES {
        return Err(format!(
            "OpenCode delegation prompt exceeds {MAX_OPENCODE_PROMPT_BYTES} bytes"
        ));
    }
    Ok(prompt)
}

fn opencode_launch_error(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "OpenCode backend unavailable: the configured OpenCode executable could not be found; install OpenCode, add it to PATH, or fix TEMOTE_OPENCODE_BIN".to_owned()
    } else {
        format!("could not start OpenCode delegation: {error}")
    }
}

fn build_opencode_command<I>(
    options: &Options,
    working_directory: &Path,
    effective_prompt: &str,
    environment: I,
) -> Command
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut command = Command::new(&options.opencode_binary);
    command
        .arg("run")
        .arg("--pure")
        .arg("--format")
        .arg("json")
        .arg("--dir")
        .arg(working_directory)
        .arg("--model")
        .arg(&options.model);
    if let Some(variant) = &options.variant {
        command.arg("--variant").arg(variant);
    }
    command.arg("--").arg(effective_prompt);
    configure_opencode_environment(&mut command, environment);
    command
}

fn configure_opencode_environment<I>(command: &mut Command, environment: I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    command.env_clear();
    for (key, value) in filtered_opencode_environment(environment) {
        command.env(key, value);
    }
}

fn filtered_opencode_environment<I>(environment: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    super::filtered_child_environment(environment, OPENCODE_CHILD_ENV_ALLOWLIST)
}

fn collect_opencode_output(path: &Path) -> std::io::Result<(Evidence, ReportState)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let mut reader = BufReader::new(file.take(MAX_EVIDENCE_BYTES.saturating_add(1)));
    let mut line = Vec::new();
    let mut evidence = Evidence::default();
    let mut text_parts: Vec<(Option<String>, String)> = Vec::new();

    while let Some(within_limit) = read_bounded_line(&mut reader, &mut line)? {
        if !within_limit {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };

        if evidence.thread_id.is_none() {
            evidence.thread_id = bounded_string(object.get("sessionID"));
        }
        if evidence.observed_model.is_none() {
            evidence.observed_model = opencode_observed_model(object);
        }

        match object.get("type").and_then(Value::as_str) {
            Some("step_finish") => {
                if let Some(tokens) = object
                    .get("part")
                    .and_then(|part| part.get("tokens"))
                    .and_then(Value::as_object)
                {
                    accumulate_opencode_usage(&mut evidence.usage, opencode_usage(tokens));
                }
            }
            Some("text") => {
                if let Some(part) = object.get("part").and_then(Value::as_object)
                    && let Some(text) = part.get("text").and_then(Value::as_str)
                    && text.len() <= MAX_EVENT_LINE_BYTES
                {
                    text_parts.push((bounded_string(part.get("messageID")), text.to_owned()));
                }
            }
            _ => {}
        }
    }

    let report = opencode_report_state(&text_parts);
    Ok((evidence, report))
}

fn opencode_report_state(text_parts: &[(Option<String>, String)]) -> ReportState {
    let Some(text) = opencode_report_text(text_parts) else {
        return ReportState::Missing;
    };
    parse_opencode_report(&text)
}

fn opencode_report_text(text_parts: &[(Option<String>, String)]) -> Option<String> {
    let (last_message_id, _) = text_parts.last()?;
    let mut text = String::new();
    if last_message_id.is_some() {
        for (message_id, part) in text_parts {
            if message_id == last_message_id {
                text.push_str(part);
            }
        }
    } else {
        for (_, part) in text_parts {
            text.push_str(part);
        }
    }
    Some(text)
}

fn parse_opencode_report(text: &str) -> ReportState {
    let over_budget = text.len() > DEFAULT_MAX_REPORT_BYTES;
    if text.len() > MAX_REPORT_SCAN_BYTES {
        return ReportState::Oversized;
    }
    if let Some(value) = parse_json_object(text.trim()) {
        return ReportState::Valid(value);
    }
    for candidate in json_object_candidates(text).into_iter().rev() {
        if let Some(value) = parse_json_object(candidate) {
            return ReportState::Valid(value);
        }
        if let Some(value) = parse_json_object(&sanitize_json_strings(candidate)) {
            return ReportState::Valid(value);
        }
    }
    if over_budget {
        ReportState::Oversized
    } else {
        ReportState::InvalidJson
    }
}

fn parse_json_object(text: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    value.is_object().then_some(value)
}

fn json_object_candidates(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut candidates = Vec::new();
    let mut index = 0;
    let mut attempts = 0;
    while index < bytes.len()
        && candidates.len() < MAX_JSON_OBJECT_CANDIDATES
        && attempts < MAX_JSON_SCAN_ATTEMPTS
    {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        attempts += 1;
        let start = index;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        while index < bytes.len() {
            let byte = bytes[index];
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
            } else {
                match byte {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(index);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            index += 1;
        }
        let Some(end) = end else {
            index = start + 1;
            continue;
        };
        candidates.push(&text[start..=end]);
        index = end + 1;
    }
    candidates
}

fn sanitize_json_strings(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in text.chars() {
        if !in_string {
            if character == '"' {
                in_string = true;
            }
            sanitized.push(character);
            continue;
        }
        if escaped {
            sanitized.push(character);
            escaped = false;
        } else if character == '\\' {
            sanitized.push(character);
            escaped = true;
        } else if character == '"' {
            sanitized.push(character);
            in_string = false;
        } else if character == '\n' {
            sanitized.push_str("\\n");
        } else if character == '\r' {
            sanitized.push_str("\\r");
        } else if character == '\t' {
            sanitized.push_str("\\t");
        } else if (character as u32) < 0x20 {
            sanitized.push_str(&format!("\\u{:04x}", character as u32));
        } else {
            sanitized.push(character);
        }
    }
    sanitized
}

fn normalize_opencode_report(value: Value, options: &Options) -> Option<Value> {
    let object = value.as_object()?;
    if object.len() != REPORT_FIELDS.len()
        || REPORT_FIELDS
            .iter()
            .any(|field| !object.contains_key(*field))
    {
        return None;
    }
    let status = object.get("status").and_then(Value::as_str)?;
    if !matches!(
        status,
        "completed" | "failed" | "blocked" | "needs_decision"
    ) {
        return None;
    }

    Some(json!({
        "status": status,
        "summary": bounded_summary(object.get("summary")?)?,
        "base_commit": bounded_report_text(object.get("base_commit")?, MAX_REPORT_COMMIT_CHARS)?,
        "changed_files": bounded_report_items(object.get("changed_files")?)?,
        "checks": bounded_report_items(object.get("checks")?)?,
        "unresolved": bounded_report_items(object.get("unresolved")?)?,
        "requested_model": options.model,
        "requested_effort": options.variant.clone().unwrap_or_default(),
        "observed_model": bounded_nullable_report_text(object.get("observed_model")?)?,
        "observed_effort": bounded_nullable_report_text(object.get("observed_effort")?)?,
    }))
}

fn bounded_summary(value: &Value) -> Option<String> {
    let summary = value.as_str()?;
    if summary.chars().count() <= MAX_REPORT_SUMMARY_CHARS {
        return Some(summary.to_owned());
    }
    let marker_chars = SUMMARY_TRUNCATION_MARKER.chars().count();
    let limit = MAX_REPORT_SUMMARY_CHARS.saturating_sub(marker_chars);
    let mut bounded: String = summary.chars().take(limit).collect();
    bounded.push_str(SUMMARY_TRUNCATION_MARKER);
    Some(bounded)
}

fn bounded_report_text(value: &Value, max_chars: usize) -> Option<String> {
    let text = value.as_str()?;
    (text.chars().count() <= max_chars).then(|| text.to_owned())
}

fn bounded_nullable_report_text(value: &Value) -> Option<Value> {
    if value.is_null() {
        return Some(Value::Null);
    }
    bounded_report_text(value, MAX_REPORT_ARGUMENT_CHARS).map(Value::String)
}

fn bounded_report_items(value: &Value) -> Option<Vec<Value>> {
    let items = value.as_array()?;
    if items.len() > MAX_REPORT_ARRAY_ITEMS {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let text = item.as_str()?;
            (text.chars().count() <= MAX_REPORT_ARRAY_ITEM_CHARS)
                .then(|| Value::String(text.to_owned()))
        })
        .collect::<Option<Vec<_>>>()
}

fn canonical_report_fits(report: &Value) -> bool {
    serde_json::to_vec(report).is_ok_and(|encoded| encoded.len() <= DEFAULT_MAX_REPORT_BYTES)
}

fn persist_opencode_report(paths: &ArtifactPaths, report: &Value) -> Result<(), String> {
    let encoded = serde_json::to_vec(report)
        .map_err(|error| format!("could not encode OpenCode delegation report: {error}"))?;
    let mut file = create_private_file(&paths.report)?;
    file.write_all(&encoded)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("could not write OpenCode delegation report: {error}"))
}

fn accumulate_opencode_usage(target: &mut Option<Map<String, Value>>, step: Map<String, Value>) {
    let usage = target.get_or_insert_with(Map::new);
    for (field, value) in step {
        let Some(current) = usage.get_mut(&field) else {
            usage.insert(field, value);
            continue;
        };
        match (current.as_u64(), value.as_u64()) {
            (Some(existing), Some(addition)) => {
                *current = Value::from(existing.saturating_add(addition));
            }
            _ => {
                *current = value;
            }
        }
    }
}

fn opencode_observed_model(object: &Map<String, Value>) -> Option<String> {
    if let Some(model) =
        bounded_string(object.get("modelID")).or_else(|| bounded_string(object.get("model")))
    {
        return Some(model);
    }
    let part = object.get("part").and_then(Value::as_object)?;
    let model =
        bounded_string(part.get("modelID")).or_else(|| bounded_string(part.get("model")))?;
    let composed = match bounded_string(part.get("providerID")) {
        Some(provider) => format!("{provider}/{model}"),
        None => model,
    };
    (composed.len() <= MAX_EVIDENCE_STRING_BYTES).then_some(composed)
}

fn opencode_usage(tokens: &Map<String, Value>) -> Map<String, Value> {
    let mut usage = Map::new();
    for (source, target) in [
        ("input", "input_tokens"),
        ("output", "output_tokens"),
        ("reasoning", "reasoning_output_tokens"),
        ("total", "total_tokens"),
    ] {
        if let Some(value) = tokens.get(source).and_then(Value::as_u64) {
            usage.insert(target.to_owned(), Value::from(value));
        }
    }
    if let Some(cached) = tokens
        .get("cache")
        .and_then(Value::as_object)
        .and_then(|cache| cache.get("read"))
        .and_then(Value::as_u64)
    {
        usage.insert("cached_input_tokens".to_owned(), Value::from(cached));
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_opencode(root: &Path, mode: &str) -> PathBuf {
        let path = root.join(format!("fake-opencode-{mode}"));
        let script = r##"#!/bin/sh
set -eu
printf 'cwd=%s\n' "$(pwd -P)" >&2
printf 'args=' >&2
for arg in "$@"; do
    printf '<%s>' "$arg" >&2
done
printf '\n' >&2

report='{"status":"completed","summary":"ok","base_commit":"abc","changed_files":[],"checks":["cargo test"],"unresolved":[],"requested_model":"test-model","requested_effort":"high","observed_model":null,"observed_effort":null}'

case "$0" in
    *observed)
        printf '%s\n' '{"type":"step_start","sessionID":"ses_test"}'
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","providerID":"opencode-go","modelID":"deepseek-v4-flash","text":"{\"status\":\"completed\",\"summary\":\"ok\",\"base_commit\":\"abc\",\"changed_files\":[],\"checks\":[\"cargo test\"],\"unresolved\":[],\"requested_model\":\"test-model\",\"requested_effort\":\"high\",\"observed_model\":null,\"observed_effort\":null}"}}'
        printf '%s\n' '{"type":"step_finish","sessionID":"ses_test","part":{"type":"step-finish","tokens":{"total":30,"input":20,"output":5,"reasoning":2,"cache":{"read":3}}}}'
        exit 0
        ;;
    *success)
        printf '%s\n' '{"type":"step_start","sessionID":"ses_test"}'
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"{\"status\":\"completed\",\"summary\":\"ok\",\"base_commit\":\"abc\",\"changed_files\":[],\"checks\":[\"cargo test\"],\"unresolved\":[],\"requested_model\":\"test-model\",\"requested_effort\":\"high\",\"observed_model\":null,\"observed_effort\":null}"}}'
        printf '%s\n' '{"type":"step_finish","sessionID":"ses_test","part":{"type":"step-finish","tokens":{"total":30,"input":20,"output":5,"reasoning":2,"cache":{"read":3}}}}'
        exit 0
        ;;
    *nonzero)
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"not-json"}}'
        exit 7
        ;;
    *missing)
        printf '%s\n' '{"type":"step_start","sessionID":"ses_test"}'
        exit 0
        ;;
    *invalid_json)
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"not-json"}}'
        exit 0
        ;;
    *invalid_schema)
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"{\"status\":\"completed\"}"}}'
        exit 0
        ;;
    *oversized)
        printf '%s' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"'
        head -c 4200 /dev/zero | tr '\0' 'x'
        printf '%s\n' '"}}'
        exit 0
        ;;
    *huge_stdout)
        printf '%s' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"'
        yes x | head -c 9000000
        printf '%s\n' '"}}'
        exit 0
        ;;
    *long_summary)
        long=$(head -c 1500 /dev/zero | tr '\0' 'y')
        printf '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"{\\"status\\":\\"completed\\",\\"summary\\":\\"%s\\",\\"base_commit\\":\\"\\",\\"changed_files\\":[],\\"checks\\":[],\\"unresolved\\":[],\\"requested_model\\":\\"raw\\",\\"requested_effort\\":\\"\\",\\"observed_model\\":null,\\"observed_effort\\":null}"}}\n' "$long"
        exit 0
        ;;
    *multi_step)
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"{\"status\":\"completed\",\"summary\":\"ok\",\"base_commit\":\"\",\"changed_files\":[],\"checks\":[],\"unresolved\":[],\"requested_model\":\"raw\",\"requested_effort\":\"\",\"observed_model\":null,\"observed_effort\":null}"}}'
        printf '%s\n' '{"type":"step_finish","sessionID":"ses_test","part":{"type":"step-finish","tokens":{"total":12,"input":10,"output":2,"reasoning":0,"cache":{"read":0}}}}'
        printf '%s\n' '{"type":"step_finish","sessionID":"ses_test","part":{"type":"step-finish","tokens":{"total":8,"input":5,"output":3,"reasoning":0,"cache":{"read":4}}}}'
        exit 0
        ;;
    *extra_field)
        printf '%s\n' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"{\"status\":\"completed\",\"summary\":\"ok\",\"base_commit\":\"\",\"changed_files\":[],\"checks\":[],\"unresolved\":[],\"requested_model\":\"raw\",\"requested_effort\":\"\",\"observed_model\":null,\"observed_effort\":null,\"extra\":1}"}}'
        exit 0
        ;;
    *oversized_array)
        items=''
        i=0
        while [ "$i" -lt 129 ]; do
            items="${items}\"item\","
            i=$((i + 1))
        done
        report="{\"status\":\"completed\",\"summary\":\"ok\",\"base_commit\":\"\",\"changed_files\":[],\"checks\":[],\"unresolved\":[${items}\"last\"],\"requested_model\":\"raw\",\"requested_effort\":\"\",\"observed_model\":null,\"observed_effort\":null}"
        printf '%s' '{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_1","type":"text","text":"'
        printf '%s' "$report" | sed 's/\\/\\\\/g; s/"/\\"/g'
        printf '%s\n' '"}}'
        exit 0
        ;;
    *timeout)
        exec sleep 30
        ;;
esac
"##;
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn opencode_options(root: &Path, mode: &str, variant: Option<&str>) -> Options {
        Options::new_opencode(
            "return a bounded report",
            "opencode-go/test-model",
            variant,
            fake_opencode(root, mode),
        )
    }

    fn run_fake_opencode(root: &Path, mode: &str, variant: Option<&str>) -> (Value, String) {
        let result = run_with_options(opencode_options(root, mode, variant)).unwrap();
        let value = result_to_json(&result);
        let stderr = fs::read_to_string(value["artifacts"]["stderr"].as_str().unwrap()).unwrap();
        (value, stderr)
    }

    #[test]
    fn opencode_backend_selection_and_flag_validation() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };

        let options = parse_generic_args(&args(&[
            "--backend",
            "opencode",
            "--model",
            "opencode-go/deepseek-v4-flash",
            "--variant",
            "high",
            "--prompt",
            "task",
        ]))
        .unwrap();
        assert_eq!(options.backend, DelegationBackend::OpenCode);
        assert_eq!(options.variant.as_deref(), Some("high"));
        assert_eq!(options.reasoning_effort, None);

        let options = parse_generic_args(&args(&[
            "--backend",
            "codex",
            "--model",
            "gpt-5.6-luna",
            "--reasoning-effort",
            "high",
            "--prompt",
            "task",
        ]))
        .unwrap();
        assert_eq!(options.backend, DelegationBackend::Codex);
        assert_eq!(options.reasoning_effort.as_deref(), Some("high"));

        assert!(
            parse_generic_args(&args(&[
                "--backend",
                "codex",
                "--model",
                "m",
                "--prompt",
                "task"
            ]))
            .is_err()
        );
        assert!(
            parse_generic_args(&args(&[
                "--backend",
                "codex",
                "--model",
                "m",
                "--variant",
                "high",
                "--reasoning-effort",
                "high",
                "--prompt",
                "task"
            ]))
            .is_err()
        );
        assert!(
            parse_generic_args(&args(&[
                "--backend",
                "opencode",
                "--model",
                "m",
                "--reasoning-effort",
                "high",
                "--prompt",
                "task"
            ]))
            .is_err()
        );
        assert!(
            parse_generic_args(&args(&[
                "--backend",
                "other",
                "--model",
                "m",
                "--prompt",
                "task"
            ]))
            .is_err()
        );
        assert!(parse_generic_args(&args(&["--backend", "opencode", "--prompt", "task"])).is_err());
        assert!(run_generic_cli(&[]).unwrap().contains("--backend opencode"));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_command_is_structured_without_shell_and_filters_environment() {
        let root = tempfile::tempdir().unwrap();
        let options = opencode_options(root.path(), "success", Some("high"));
        let cwd = fs::canonicalize(root.path()).unwrap();
        let environment = vec![
            (OsString::from("HOME"), OsString::from("/home/user")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (
                OsString::from("OPENCODE_TEST_SECRET"),
                OsString::from("sentinel-secret-value"),
            ),
            (
                OsString::from("OP_SERVICE_ACCOUNT_TOKEN"),
                OsString::from("sentinel-op-token"),
            ),
            (
                OsString::from("TEMOTE_OPENCODE_BIN"),
                OsString::from("/sentinel/override/opencode"),
            ),
        ];
        let command = build_opencode_command(&options, &cwd, "effective prompt", environment);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args[0], "run");
        assert_eq!(args[1], "--pure");
        assert_eq!(args[2], "--format");
        assert_eq!(args[3], "json");
        assert_eq!(args[4], "--dir");
        assert_eq!(Path::new(&args[5]), cwd);
        assert_eq!(args[6], "--model");
        assert_eq!(args[7], "opencode-go/test-model");
        assert_eq!(args[8], "--variant");
        assert_eq!(args[9], "high");
        assert_eq!(args[10], "--");
        assert_eq!(args[11], "effective prompt");
        assert!(
            !args
                .iter()
                .any(|arg| arg == "sh" || arg == "-c" || arg == "bash")
        );

        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        assert!(envs.iter().any(|(key, _)| key == "HOME"));
        assert!(envs.iter().any(|(key, _)| key == "LC_ALL"));
        assert!(!envs.iter().any(|(key, _)| key == "OPENCODE_TEST_SECRET"));
        assert!(
            !envs
                .iter()
                .any(|(key, _)| key == "OP_SERVICE_ACCOUNT_TOKEN")
        );
        assert!(!envs.iter().any(|(key, _)| key == OPENCODE_BIN_ENV));
    }

    #[test]
    fn opencode_effective_prompt_embeds_report_contract() {
        let root = tempfile::tempdir().unwrap();
        let options = opencode_options(root.path(), "success", Some("high"));
        let prompt = opencode_effective_prompt(&options).unwrap();
        assert!(prompt.contains("respond with ONLY one JSON object"));
        assert!(prompt.contains("\"opencode-go/test-model\""));
        assert!(prompt.contains("\"high\""));
        assert!(prompt.ends_with("return a bounded report"));
    }

    #[test]
    fn opencode_success_is_normalized_to_backend_neutral_result() {
        let root = tempfile::tempdir().unwrap();
        let (value, stderr) = run_fake_opencode(root.path(), "success", None);
        assert_eq!(value["status"], "success");
        assert_eq!(value["requested"]["model"], "opencode-go/test-model");
        assert_eq!(value["requested"]["reasoning_effort"], "");
        assert_eq!(value["report"]["status"], "completed");
        assert_eq!(value["evidence"]["thread_id"], "ses_test");
        assert_eq!(value["evidence"]["usage"]["input_tokens"], 20);
        assert_eq!(value["evidence"]["usage"]["cached_input_tokens"], 3);
        assert_eq!(value["evidence"]["usage"]["output_tokens"], 5);
        assert_eq!(value["evidence"]["usage"]["reasoning_output_tokens"], 2);
        assert_eq!(value["evidence"]["usage"]["total_tokens"], 30);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["artifacts_truncated"], false);
        assert!(stderr.contains("<run><--pure><--format><json><--dir>"));
        assert!(stderr.contains("<--model><opencode-go/test-model><--><"));
        assert!(stderr.contains("You are a delegated implementation worker"));
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_observed_model_and_variant_are_recorded() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "observed", Some("high"));
        assert_eq!(value["status"], "success");
        assert_eq!(value["observed"]["model"], "opencode-go/deepseek-v4-flash");
        assert_eq!(value["observed"]["reasoning_effort"], Value::Null);
        assert_eq!(value["requested"]["reasoning_effort"], "high");
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_classifies_process_and_report_failures() {
        let root = tempfile::tempdir().unwrap();
        for (mode, expected) in [
            ("nonzero", "process_nonzero_exit"),
            ("missing", "missing_report"),
            ("invalid_json", "invalid_json"),
            ("invalid_schema", "invalid_report_schema"),
            ("oversized", "oversized_report"),
        ] {
            let (value, _) = run_fake_opencode(root.path(), mode, None);
            assert_eq!(value["status"], expected, "mode {mode}");
            assert_eq!(value["report"], Value::Null, "mode {mode}");
            fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
        }
    }

    #[test]
    fn opencode_bounded_stdout_sets_truncation_flag() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "huge_stdout", None);
        assert_eq!(value["artifacts_truncated"], true);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_timeout_classifies_and_does_not_hang() {
        let root = tempfile::tempdir().unwrap();
        let mut options = opencode_options(root.path(), "timeout", None);
        options.timeout = Some(Duration::from_millis(300));
        let started = Instant::now();
        let result = run_with_options(options).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout did not bound the child"
        );
        let value = result_to_json(&result);
        assert_eq!(value["status"], "process_timeout");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_missing_executable_is_backend_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist/opencode");
        let options =
            Options::new_opencode("task", "opencode-go/test-model", None, missing.clone());
        let cleaned_before = CLEANED_ARTIFACT_DIRECTORIES.load(Ordering::Relaxed);
        let error = run_with_options(options).unwrap_err();
        assert!(
            error.contains("OpenCode backend unavailable"),
            "unexpected error: {error}"
        );
        assert!(
            !error.contains("does-not-exist"),
            "error leaked the executable path: {error}"
        );
        assert!(
            CLEANED_ARTIFACT_DIRECTORIES.load(Ordering::Relaxed) > cleaned_before,
            "early launch failure did not clean up its artifacts"
        );
    }

    #[test]
    fn opencode_oversized_array_is_not_silently_truncated() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "oversized_array", None);
        assert_eq!(value["status"], "invalid_report_schema");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_extra_report_fields_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "extra_field", None);
        assert_eq!(value["status"], "invalid_report_schema");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_prompt_and_variant_bounds_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut options = opencode_options(root.path(), "success", None);
        options.prompt = "x".repeat(MAX_OPENCODE_PROMPT_BYTES);
        assert!(super::super::validate_options(&options).is_err());

        let options = opencode_options(root.path(), "success", Some(""));
        assert!(super::super::validate_options(&options).is_err());

        let mut options = opencode_options(root.path(), "success", Some("high"));
        options.model = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        assert!(super::super::validate_options(&options).is_err());
    }

    #[test]
    fn opencode_text_parts_use_the_last_message_and_ignore_earlier_messages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let report = r#"{"status":"completed","summary":"ok","base_commit":"abc","changed_files":[],"checks":[],"unresolved":[],"requested_model":"m","requested_effort":"","observed_model":null,"observed_effort":null}"#;
        let (first, second) = report.split_at(20);
        let events = [
            r#"{"type":"text","sessionID":"ses_test","part":{"messageID":"msg_old","type":"text","text":"ignore me"}}"#.to_owned(),
            r#"{"type":"step_finish","sessionID":"ses_test","part":{"type":"step-finish","tokens":{"input":1,"output":2,"total":3}}}"#.to_owned(),
            format!(
                r#"{{"type":"text","sessionID":"ses_test","part":{{"messageID":"msg_final","type":"text","text":{}}}}}"#,
                serde_json::to_string(first).unwrap()
            ),
            format!(
                r#"{{"type":"text","sessionID":"ses_test","part":{{"messageID":"msg_final","type":"text","text":{}}}}}"#,
                serde_json::to_string(second).unwrap()
            ),
        ];
        fs::write(&path, format!("{}\n", events.join("\n"))).unwrap();

        let (evidence, state) = collect_opencode_output(&path).unwrap();
        assert_eq!(evidence.thread_id.as_deref(), Some("ses_test"));
        assert_eq!(evidence.usage.as_ref().unwrap()["input_tokens"], 1);
        assert!(matches!(state, ReportState::Valid(_)), "state: {state:?}");
    }

    fn fake_opencode_diagnostic(root: &Path, mode: &str) -> PathBuf {
        let path = root.join(format!("fake-opencode-diagnostic-{mode}"));
        let script = r##"#!/bin/sh
set -eu
mode=${0##*-}
case "$1" in
    --version)
        case "$mode" in
            version_fail) printf 'boom\n' >&2; exit 3 ;;
            version_timeout) exec sleep 30 ;;
            version_oversized) head -c 5000 /dev/zero | tr '\0' 'x'; printf '\n' ;;
            version_malformed) printf 'not a version\n' ;;
            version_empty) : ;;
            *) printf '1.18.30\n' ;;
        esac
        ;;
    models)
        case "$mode" in
            models_requested_absent) printf 'opencode/mimo-v2.5-free\nopencode/big-pickle\n' ;;
            models_empty) : ;;
            models_fail) printf 'provider listing failed\n' >&2; exit 1 ;;
            models_unsupported) printf 'Unknown command: models\n' >&2; exit 1 ;;
            models_timeout) exec sleep 30 ;;
            models_oversized) yes opencode-go/model-x | head -c 9000000 ;;
            models_mixed) printf 'Available models:\nopencode-go/deepseek-v4-flash\n' ;;
            models_secret)
                printf 'OPENCODE_DIAGNOSTIC_SENTINEL=%s\n' "${OPENCODE_DIAGNOSTIC_SENTINEL:-missing}"
                printf 'stderr %s\n' "${OPENAI_API_KEY:-no-key}" >&2
                ;;
            *) printf 'opencode-go/deepseek-v4-flash\nopencode/mimo-v2.5-free\n' ;;
        esac
        ;;
    *)
        exit 9
        ;;
esac
"##;
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn diagnose_fake(root: &Path, mode: &str, model: Option<&str>) -> Value {
        diagnose_fake_with_timeouts(root, mode, model, OpenCodeDiagnosticTimeouts::default())
    }

    fn resolved_for_test(binary: &Path) -> ResolvedOpenCodeExecutable {
        ResolvedOpenCodeExecutable {
            source: OpenCodeExecutableSource::PathLookup,
            binary: binary.to_path_buf(),
        }
    }

    fn diagnose_fake_with_timeouts(
        root: &Path,
        mode: &str,
        model: Option<&str>,
        timeouts: OpenCodeDiagnosticTimeouts,
    ) -> Value {
        let binary = fake_opencode_diagnostic(root, mode);
        let diagnostics =
            opencode_diagnostics(&resolved_for_test(&binary), model, timeouts).unwrap();
        diagnostics_to_json(&diagnostics)
    }

    #[test]
    fn opencode_diagnostics_missing_binary_is_unavailable_without_false_ready() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing/opencode");
        let diagnostics = opencode_diagnostics(
            &resolved_for_test(&missing),
            Some("opencode-go/deepseek-v4-flash"),
            OpenCodeDiagnosticTimeouts::default(),
        )
        .unwrap();
        assert_eq!(
            diagnostics.executable,
            OpenCodeExecutableStatus::Unavailable
        );
        assert_eq!(diagnostics.version, OpenCodeVersionStatus::Unavailable);
        assert_eq!(diagnostics.models, OpenCodeModelsStatus::Unavailable);
        assert_eq!(
            diagnostics.requested_model_status,
            OpenCodeRequestedModelStatus::Unknown
        );
        let json = diagnostics_to_json(&diagnostics);
        assert_eq!(json["executable"]["status"], "unavailable");
        assert_eq!(json["executable"]["resolved"], false);
        assert_eq!(json["version"]["value"], Value::Null);
        assert_eq!(json["requested_model"]["status"], "unknown");

        let diagnostics = opencode_diagnostics(
            &resolved_for_test(&missing),
            None,
            OpenCodeDiagnosticTimeouts::default(),
        )
        .unwrap();
        assert_eq!(
            diagnostics.requested_model_status,
            OpenCodeRequestedModelStatus::NotChecked
        );
    }

    #[test]
    fn opencode_diagnostics_reports_version_and_requested_model_presence() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(
            root.path(),
            "models_ok",
            Some("opencode-go/deepseek-v4-flash"),
        );
        assert_eq!(json["backend"], "opencode");
        assert_eq!(json["executable"]["status"], "available");
        assert_eq!(json["executable"]["resolved"], true);
        assert_eq!(json["version"]["status"], "ready");
        assert_eq!(json["version"]["value"], "1.18.30");
        assert_eq!(json["models"]["status"], "ready");
        assert_eq!(json["models"]["count"], 2);
        assert_eq!(json["models"]["truncated"], false);
        assert_eq!(json["requested_model"]["status"], "present");
        assert_eq!(
            json["requested_model"]["value"],
            "opencode-go/deepseek-v4-flash"
        );
    }

    #[test]
    fn opencode_diagnostics_distinguishes_absent_from_unknown_requested_model() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(
            root.path(),
            "models_requested_absent",
            Some("opencode-go/deepseek-v4-flash"),
        );
        assert_eq!(json["models"]["status"], "ready");
        assert_eq!(json["requested_model"]["status"], "absent");

        let json = diagnose_fake(
            root.path(),
            "models_fail",
            Some("opencode-go/deepseek-v4-flash"),
        );
        assert_eq!(json["models"]["status"], "failed");
        assert_eq!(json["requested_model"]["status"], "unknown");
    }

    #[test]
    fn opencode_diagnostics_empty_model_listing_is_unavailable_not_absent() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(
            root.path(),
            "models_empty",
            Some("opencode-go/deepseek-v4-flash"),
        );
        assert_eq!(json["models"]["status"], "unavailable");
        assert_eq!(json["requested_model"]["status"], "unknown");
    }

    #[test]
    fn opencode_diagnostics_classifies_unsupported_model_command() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(root.path(), "models_unsupported", Some("provider/model"));
        assert_eq!(json["models"]["status"], "unsupported");
        assert_eq!(json["requested_model"]["status"], "unknown");
    }

    #[test]
    fn opencode_diagnostics_version_timeout_is_bounded_and_cleans_up_child() {
        let root = tempfile::tempdir().unwrap();
        let timeouts = OpenCodeDiagnosticTimeouts {
            version: Duration::from_millis(300),
            ..OpenCodeDiagnosticTimeouts::default()
        };
        let started = Instant::now();
        let json = diagnose_fake_with_timeouts(root.path(), "version_timeout", None, timeouts);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "version timeout did not bound the child"
        );
        assert_eq!(json["version"]["status"], "timeout");
        assert_eq!(json["version"]["value"], Value::Null);
        assert_eq!(json["requested_model"]["status"], "not_checked");
    }

    #[test]
    fn opencode_diagnostics_models_timeout_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        let timeouts = OpenCodeDiagnosticTimeouts {
            models: Duration::from_millis(300),
            ..OpenCodeDiagnosticTimeouts::default()
        };
        let started = Instant::now();
        let json = diagnose_fake_with_timeouts(
            root.path(),
            "models_timeout",
            Some("opencode-go/deepseek-v4-flash"),
            timeouts,
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "models timeout did not bound the child"
        );
        assert_eq!(json["version"]["status"], "ready");
        assert_eq!(json["models"]["status"], "timeout");
        assert_eq!(json["requested_model"]["status"], "unknown");
    }

    #[test]
    fn opencode_diagnostics_version_output_is_bounded_and_never_falsely_ready() {
        let root = tempfile::tempdir().unwrap();
        for mode in [
            "version_oversized",
            "version_malformed",
            "version_empty",
            "version_fail",
        ] {
            let json = diagnose_fake(root.path(), mode, None);
            assert_eq!(json["version"]["status"], "failed", "mode {mode}");
            assert_eq!(json["version"]["value"], Value::Null, "mode {mode}");
            assert!(json.to_string().len() < 2048, "mode {mode}");
        }
    }

    #[test]
    fn opencode_diagnostics_oversized_model_listing_is_bounded_and_unknown() {
        let root = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let json = diagnose_fake(root.path(), "models_oversized", Some("opencode-go/model-x"));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "oversized listing was not drained within the bound"
        );
        assert_eq!(json["models"]["status"], "ready");
        assert_eq!(json["models"]["truncated"], true);
        assert_eq!(json["requested_model"]["status"], "unknown");
        assert!(json.to_string().len() < 2048);
    }

    #[test]
    fn opencode_diagnostics_ignores_unrecognized_listing_lines() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(
            root.path(),
            "models_mixed",
            Some("opencode-go/deepseek-v4-flash"),
        );
        assert_eq!(json["models"]["status"], "ready");
        assert_eq!(json["models"]["count"], 1);
        assert_eq!(json["requested_model"]["status"], "present");
    }

    #[test]
    fn opencode_diagnostics_never_leaks_child_output_or_environment_secrets() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(root.path(), "models_secret", None);
        let encoded = json.to_string();
        assert!(!encoded.contains("OPENCODE_DIAGNOSTIC_SENTINEL"));
        assert!(!encoded.contains("sentinel-secret-value"));
        assert!(!encoded.contains("no-key"));

        let environment = vec![
            (OsString::from("HOME"), OsString::from("/home/user")),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (
                OsString::from("OPENAI_API_KEY"),
                OsString::from("sentinel-secret-value"),
            ),
            (
                OsString::from("ANTHROPIC_API_KEY"),
                OsString::from("sentinel-secret-value"),
            ),
            (
                OsString::from("OP_SERVICE_ACCOUNT_TOKEN"),
                OsString::from("sentinel-secret-value"),
            ),
            (
                OsString::from("OPENCODE_DIAGNOSTIC_SENTINEL"),
                OsString::from("sentinel-secret-value"),
            ),
        ];
        let command =
            build_opencode_probe_command(Path::new("opencode"), &["models", "--pure"], environment);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["models", "--pure"]);
        let envs = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(envs.iter().any(|key| key == "HOME"));
        assert!(envs.iter().any(|key| key == "LC_ALL"));
        for forbidden in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "OP_SERVICE_ACCOUNT_TOKEN",
            "OPENCODE_DIAGNOSTIC_SENTINEL",
        ] {
            assert!(
                !envs.iter().any(|key| key == forbidden),
                "diagnostics environment leaked {forbidden}"
            );
        }
    }

    #[test]
    fn opencode_diagnostics_cli_requires_the_opencode_backend() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(parse_diagnose_args(&args(&["--backend", "codex"])).is_err());
        assert!(parse_diagnose_args(&args(&["--backend", "other"])).is_err());
        assert!(parse_diagnose_args(&args(&["--unknown", "value"])).is_err());
        let oversized_model = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        assert!(
            parse_diagnose_args(&args(&[
                "--backend",
                "opencode",
                "--model",
                &oversized_model
            ]))
            .is_err()
        );
        assert_eq!(
            parse_diagnose_args(&args(&[
                "--backend",
                "opencode",
                "--model",
                "provider/model"
            ]))
            .unwrap()
            .as_deref(),
            Some("provider/model")
        );
        assert!(run_diagnose_cli(&[]).unwrap().contains("delegate diagnose"));
        assert!(run_generic_cli(&[]).unwrap().contains("delegate diagnose"));
    }

    #[test]
    fn parse_opencode_version_rejects_unexpected_output_without_panicking() {
        assert_eq!(
            parse_opencode_version(b"1.18.30\n").as_deref(),
            Some("1.18.30")
        );
        assert_eq!(
            parse_opencode_version(b"\n  1.18.30-dev.1+build  \n").as_deref(),
            Some("1.18.30-dev.1+build")
        );
        assert_eq!(parse_opencode_version(b"\n"), None);
        assert_eq!(parse_opencode_version(b"not a version\n"), None);
        assert_eq!(parse_opencode_version(b"beta\n"), None);
        assert_eq!(
            parse_opencode_version(&[b'9'; MAX_DIAGNOSTIC_VERSION_BYTES + 1]),
            None
        );
        assert!(parse_opencode_version(&[0xff, 0xfe, b'\n']).is_none());
    }

    #[test]
    fn model_identifier_grammar_ignores_headers_and_urls() {
        assert!(is_opencode_model_identifier(
            "opencode-go/deepseek-v4-flash"
        ));
        assert!(is_opencode_model_identifier("opencode/mimo-v2.5-free"));
        assert!(!is_opencode_model_identifier("Available models:"));
        assert!(!is_opencode_model_identifier("https://models.dev/api.json"));
        assert!(!is_opencode_model_identifier("/usr/bin/opencode"));
        assert!(!is_opencode_model_identifier("opencode/"));
        assert!(!is_opencode_model_identifier(""));
    }

    fn text_parts_for(text: &str) -> Vec<(Option<String>, String)> {
        vec![(Some("msg_1".to_owned()), text.to_owned())]
    }

    fn report_object(summary: &str) -> Value {
        json!({
            "status": "completed",
            "summary": summary,
            "base_commit": "",
            "changed_files": [],
            "checks": [],
            "unresolved": [],
            "requested_model": "raw-model",
            "requested_effort": "raw-effort",
            "observed_model": null,
            "observed_effort": null,
        })
    }

    #[test]
    fn opencode_report_repairs_raw_newlines_in_strings() {
        let text = concat!(
            "{\"status\":\"completed\",\"summary\":\"first line\n",
            "second line\",\"base_commit\":\"\",\"changed_files\":[],\"checks\":[],",
            "\"unresolved\":[],\"requested_model\":\"raw\",\"requested_effort\":\"\",",
            "\"observed_model\":null,\"observed_effort\":null}"
        );
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "the fixture must contain a raw newline and be invalid JSON"
        );
        let state = opencode_report_state(&text_parts_for(text));
        match state {
            ReportState::Valid(value) => {
                assert_eq!(value["summary"], "first line\nsecond line");
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn opencode_report_repairs_markdown_fenced_json() {
        let report = report_object("ok").to_string();
        let text = format!("```json\n{report}\n```");
        let state = opencode_report_state(&text_parts_for(&text));
        assert!(matches!(state, ReportState::Valid(_)), "state: {state:?}");
    }

    #[test]
    fn opencode_report_finds_json_after_surrounding_prose() {
        let report = report_object("ok").to_string();
        let text = format!("Here is the delegation report.\n{report}\nEnd of report.");
        let state = opencode_report_state(&text_parts_for(&text));
        assert!(matches!(state, ReportState::Valid(_)), "state: {state:?}");
    }

    #[test]
    fn opencode_report_uses_the_last_parseable_block() {
        let report = report_object("the real report").to_string();
        let text = format!("{{\"note\":\"an earlier example\"}}\n{report}");
        let state = opencode_report_state(&text_parts_for(&text));
        match state {
            ReportState::Valid(value) => {
                assert_eq!(value["summary"], "the real report");
            }
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn opencode_report_rejects_truncated_json() {
        let text = "{\"status\":\"completed\",\"summary\":\"cut off";
        let state = opencode_report_state(&text_parts_for(text));
        assert!(
            matches!(state, ReportState::InvalidJson),
            "state: {state:?}"
        );
    }

    #[test]
    fn opencode_report_extracts_from_text_beyond_the_direct_budget() {
        let prose = "analysis line with details\n".repeat(300);
        let report = report_object("ok").to_string();
        let text = format!("{prose}{report}");
        assert!(text.len() > DEFAULT_MAX_REPORT_BYTES);
        assert!(text.len() < MAX_REPORT_SCAN_BYTES);
        let state = opencode_report_state(&text_parts_for(&text));
        assert!(matches!(state, ReportState::Valid(_)), "state: {state:?}");
    }

    #[test]
    fn opencode_report_handles_unmatched_braces_before_the_json() {
        let report = report_object("ok").to_string();
        let text = format!("Consider the shape {{ like this\n{report}");
        let state = opencode_report_state(&text_parts_for(&text));
        assert!(matches!(state, ReportState::Valid(_)), "state: {state:?}");
    }

    #[test]
    fn opencode_report_rejects_text_beyond_the_scan_budget() {
        let text = "x".repeat(MAX_REPORT_SCAN_BYTES + 1);
        let state = opencode_report_state(&text_parts_for(&text));
        assert!(matches!(state, ReportState::Oversized), "state: {state:?}");
    }

    #[test]
    fn opencode_canonical_report_budget_rejects_oversized_arrays() {
        let mut value = report_object("ok");
        value["changed_files"] = json!(vec![
            "x".repeat(MAX_REPORT_ARRAY_ITEM_CHARS);
            MAX_REPORT_ARRAY_ITEMS
        ]);
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );
        let normalized = normalize_opencode_report(value, &options).unwrap();
        assert!(validate_report_schema(&normalized));
        assert!(!canonical_report_fits(&normalized));
    }

    #[test]
    fn opencode_normalization_bounds_summary_at_utf8_boundaries() {
        let long = "界".repeat(MAX_REPORT_SUMMARY_CHARS + 40);
        let value = report_object(&long);
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );
        let normalized = normalize_opencode_report(value, &options).unwrap();
        let summary = normalized["summary"].as_str().unwrap();
        assert_eq!(summary.chars().count(), MAX_REPORT_SUMMARY_CHARS);
        assert!(summary.ends_with("…[truncated]"));
        assert!(!summary.is_empty());
        assert!(validate_report_schema(&normalized));
    }

    #[test]
    fn opencode_normalization_keeps_short_summary_unmodified() {
        let value = report_object("short summary");
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            Some("high"),
            PathBuf::from("opencode"),
        );
        let normalized = normalize_opencode_report(value, &options).unwrap();
        assert_eq!(normalized["summary"], "short summary");
        assert_eq!(normalized["requested_model"], "opencode-go/canonical");
        assert_eq!(normalized["requested_effort"], "high");
        assert!(validate_report_schema(&normalized));
    }

    #[test]
    fn opencode_normalization_replaces_raw_requested_values() {
        let mut value = report_object("ok");
        value["requested_model"] = json!("\"opencode-go/quoted\"");
        value["requested_effort"] = json!("\"\"");
        let options = Options::new_opencode(
            "task",
            "opencode-go/deepseek-v4-flash",
            Some("high"),
            PathBuf::from("opencode"),
        );
        let normalized = normalize_opencode_report(value, &options).unwrap();
        assert_eq!(
            normalized["requested_model"],
            "opencode-go/deepseek-v4-flash"
        );
        assert_eq!(normalized["requested_effort"], "high");
        assert!(
            !normalized["requested_model"]
                .as_str()
                .unwrap()
                .contains('"')
        );
    }

    #[test]
    fn opencode_normalization_rejects_unrecoverable_reports() {
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );
        let mut missing_summary = report_object("ok");
        missing_summary.as_object_mut().unwrap().remove("summary");
        assert!(normalize_opencode_report(missing_summary, &options).is_none());

        let mut invalid_status = report_object("ok");
        invalid_status["status"] = json!("done");
        assert!(normalize_opencode_report(invalid_status, &options).is_none());

        let mut non_string_summary = report_object("ok");
        non_string_summary["summary"] = json!(42);
        assert!(normalize_opencode_report(non_string_summary, &options).is_none());
    }

    #[test]
    fn opencode_normalization_rejects_oversized_arrays_and_scalars() {
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );

        let mut too_many_items = report_object("ok");
        too_many_items["unresolved"] = json!(vec!["item"; MAX_REPORT_ARRAY_ITEMS + 1]);
        assert!(normalize_opencode_report(too_many_items, &options).is_none());

        let mut oversized_item = report_object("ok");
        oversized_item["checks"] = json!(vec!["x".repeat(MAX_REPORT_ARRAY_ITEM_CHARS + 1)]);
        assert!(normalize_opencode_report(oversized_item, &options).is_none());

        let mut oversized_commit = report_object("ok");
        oversized_commit["base_commit"] = json!("b".repeat(MAX_REPORT_COMMIT_CHARS + 1));
        assert!(normalize_opencode_report(oversized_commit, &options).is_none());

        let mut oversized_observed = report_object("ok");
        oversized_observed["observed_model"] = json!("m".repeat(MAX_REPORT_ARGUMENT_CHARS + 1));
        assert!(normalize_opencode_report(oversized_observed, &options).is_none());
    }

    #[test]
    fn opencode_normalization_accepts_values_at_the_schema_bounds() {
        let mut value = report_object("ok");
        value["base_commit"] = json!("b".repeat(MAX_REPORT_COMMIT_CHARS));
        value["unresolved"] = json!(vec![
            "x".repeat(MAX_REPORT_ARRAY_ITEM_CHARS);
            MAX_REPORT_ARRAY_ITEMS
        ]);
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );
        let normalized = normalize_opencode_report(value, &options).unwrap();
        assert_eq!(
            normalized["unresolved"].as_array().unwrap().len(),
            MAX_REPORT_ARRAY_ITEMS
        );
        assert!(validate_report_schema(&normalized));
        assert!(!canonical_report_fits(&normalized));
    }

    #[test]
    fn opencode_normalization_rejects_extra_top_level_fields() {
        let mut value = report_object("ok");
        value["extra"] = json!("unexpected");
        let options = Options::new_opencode(
            "task",
            "opencode-go/canonical",
            None,
            PathBuf::from("opencode"),
        );
        assert!(normalize_opencode_report(value, &options).is_none());
    }

    #[test]
    fn opencode_artifact_cleanup_removes_the_directory_unless_kept() {
        let root = tempfile::tempdir().unwrap();
        let removed_directory = root.path().join("removed");
        fs::create_dir(&removed_directory).unwrap();
        {
            let _cleanup = ArtifactCleanup::new(removed_directory.clone());
        }
        assert!(!removed_directory.exists());

        let kept_directory = root.path().join("kept");
        fs::create_dir(&kept_directory).unwrap();
        {
            let mut cleanup = ArtifactCleanup::new(kept_directory.clone());
            cleanup.keep();
        }
        assert!(kept_directory.exists());
    }

    #[test]
    fn opencode_usage_accumulates_each_step_once() {
        let mut usage: Option<Map<String, Value>> = None;
        accumulate_opencode_usage(
            &mut usage,
            opencode_usage(
                json!({"input": 10, "output": 2, "total": 12, "cache": {"read": 0}})
                    .as_object()
                    .unwrap(),
            ),
        );
        accumulate_opencode_usage(
            &mut usage,
            opencode_usage(
                json!({"input": 5, "output": 3, "total": 8, "cache": {"read": 4}})
                    .as_object()
                    .unwrap(),
            ),
        );
        let usage = usage.unwrap();
        assert_eq!(usage["input_tokens"], 15);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 20);
        assert_eq!(usage["cached_input_tokens"], 4);
    }

    #[test]
    fn opencode_success_persists_the_canonical_report_artifact() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "success", None);
        assert_eq!(value["status"], "success");
        assert_eq!(value["report"]["requested_model"], "opencode-go/test-model");
        let report_path = value["artifacts"]["report"].as_str().unwrap();
        let persisted: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(persisted, value["report"]);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_delivers_oversized_summary_with_a_truncation_marker() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "long_summary", None);
        assert_eq!(value["status"], "success");
        let summary = value["report"]["summary"].as_str().unwrap();
        assert_eq!(summary.chars().count(), MAX_REPORT_SUMMARY_CHARS);
        assert!(summary.ends_with("…[truncated]"));
        assert!(validate_report_schema(&value["report"]));
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_delivers_accumulated_multi_step_usage() {
        let root = tempfile::tempdir().unwrap();
        let (value, _) = run_fake_opencode(root.path(), "multi_step", None);
        assert_eq!(value["status"], "success");
        assert_eq!(value["evidence"]["usage"]["input_tokens"], 15);
        assert_eq!(value["evidence"]["usage"]["output_tokens"], 5);
        assert_eq!(value["evidence"]["usage"]["total_tokens"], 20);
        assert_eq!(value["evidence"]["usage"]["cached_input_tokens"], 4);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn opencode_report_contract_example_is_valid_json() {
        let root = tempfile::tempdir().unwrap();
        let options = opencode_options(root.path(), "success", Some("high"));
        let prompt = opencode_effective_prompt(&options).unwrap();
        let example = prompt
            .lines()
            .find(|line| line.starts_with("{\"status\":\"completed|failed"))
            .expect("report example line");
        let parsed: Value = serde_json::from_str(example).unwrap();
        assert_eq!(parsed["requested_model"], "opencode-go/test-model");
        assert_eq!(parsed["requested_effort"], "high");
    }

    fn executable_file(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn opencode_executable_resolver_prefers_a_valid_override() {
        let root = tempfile::tempdir().unwrap();
        let override_path = executable_file(root.path(), "override-opencode");
        let fallback = root.path().join("fallback-opencode");
        let resolved =
            resolve_opencode_executable(Some(override_path.to_str().unwrap()), &fallback).unwrap();
        assert_eq!(resolved.source(), OpenCodeExecutableSource::EnvOverride);
        assert_eq!(resolved.binary(), fs::canonicalize(&override_path).unwrap());
    }

    #[test]
    fn opencode_executable_resolver_falls_back_to_path_lookup_when_unset() {
        let resolved = resolve_opencode_executable(None, Path::new(OPENCODE_BINARY_NAME)).unwrap();
        assert_eq!(resolved.source(), OpenCodeExecutableSource::PathLookup);
        assert_eq!(resolved.binary(), Path::new(OPENCODE_BINARY_NAME));
    }

    #[test]
    fn opencode_executable_resolver_rejects_invalid_overrides_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing-opencode");

        for (value, expected) in [
            (String::new(), OpenCodeExecutableError::Empty),
            ("opencode".to_owned(), OpenCodeExecutableError::NotAbsolute),
            (
                "/tmp/embedded\0nul".to_owned(),
                OpenCodeExecutableError::InvalidValue,
            ),
            (
                format!("/{}", "a".repeat(MAX_OPENCODE_BIN_PATH_BYTES)),
                OpenCodeExecutableError::TooLong,
            ),
            (
                missing.to_str().unwrap().to_owned(),
                OpenCodeExecutableError::Missing,
            ),
            (
                root.path().to_str().unwrap().to_owned(),
                OpenCodeExecutableError::NotAFile,
            ),
        ] {
            let error =
                resolve_opencode_executable(Some(&value), Path::new("opencode")).unwrap_err();
            assert_eq!(error, expected, "value {value:?}");
        }

        let plain_file = root.path().join("plain-opencode");
        fs::write(&plain_file, "not executable").unwrap();
        fs::set_permissions(&plain_file, fs::Permissions::from_mode(0o600)).unwrap();
        let error =
            resolve_opencode_executable(Some(plain_file.to_str().unwrap()), Path::new("opencode"))
                .unwrap_err();
        assert_eq!(error, OpenCodeExecutableError::NotExecutable);
    }

    #[cfg(unix)]
    #[test]
    fn opencode_executable_resolver_canonicalizes_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = executable_file(root.path(), "opencode-target");
        let link = root.path().join("opencode-link");
        symlink(&target, &link).unwrap();
        let resolved =
            resolve_opencode_executable(Some(link.to_str().unwrap()), Path::new("opencode"))
                .unwrap();
        assert_eq!(resolved.source(), OpenCodeExecutableSource::EnvOverride);
        assert_eq!(resolved.binary(), fs::canonicalize(&target).unwrap());
    }

    #[test]
    fn opencode_executable_error_messages_hide_the_configured_path() {
        let root = tempfile::tempdir().unwrap();
        let sentinel_dir = root.path().join("sentinel-secret-directory");
        fs::create_dir_all(&sentinel_dir).unwrap();
        let sentinel_missing = sentinel_dir.join("opencode");

        for value in [
            sentinel_dir.to_str().unwrap().to_owned(),
            sentinel_missing.to_str().unwrap().to_owned(),
        ] {
            let error =
                resolve_opencode_executable(Some(&value), Path::new("opencode")).unwrap_err();
            assert!(!error.message().contains("sentinel-secret-directory"));
            assert!(
                !error
                    .delegation_message()
                    .contains("sentinel-secret-directory")
            );
            assert!(
                error
                    .delegation_message()
                    .starts_with("OpenCode backend unavailable: ")
            );
        }
    }

    #[test]
    fn opencode_invalid_override_diagnostics_report_source_without_a_probe() {
        let diagnostics = invalid_override_diagnostics(
            OpenCodeExecutableError::Missing,
            Some("opencode-go/deepseek-v4-flash"),
        );
        let json = diagnostics_to_json(&diagnostics);
        assert_eq!(json["executable"]["status"], "unavailable");
        assert_eq!(json["executable"]["resolved"], false);
        assert_eq!(json["executable"]["source"], "invalid_override");
        assert_eq!(json["executable"]["reason"], "not_found");
        assert_eq!(json["version"]["status"], "unavailable");
        assert_eq!(json["models"]["status"], "unavailable");
        assert_eq!(json["requested_model"]["status"], "unknown");
        assert!(!json.to_string().contains("sentinel"));
    }

    #[test]
    fn opencode_diagnostics_reports_the_env_override_source() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_opencode_diagnostic(root.path(), "models_ok");
        let executable = ResolvedOpenCodeExecutable {
            source: OpenCodeExecutableSource::EnvOverride,
            binary,
        };
        let diagnostics =
            opencode_diagnostics(&executable, None, OpenCodeDiagnosticTimeouts::default()).unwrap();
        let json = diagnostics_to_json(&diagnostics);
        assert_eq!(json["executable"]["status"], "available");
        assert_eq!(json["executable"]["source"], "env_override");
        assert_eq!(json["executable"].get("reason"), None);
    }

    #[test]
    fn opencode_diagnostics_reports_the_path_source_for_default_lookup() {
        let root = tempfile::tempdir().unwrap();
        let json = diagnose_fake(root.path(), "models_ok", None);
        assert_eq!(json["executable"]["source"], "path");
    }
}
