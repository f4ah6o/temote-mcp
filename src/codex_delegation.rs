use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Map, Value, json};
use uuid::Uuid;

pub(crate) const DEFAULT_MAX_REPORT_BYTES: usize = 4096;

const ARTIFACT_DIRECTORY_PREFIX: &str = "temote-codex-delegation-";
const MAX_PARENT_RESULT_BYTES: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_OPENCODE_PROMPT_BYTES: usize = 64 * 1024;
const OPENCODE_RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OPENCODE_DIAGNOSTIC_VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const OPENCODE_DIAGNOSTIC_MODELS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DIAGNOSTIC_VERSION_BYTES: usize = 128;
const MAX_DIAGNOSTIC_LISTING_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_ERROR_BYTES: usize = 4096;
const OPENCODE_BINARY_NAME: &str = "opencode";
const MAX_EVIDENCE_STRING_BYTES: usize = 256;
const MAX_EVIDENCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENT_LINE_BYTES: usize = 128 * 1024;
const OUTPUT_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "status",
    "summary",
    "base_commit",
    "changed_files",
    "checks",
    "unresolved",
    "requested_model",
    "requested_effort",
    "observed_model",
    "observed_effort"
  ],
  "properties": {
    "status": {
      "type": "string",
      "enum": ["completed", "failed", "blocked", "needs_decision"]
    },
    "summary": { "type": "string", "maxLength": 1200 },
    "base_commit": { "type": "string", "maxLength": 200 },
    "changed_files": {
      "type": "array",
      "maxItems": 128,
      "items": { "type": "string", "maxLength": 512 }
    },
    "checks": {
      "type": "array",
      "maxItems": 128,
      "items": { "type": "string", "maxLength": 512 }
    },
    "unresolved": {
      "type": "array",
      "maxItems": 128,
      "items": { "type": "string", "maxLength": 512 }
    },
    "requested_model": { "type": "string", "maxLength": 256 },
    "requested_effort": { "type": "string", "maxLength": 256 },
    "observed_model": { "type": ["string", "null"], "maxLength": 256 },
    "observed_effort": { "type": ["string", "null"], "maxLength": 256 }
  }
}
"#;

const CODEX_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "CODEX_HOME",
    "HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "LANG",
    "LOGNAME",
    "NO_PROXY",
    "OPENAI_API_KEY",
    "PATH",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
];

const TOKEN_USAGE_FIELDS: &[&str] = &[
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
];

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
{"status":"completed|failed|blocked|needs_decision","summary":"short summary, at most 1200 characters","base_commit":"","changed_files":[],"checks":[],"unresolved":[],"requested_model":"__REQUESTED_MODEL__","requested_effort":"__REQUESTED_EFFORT__","observed_model":null,"observed_effort":null}
Rules:
- All string values are plain strings; changed_files, checks, and unresolved are arrays of strings (use [] when empty).
- Set "requested_model" to "__REQUESTED_MODEL__" and "requested_effort" to "__REQUESTED_EFFORT__".
- Set "observed_model"/"observed_effort" only when you can actually observe them; otherwise keep null.
- Do not include any other fields.

Task:
"#;

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) backend: DelegationBackend,
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) codex_binary: PathBuf,
    pub(crate) opencode_binary: PathBuf,
    pub(crate) timeout: Option<Duration>,
}

impl Options {
    #[cfg(test)]
    fn new(prompt: &str, model: &str, reasoning_effort: &str, codex_binary: PathBuf) -> Self {
        Self {
            backend: DelegationBackend::Codex,
            prompt: prompt.to_owned(),
            model: model.to_owned(),
            reasoning_effort: Some(reasoning_effort.to_owned()),
            variant: None,
            codex_binary,
            opencode_binary: PathBuf::from(OPENCODE_BINARY_NAME),
            timeout: None,
        }
    }

    #[cfg(test)]
    fn new_opencode(
        prompt: &str,
        model: &str,
        variant: Option<&str>,
        opencode_binary: PathBuf,
    ) -> Self {
        Self {
            backend: DelegationBackend::OpenCode,
            prompt: prompt.to_owned(),
            model: model.to_owned(),
            reasoning_effort: None,
            variant: variant.map(str::to_owned),
            codex_binary: PathBuf::from("codex"),
            opencode_binary,
            timeout: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    Success,
    ProcessNonzeroExit,
    ProcessTimeout,
    MissingReport,
    InvalidJson,
    InvalidReportSchema,
    OversizedReport,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ProcessNonzeroExit => "process_nonzero_exit",
            Self::ProcessTimeout => "process_timeout",
            Self::MissingReport => "missing_report",
            Self::InvalidJson => "invalid_json",
            Self::InvalidReportSchema => "invalid_report_schema",
            Self::OversizedReport => "oversized_report",
        }
    }
}

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
struct OpenCodeDiagnosticTimeouts {
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
struct OpenCodeDiagnostics {
    executable: OpenCodeExecutableStatus,
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

#[derive(Debug, Default)]
struct Evidence {
    thread_id: Option<String>,
    usage: Option<Map<String, Value>>,
    observed_model: Option<String>,
    observed_reasoning_effort: Option<String>,
}

#[derive(Clone, Debug)]
struct ArtifactPaths {
    directory: PathBuf,
    events: PathBuf,
    stderr: PathBuf,
    report: PathBuf,
    schema: PathBuf,
}

struct Artifacts {
    paths: ArtifactPaths,
    events_file: File,
    stderr_file: File,
}

#[derive(Debug)]
struct DelegationResult {
    backend: DelegationBackend,
    status: Status,
    requested_model: String,
    requested_reasoning_effort: String,
    observed_model: Option<String>,
    observed_reasoning_effort: Option<String>,
    report: Option<Value>,
    evidence: Evidence,
    exit_code: Option<i32>,
    artifacts_truncated: bool,
    artifacts: ArtifactPaths,
}

#[derive(Debug)]
enum ReportState {
    Missing,
    InvalidJson,
    InvalidSchema,
    Oversized,
    Valid(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DelegationBackend {
    Codex,
    OpenCode,
}

impl DelegationBackend {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            _ => Err(format!(
                "unsupported delegation backend {value:?}; expected codex or opencode"
            )),
        }
    }
}

#[derive(Debug)]
struct NormalizedEvidence {
    thread_id: Option<String>,
    usage: Option<Map<String, Value>>,
}

#[derive(Debug)]
struct NormalizedResult {
    #[allow(dead_code)]
    backend: DelegationBackend,
    status: Status,
    requested_model: String,
    requested_variant: String,
    observed_model: Option<String>,
    observed_variant: Option<String>,
    report: Option<Value>,
    evidence: NormalizedEvidence,
    exit_code: Option<i32>,
    artifacts_truncated: bool,
    artifacts: ArtifactPaths,
}

impl DelegationResult {
    fn normalize(&self) -> NormalizedResult {
        NormalizedResult {
            backend: self.backend,
            status: self.status,
            requested_model: self.requested_model.clone(),
            requested_variant: self.requested_reasoning_effort.clone(),
            observed_model: self.observed_model.clone(),
            observed_variant: self.observed_reasoning_effort.clone(),
            report: self.report.clone(),
            evidence: NormalizedEvidence {
                thread_id: self.evidence.thread_id.clone(),
                usage: self.evidence.usage.clone(),
            },
            exit_code: self.exit_code,
            artifacts_truncated: self.artifacts_truncated,
            artifacts: self.artifacts.clone(),
        }
    }
}

pub(crate) fn usage() -> String {
    r#"Experimental Codex delegation bootstrap

Usage:
  temote-mcp codex delegate --model <MODEL> --reasoning-effort <EFFORT> --prompt <PROMPT>
  temote-mcp codex delegate --model <MODEL> --reasoning-effort <EFFORT> --prompt-file <PATH>

This developer-only command runs the installed codex exec CLI, captures JSONL
and stderr as private temporary artifacts, and prints one bounded JSON result.
"#
    .to_owned()
}

pub(crate) fn generic_usage() -> String {
    r#"Delegation backend

Usage:
  temote-mcp delegate --backend codex --model <MODEL> --reasoning-effort <EFFORT> --prompt <PROMPT>
  temote-mcp delegate --backend codex --model <MODEL> --reasoning-effort <EFFORT> --prompt-file <PATH>
  temote-mcp delegate --backend opencode --model <provider/model> [--variant <VARIANT>] --prompt <PROMPT>
  temote-mcp delegate --backend opencode --model <provider/model> [--variant <VARIANT>] --prompt-file <PATH>
  temote-mcp delegate diagnose --backend opencode [--model <provider/model>]

Each request runs one bounded, non-interactive delegation process and prints
one bounded JSON result. The legacy `temote-mcp codex delegate ...` command
always uses the Codex backend. Diagnostics are read-only and never log in,
change credentials, download models, or run a delegation task.
"#
    .to_owned()
}

pub(crate) fn run_cli(args: &[String]) -> Result<String, String> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(usage());
    }

    let options = parse_args(args)?;
    let result = run_with_options(options)?;
    serialize_parent_result(&result)
        .map(|json| format!("{json}\n"))
        .map_err(|error| format!("could not serialize Codex delegation result: {error}"))
}

pub(crate) fn run_generic_cli(args: &[String]) -> Result<String, String> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(generic_usage());
    }

    let options = parse_generic_args(args)?;
    let result = run_with_options(options)?;
    serialize_parent_result(&result)
        .map(|json| format!("{json}\n"))
        .map_err(|error| format!("could not serialize delegation result: {error}"))
}

pub(crate) fn run_diagnose_cli(args: &[String]) -> Result<String, String> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(diagnostics_usage());
    }

    let requested_model = parse_diagnose_args(args)?;
    let diagnostics = opencode_diagnostics(
        Path::new(OPENCODE_BINARY_NAME),
        requested_model.as_deref(),
        OpenCodeDiagnosticTimeouts::default(),
    )?;
    serde_json::to_string(&diagnostics_to_json(&diagnostics))
        .map(|json| format!("{json}\n"))
        .map_err(|error| format!("could not serialize OpenCode diagnostics: {error}"))
}

fn diagnostics_usage() -> String {
    r#"OpenCode delegation diagnostics (read-only)

Usage:
  temote-mcp delegate diagnose --backend opencode [--model <provider/model>]

Probes the installed OpenCode CLI, its version, and local model discovery.
The command never logs in, changes credentials, downloads models, or runs a
delegation task.
"#
    .to_owned()
}

fn parse_diagnose_args(args: &[String]) -> Result<Option<String>, String> {
    let mut backend = None;
    let mut model = None;
    let mut index = 0;

    while index < args.len() {
        let flag = &args[index];
        match flag.as_str() {
            "--backend" => {
                let value = argument_value(args, &mut index, flag)?;
                backend = Some(
                    DelegationBackend::parse(&value)
                        .map_err(|error| format!("{error}\n\n{}", diagnostics_usage()))?,
                );
            }
            "--model" => model = Some(argument_value(args, &mut index, flag)?),
            _ => {
                return Err(format!(
                    "unsupported delegation diagnostics option: {flag}\n\n{}",
                    diagnostics_usage()
                ));
            }
        }
        index += 1;
    }

    let backend = match backend {
        Some(backend) => backend,
        None => match std::env::var("TEMOTE_DELEGATION_BACKEND") {
            Ok(value) if !value.trim().is_empty() => DelegationBackend::parse(value.trim())
                .map_err(|error| format!("{error}\n\n{}", diagnostics_usage()))?,
            _ => {
                return Err(format!(
                    "--backend is required for delegation diagnostics\n\n{}",
                    diagnostics_usage()
                ));
            }
        },
    };
    if backend != DelegationBackend::OpenCode {
        return Err(format!(
            "delegation diagnostics currently support only the opencode backend\n\n{}",
            diagnostics_usage()
        ));
    }
    if let Some(model) = &model {
        validate_model_argument(model)?;
    }
    Ok(model)
}

fn opencode_diagnostics(
    binary: &Path,
    requested_model: Option<&str>,
    timeouts: OpenCodeDiagnosticTimeouts,
) -> Result<OpenCodeDiagnostics, String> {
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

fn diagnostics_to_json(diagnostics: &OpenCodeDiagnostics) -> Value {
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

    json!({
        "backend": DelegationBackend::OpenCode.name(),
        "executable": {
            "status": diagnostics.executable.as_str(),
            "resolved": diagnostics.executable == OpenCodeExecutableStatus::Available,
        },
        "version": Value::Object(version),
        "models": Value::Object(models),
        "requested_model": Value::Object(requested),
    })
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut model = None;
    let mut reasoning_effort = None;
    let mut prompt = None;
    let mut prompt_file = None;
    let mut index = 0;

    while index < args.len() {
        let flag = &args[index];
        match flag.as_str() {
            "--model" => model = Some(argument_value(args, &mut index, flag)?),
            "--reasoning-effort" => {
                reasoning_effort = Some(argument_value(args, &mut index, flag)?)
            }
            "--prompt" => prompt = Some(argument_value(args, &mut index, flag)?),
            "--prompt-file" => prompt_file = Some(argument_value(args, &mut index, flag)?),
            _ => {
                return Err(format!(
                    "unsupported Codex delegation option: {flag}\n\n{}",
                    usage()
                ));
            }
        }
        index += 1;
    }

    let prompt = delegation_prompt(prompt, prompt_file)?;
    let model = model.ok_or_else(|| format!("--model is required\n\n{}", usage()))?;
    let reasoning_effort =
        reasoning_effort.ok_or_else(|| format!("--reasoning-effort is required\n\n{}", usage()))?;

    Ok(Options {
        backend: DelegationBackend::Codex,
        prompt,
        model,
        reasoning_effort: Some(reasoning_effort),
        variant: None,
        codex_binary: PathBuf::from("codex"),
        opencode_binary: PathBuf::from(OPENCODE_BINARY_NAME),
        timeout: None,
    })
}

fn parse_generic_args(args: &[String]) -> Result<Options, String> {
    let mut backend = None;
    let mut model = None;
    let mut reasoning_effort = None;
    let mut variant = None;
    let mut prompt = None;
    let mut prompt_file = None;
    let mut index = 0;

    while index < args.len() {
        let flag = &args[index];
        match flag.as_str() {
            "--backend" => {
                let value = argument_value(args, &mut index, flag)?;
                backend = Some(
                    DelegationBackend::parse(&value)
                        .map_err(|error| format!("{error}\n\n{}", generic_usage()))?,
                );
            }
            "--model" => model = Some(argument_value(args, &mut index, flag)?),
            "--reasoning-effort" => {
                reasoning_effort = Some(argument_value(args, &mut index, flag)?)
            }
            "--variant" => variant = Some(argument_value(args, &mut index, flag)?),
            "--prompt" => prompt = Some(argument_value(args, &mut index, flag)?),
            "--prompt-file" => prompt_file = Some(argument_value(args, &mut index, flag)?),
            _ => {
                return Err(format!(
                    "unsupported delegation option: {flag}\n\n{}",
                    generic_usage()
                ));
            }
        }
        index += 1;
    }

    let backend = match backend {
        Some(backend) => backend,
        None => match std::env::var("TEMOTE_DELEGATION_BACKEND") {
            Ok(value) if !value.trim().is_empty() => DelegationBackend::parse(value.trim())
                .map_err(|error| format!("{error}\n\n{}", generic_usage()))?,
            _ => DelegationBackend::Codex,
        },
    };
    let prompt = delegation_prompt(prompt, prompt_file)?;
    let model = model.ok_or_else(|| format!("--model is required\n\n{}", generic_usage()))?;

    let reasoning_effort = match backend {
        DelegationBackend::Codex => {
            if variant.is_some() {
                return Err(format!(
                    "--variant is only supported by the opencode backend\n\n{}",
                    generic_usage()
                ));
            }
            Some(reasoning_effort.ok_or_else(|| {
                format!(
                    "--reasoning-effort is required for the codex backend\n\n{}",
                    generic_usage()
                )
            })?)
        }
        DelegationBackend::OpenCode => {
            if reasoning_effort.is_some() {
                return Err(format!(
                    "--reasoning-effort is only supported by the codex backend; use --variant for opencode\n\n{}",
                    generic_usage()
                ));
            }
            None
        }
    };

    Ok(Options {
        backend,
        prompt,
        model,
        reasoning_effort,
        variant,
        codex_binary: PathBuf::from("codex"),
        opencode_binary: PathBuf::from(OPENCODE_BINARY_NAME),
        timeout: None,
    })
}

fn delegation_prompt(
    prompt: Option<String>,
    prompt_file: Option<String>,
) -> Result<String, String> {
    match (prompt, prompt_file) {
        (Some(_), Some(_)) => Err("--prompt and --prompt-file cannot be combined".to_owned()),
        (Some(prompt), None) => Ok(prompt),
        (None, Some(path)) => read_prompt_file(Path::new(&path)),
        (None, None) => Err("a prompt is required".to_owned()),
    }
}

fn argument_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn read_prompt_file(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect Codex prompt file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Codex prompt file must be a regular non-symlink file".to_owned());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not read Codex prompt file: {error}"))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read Codex prompt file: {error}"))?;
    if bytes.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "Codex prompt file exceeds {MAX_PROMPT_BYTES} bytes"
        ));
    }
    String::from_utf8(bytes).map_err(|_| "Codex prompt file is not valid UTF-8".to_owned())
}

fn run_with_options(options: Options) -> Result<DelegationResult, String> {
    validate_options(&options)?;
    match options.backend {
        DelegationBackend::Codex => run_codex(options),
        DelegationBackend::OpenCode => run_opencode(options),
    }
}

fn run_codex(options: Options) -> Result<DelegationResult, String> {
    let artifacts = create_artifacts()?;
    let paths = artifacts.paths.clone();
    let reasoning_effort = options
        .reasoning_effort
        .as_deref()
        .ok_or_else(|| "Codex reasoning effort is required".to_owned())?;
    let reasoning_override = format!(
        "model_reasoning_effort={}",
        serde_json::to_string(reasoning_effort)
            .map_err(|error| format!("could not encode reasoning effort: {error}"))?
    );

    let working_directory =
        std::env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(|error| {
                format!("could not resolve Codex delegation working directory: {error}")
            })?;
    let mut command = build_codex_command(
        &options,
        &paths,
        &working_directory,
        &reasoning_override,
        std::env::vars_os(),
    );
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start Codex delegation: {error}"))?;

    let artifacts_truncated =
        wait_with_bounded_artifacts(&mut child, artifacts.events_file, artifacts.stderr_file)?;
    let status = child
        .try_wait()
        .map_err(|error| format!("could not inspect Codex delegation status: {error}"))?
        .ok_or_else(|| "Codex delegation exited without a process status".to_owned())?;

    secure_artifact_files(&paths);
    let evidence = collect_evidence(&paths.events).unwrap_or_default();
    let exit_code = status.code();
    if !status.success() {
        return Ok(DelegationResult {
            backend: DelegationBackend::Codex,
            status: Status::ProcessNonzeroExit,
            requested_model: options.model,
            requested_reasoning_effort: reasoning_effort.to_owned(),
            observed_model: evidence.observed_model.clone(),
            observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
            report: None,
            evidence,
            exit_code,
            artifacts_truncated,
            artifacts: paths,
        });
    }

    let report_state = read_report(&paths.report, DEFAULT_MAX_REPORT_BYTES);
    let (report_status, report) = match report_state {
        ReportState::Missing => (Status::MissingReport, None),
        ReportState::InvalidJson => (Status::InvalidJson, None),
        ReportState::InvalidSchema => (Status::InvalidReportSchema, None),
        ReportState::Oversized => (Status::OversizedReport, None),
        ReportState::Valid(report) => (Status::Success, Some(report)),
    };

    Ok(DelegationResult {
        backend: DelegationBackend::Codex,
        status: report_status,
        requested_model: options.model,
        requested_reasoning_effort: reasoning_effort.to_owned(),
        observed_model: evidence.observed_model.clone(),
        observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
        report,
        evidence,
        exit_code,
        artifacts_truncated,
        artifacts: paths,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    Exited,
    TimedOut,
}

fn run_opencode(options: Options) -> Result<DelegationResult, String> {
    let artifacts = create_artifacts()?;
    let paths = artifacts.paths.clone();
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
        .map_err(|error| opencode_launch_error(&options, &error))?;

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
        ReportState::Missing => (Status::MissingReport, None),
        ReportState::InvalidJson => (Status::InvalidJson, None),
        ReportState::InvalidSchema => (Status::InvalidReportSchema, None),
        ReportState::Oversized => (Status::OversizedReport, None),
        ReportState::Valid(report) => (Status::Success, Some(report)),
    };

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

fn opencode_launch_error(options: &Options, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "OpenCode backend unavailable: {} was not found; install OpenCode or add it to PATH (callers cannot supply an executable path)",
            options.opencode_binary.display()
        )
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
    environment
        .into_iter()
        .filter(|(key, _)| opencode_environment_key_allowed(key))
        .collect()
}

fn opencode_environment_key_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || OPENCODE_CHILD_ENV_ALLOWLIST.contains(&key)
}

fn wait_with_bounded_artifacts_timeout(
    child: &mut Child,
    events_file: File,
    stderr_file: File,
    timeout: Duration,
) -> Result<(WaitOutcome, bool, bool), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "OpenCode delegation stdout pipe is unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "OpenCode delegation stderr pipe is unavailable".to_owned())?;
    let deadline = Instant::now() + timeout;

    let (outcome, stdout_result, stderr_result) = std::thread::scope(|scope| {
        let stdout_handle = scope.spawn(|| capture_artifact(stdout, events_file));
        let stderr_handle = scope.spawn(|| capture_artifact(stderr, stderr_file));
        let outcome = (|| -> Result<WaitOutcome, String> {
            loop {
                if child
                    .try_wait()
                    .map_err(|error| format!("could not wait for OpenCode delegation: {error}"))?
                    .is_some()
                {
                    return Ok(WaitOutcome::Exited);
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(WaitOutcome::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })();
        (outcome, stdout_handle.join(), stderr_handle.join())
    });

    let stdout_truncated = stdout_result
        .map_err(|_| "OpenCode delegation stdout capture thread panicked".to_owned())?
        .map_err(|error| format!("could not capture OpenCode delegation JSON events: {error}"))?;
    let stderr_truncated = stderr_result
        .map_err(|_| "OpenCode delegation stderr capture thread panicked".to_owned())?
        .map_err(|error| format!("could not capture OpenCode delegation stderr: {error}"))?;
    Ok((outcome?, stdout_truncated, stderr_truncated))
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
                    evidence.usage = Some(opencode_usage(tokens));
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
    let Some((last_message_id, _)) = text_parts.last() else {
        return ReportState::Missing;
    };
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

    if text.len() > DEFAULT_MAX_REPORT_BYTES {
        return ReportState::Oversized;
    }
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return ReportState::InvalidJson;
    };
    if !validate_report_schema(&value) {
        return ReportState::InvalidSchema;
    }
    ReportState::Valid(value)
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

fn wait_with_bounded_artifacts(
    child: &mut Child,
    events_file: File,
    stderr_file: File,
) -> Result<bool, String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Codex delegation stdout pipe is unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Codex delegation stderr pipe is unavailable".to_owned())?;

    let (status, stdout_result, stderr_result) = std::thread::scope(|scope| {
        let stdout_handle = scope.spawn(|| capture_artifact(stdout, events_file));
        let stderr_handle = scope.spawn(|| capture_artifact(stderr, stderr_file));
        let status = child.wait();
        let stdout_result = stdout_handle.join();
        let stderr_result = stderr_handle.join();
        (status, stdout_result, stderr_result)
    });

    status.map_err(|error| format!("could not wait for Codex delegation: {error}"))?;
    let stdout_truncated = stdout_result
        .map_err(|_| "Codex delegation stdout capture thread panicked".to_owned())?
        .map_err(|error| format!("could not capture Codex delegation JSONL: {error}"))?;
    let stderr_truncated = stderr_result
        .map_err(|_| "Codex delegation stderr capture thread panicked".to_owned())?
        .map_err(|error| format!("could not capture Codex delegation stderr: {error}"))?;
    Ok(stdout_truncated || stderr_truncated)
}

fn capture_artifact<R: Read>(mut reader: R, mut file: File) -> std::io::Result<bool> {
    let mut bounded = reader.by_ref().take(MAX_ARTIFACT_BYTES);
    std::io::copy(&mut bounded, &mut file)?;
    let mut discarded = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut discarded)?;
        if read == 0 {
            break;
        }
        truncated = true;
    }
    file.flush()?;
    file.sync_all()?;
    Ok(truncated)
}

fn build_codex_command<I>(
    options: &Options,
    paths: &ArtifactPaths,
    working_directory: &Path,
    reasoning_override: &str,
    environment: I,
) -> Command
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut command = Command::new(&options.codex_binary);
    command
        .arg("exec")
        .arg("--ignore-user-config")
        .arg("--ephemeral")
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--cd")
        .arg(working_directory)
        .arg("--model")
        .arg(&options.model)
        .arg("--config")
        .arg(reasoning_override)
        .arg("--config")
        .arg("shell_environment_policy.inherit=\"core\"")
        .arg("--json")
        .arg("--output-schema")
        .arg(&paths.schema)
        .arg("--output-last-message")
        .arg(&paths.report)
        .arg("--")
        .arg(&options.prompt)
        .env_clear();
    for (key, value) in filtered_codex_environment(environment) {
        command.env(key, value);
    }
    command
}

fn filtered_codex_environment<I>(environment: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter(|(key, _)| codex_environment_key_allowed(key))
        .collect()
}

fn codex_environment_key_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || CODEX_CHILD_ENV_ALLOWLIST.contains(&key)
}

fn validate_model_argument(model: &str) -> Result<(), String> {
    if model.is_empty() || model.len() > MAX_ARGUMENT_BYTES || model.contains('\0') {
        return Err(format!(
            "delegation model must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_options(options: &Options) -> Result<(), String> {
    validate_model_argument(&options.model)?;
    if options.prompt.is_empty()
        || options.prompt.len() > MAX_PROMPT_BYTES
        || options.prompt.contains('\0')
    {
        return Err(format!(
            "delegation prompt must be non-empty, NUL-free, and at most {MAX_PROMPT_BYTES} bytes"
        ));
    }
    match options.backend {
        DelegationBackend::Codex => {
            let reasoning_effort = options
                .reasoning_effort
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!(
                        "Codex reasoning effort must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
                    )
                })?;
            if reasoning_effort.len() > MAX_ARGUMENT_BYTES || reasoning_effort.contains('\0') {
                return Err(format!(
                    "Codex reasoning effort must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
                ));
            }
        }
        DelegationBackend::OpenCode => {
            if let Some(variant) = &options.variant
                && (variant.is_empty()
                    || variant.len() > MAX_ARGUMENT_BYTES
                    || variant.contains('\0'))
            {
                return Err(format!(
                    "OpenCode variant must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
                ));
            }
            opencode_effective_prompt(options)?;
        }
    }
    Ok(())
}

fn create_artifacts() -> Result<Artifacts, String> {
    let parent = std::env::temp_dir();
    for _ in 0..8 {
        let directory = parent.join(format!("{ARTIFACT_DIRECTORY_PREFIX}{}", Uuid::new_v4()));
        match fs::create_dir(&directory) {
            Ok(()) => {
                let result = create_artifacts_in(&directory);
                if result.is_err() {
                    let _ = fs::remove_dir_all(&directory);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create delegation artifact directory: {error}"
                ));
            }
        }
    }
    Err("could not allocate a unique delegation artifact directory".to_owned())
}

fn create_artifacts_in(directory: &Path) -> Result<Artifacts, String> {
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure delegation artifacts: {error}"))?;

    let paths = ArtifactPaths {
        directory: directory.to_path_buf(),
        events: directory.join("events.jsonl"),
        stderr: directory.join("stderr.log"),
        report: directory.join("report.json"),
        schema: directory.join("output-schema.json"),
    };
    let mut schema = create_private_file(&paths.schema)?;
    schema
        .write_all(OUTPUT_SCHEMA.as_bytes())
        .and_then(|_| schema.sync_all())
        .map_err(|error| format!("could not write Codex delegation output schema: {error}"))?;
    drop(schema);

    let events_file = create_private_file(&paths.events)?;
    let stderr_file = create_private_file(&paths.stderr)?;
    Ok(Artifacts {
        paths,
        events_file,
        stderr_file,
    })
}

fn create_private_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    options
        .open(path)
        .map_err(|error| format!("could not create private delegation artifact: {error}"))
}

fn secure_artifact_files(paths: &ArtifactPaths) {
    #[cfg(unix)]
    for path in [&paths.events, &paths.stderr, &paths.report, &paths.schema] {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

fn collect_evidence(path: &Path) -> std::io::Result<Evidence> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let mut reader = BufReader::new(file.take(MAX_EVIDENCE_BYTES.saturating_add(1)));
    let mut line = Vec::new();
    let mut evidence = Evidence::default();

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
            evidence.thread_id = bounded_string(object.get("thread_id"));
        }
        if evidence.observed_model.is_none() {
            evidence.observed_model = bounded_string(object.get("model"));
        }
        if evidence.observed_reasoning_effort.is_none() {
            evidence.observed_reasoning_effort = bounded_string(
                object
                    .get("reasoning_effort")
                    .or_else(|| object.get("model_reasoning_effort")),
            );
        }

        if object.get("type").and_then(Value::as_str) == Some("turn.completed") {
            evidence.usage = object
                .get("usage")
                .and_then(Value::as_object)
                .map(filtered_usage);
        }
    }
    Ok(evidence)
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    line.clear();
    let mut saw_bytes = false;
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return if saw_bytes {
                Ok(Some(!oversized))
            } else {
                Ok(None)
            };
        }
        saw_bytes = true;
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            if !oversized {
                let remaining = MAX_EVENT_LINE_BYTES.saturating_sub(line.len());
                if newline <= remaining {
                    line.extend_from_slice(&chunk[..newline]);
                } else {
                    oversized = true;
                }
            }
            reader.consume(newline + 1);
            return Ok(Some(!oversized));
        }
        if !oversized {
            let remaining = MAX_EVENT_LINE_BYTES.saturating_sub(line.len());
            if chunk.len() <= remaining {
                line.extend_from_slice(chunk);
            } else {
                if remaining > 0 {
                    line.extend_from_slice(&chunk[..remaining]);
                }
                oversized = true;
            }
        }
        let consumed = chunk.len();
        reader.consume(consumed);
    }
}

fn bounded_string(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    if value.is_empty() || value.len() > MAX_EVIDENCE_STRING_BYTES {
        return None;
    }
    Some(value.to_owned())
}

fn filtered_usage(usage: &Map<String, Value>) -> Map<String, Value> {
    TOKEN_USAGE_FIELDS
        .iter()
        .filter_map(|field| {
            usage
                .get(*field)
                .filter(|value| value.as_u64().is_some())
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect()
}

fn read_report(path: &Path, max_bytes: usize) -> ReportState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReportState::Missing;
        }
        Err(_) => return ReportState::InvalidJson,
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return ReportState::InvalidJson;
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let Ok(mut file) = options.open(path) else {
        return ReportState::InvalidJson;
    };
    let mut bytes = Vec::with_capacity(max_bytes.saturating_add(1));
    if Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return ReportState::InvalidJson;
    }
    if bytes.len() > max_bytes {
        return ReportState::Oversized;
    }

    let Ok(report) = serde_json::from_slice::<Value>(&bytes) else {
        return ReportState::InvalidJson;
    };
    if !validate_report_schema(&report) {
        return ReportState::InvalidSchema;
    }
    ReportState::Valid(report)
}

fn validate_report_schema(report: &Value) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    const REQUIRED_FIELDS: &[&str] = &[
        "status",
        "summary",
        "base_commit",
        "changed_files",
        "checks",
        "unresolved",
        "requested_model",
        "requested_effort",
        "observed_model",
        "observed_effort",
    ];
    if object.len() != REQUIRED_FIELDS.len()
        || REQUIRED_FIELDS
            .iter()
            .any(|field| !object.contains_key(*field))
    {
        return false;
    }

    matches!(
        object.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "blocked" | "needs_decision")
    ) && bounded_report_string(object.get("summary"), 1200)
        && bounded_report_string(object.get("base_commit"), 200)
        && bounded_report_array(object.get("changed_files"), 128, 512)
        && bounded_report_array(object.get("checks"), 128, 512)
        && bounded_report_array(object.get("unresolved"), 128, 512)
        && bounded_report_string(object.get("requested_model"), 256)
        && bounded_report_string(object.get("requested_effort"), 256)
        && bounded_nullable_report_string(object.get("observed_model"), 256)
        && bounded_nullable_report_string(object.get("observed_effort"), 256)
}

fn bounded_report_string(value: Option<&Value>, max_chars: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| value.chars().count() <= max_chars)
}

fn bounded_nullable_report_string(value: Option<&Value>, max_chars: usize) -> bool {
    value.is_some_and(|value| value.is_null() || bounded_report_string(Some(value), max_chars))
}

fn bounded_report_array(value: Option<&Value>, max_items: usize, max_chars: usize) -> bool {
    value.is_some_and(|value| {
        value.as_array().is_some_and(|items| {
            items.len() <= max_items
                && items
                    .iter()
                    .all(|item| bounded_report_string(Some(item), max_chars))
        })
    })
}

fn serialize_parent_result(result: &DelegationResult) -> Result<String, serde_json::Error> {
    let full = serde_json::to_string(&result_to_json(result))?;
    if full.len() <= MAX_PARENT_RESULT_BYTES {
        return Ok(full);
    }

    let compact = json!({
        "status": "parent_result_oversized",
        "original_status": result.status.as_str(),
        "requested": {
            "model": result.requested_model,
            "reasoning_effort": result.requested_reasoning_effort,
        },
        "observed": {
            "model": result.observed_model,
            "reasoning_effort": result.observed_reasoning_effort,
        },
        "evidence": {
            "thread_id": result.evidence.thread_id,
            "usage": result.evidence.usage,
        },
        "exit_code": result.exit_code,
    });
    serde_json::to_string(&compact)
}

fn result_to_json(result: &DelegationResult) -> Value {
    normalized_to_json(&result.normalize())
}

fn normalized_to_json(result: &NormalizedResult) -> Value {
    json!({
        "status": result.status.as_str(),
        "requested": {
            "model": result.requested_model,
            "reasoning_effort": result.requested_variant,
        },
        "observed": {
            "model": result.observed_model,
            "reasoning_effort": result.observed_variant,
        },
        "report": result.report,
        "evidence": {
            "thread_id": result.evidence.thread_id,
            "usage": result.evidence.usage,
        },
        "exit_code": result.exit_code,
        "artifacts_truncated": result.artifacts_truncated,
        "artifacts": {
            "directory": path_string(&result.artifacts.directory),
            "events": path_string(&result.artifacts.events),
            "stderr": path_string(&result.artifacts.stderr),
            "report": path_string(&result.artifacts.report),
            "schema": path_string(&result.artifacts.schema),
        },
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_codex(root: &Path, mode: &str) -> PathBuf {
        let path = root.join(format!("fake-codex-{mode}"));
        let script = r##"#!/bin/sh
set -eu

last=
next=
for arg in "$@"; do
    if [ "$next" = "last" ]; then
        last=$arg
        next=
        continue
    fi
    case "$arg" in
        --output-last-message) next=last ;;
    esac
done

if IFS= read -r line; then
    printf 'stdin=unexpected:%s\n' "$line" >&2
else
    printf 'stdin=eof\n' >&2
fi
printf 'args=' >&2
for arg in "$@"; do
    printf '<%s>' "$arg" >&2
done
printf '\n' >&2

printf '%s\n' '{"type":"thread.started","thread_id":"thread-test"}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"total_tokens":13,"input_bytes":9999}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":11,"cached_input_tokens":4,"output_tokens":5,"reasoning_output_tokens":1,"total_tokens":20,"output_bytes":777}}'
printf 'stderr marker\n' >&2

case "$0" in
    *process_nonzero)
        printf '%s' '{"status":"failed","summary":"process failed"}' > "$last"
        exit 7
        ;;
    *missing)
        exit 0
        ;;
    *invalid)
        printf '%s' 'not-json' > "$last"
        exit 0
        ;;
    *invalid_schema)
        printf '%s' '{"status":"completed","summary":"ok"}' > "$last"
        exit 0
        ;;
    *oversized)
        printf '%s' '{"status":"completed","summary":"' > "$last"
        i=0
        while [ "$i" -lt 4100 ]; do
            printf 'x' >> "$last"
            i=$((i + 1))
        done
        printf '%s' '"}' >> "$last"
        exit 0
        ;;
    *)
        printf '%s' '{"status":"completed","summary":"ok","base_commit":"b75d1f7","changed_files":[],"checks":["cargo test"],"unresolved":[],"requested_model":"test-model","requested_effort":"high","observed_model":null,"observed_effort":null}' > "$last"
        exit 0
        ;;
esac
"##;
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn run_fake(root: &Path, mode: &str) -> Value {
        let binary = fake_codex(root, mode);
        let result = run_with_options(Options::new(
            "return a bounded report",
            "test-model",
            "high",
            binary,
        ))
        .unwrap();
        let value = result_to_json(&result);
        let stderr = fs::read_to_string(value["artifacts"]["stderr"].as_str().unwrap()).unwrap();
        assert!(stderr.contains("stdin=eof"));
        value
    }

    #[test]
    fn codex_command_uses_isolated_repo_managed_invocation() {
        let root = tempfile::tempdir().unwrap();
        let paths = ArtifactPaths {
            directory: root.path().join("artifacts"),
            events: root.path().join("events.jsonl"),
            stderr: root.path().join("stderr.log"),
            report: root.path().join("report.json"),
            schema: root.path().join("schema.json"),
        };
        let options = Options::new(
            "bounded prompt",
            "test-model",
            "high",
            PathBuf::from("codex"),
        );
        let cwd = fs::canonicalize(root.path()).unwrap();
        let command = build_codex_command(
            &options,
            &paths,
            &cwd,
            "model_reasoning_effort=\"high\"",
            Vec::<(OsString, OsString)>::new(),
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--cd" && pair[1] == cwd.to_string_lossy())
        );
        assert!(args.iter().any(|arg| arg == "--ignore-user-config"));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        assert!(
            args.iter()
                .any(|arg| arg == "shell_environment_policy.inherit=\"core\"")
        );
        assert!(!args.iter().any(|arg| arg == "danger-full-access"));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    #[test]
    fn codex_environment_filter_keeps_only_deliberate_allowlist() {
        let filtered = filtered_codex_environment([
            (OsString::from("HOME"), OsString::from("/home/test")),
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (
                OsString::from("TEMOTE_SHOULD_NOT_LEAK"),
                OsString::from("forbidden"),
            ),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("forbidden"),
            ),
        ]);
        let keys = filtered
            .into_iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(keys.contains(&"HOME".to_owned()));
        assert!(keys.contains(&"PATH".to_owned()));
        assert!(keys.contains(&"LC_ALL".to_owned()));
        assert!(keys.contains(&"OPENAI_API_KEY".to_owned()));
        assert!(!keys.contains(&"TEMOTE_SHOULD_NOT_LEAK".to_owned()));
        assert!(!keys.contains(&"AWS_SECRET_ACCESS_KEY".to_owned()));
    }

    #[test]
    fn codex_delegation_success_is_bounded_and_extracts_latest_usage_only() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "success");

        assert_eq!(value["status"], "success");
        assert_eq!(value["requested"]["model"], "test-model");
        assert_eq!(value["requested"]["reasoning_effort"], "high");
        assert_eq!(value["observed"]["model"], Value::Null);
        assert_eq!(value["observed"]["reasoning_effort"], Value::Null);
        assert_eq!(value["report"]["status"], "completed");
        assert_eq!(value["evidence"]["thread_id"], "thread-test");
        assert_eq!(value["evidence"]["usage"]["input_tokens"], 11);
        assert_eq!(value["evidence"]["usage"]["cached_input_tokens"], 4);
        assert_eq!(value["evidence"]["usage"]["output_tokens"], 5);
        assert_eq!(value["evidence"]["usage"]["reasoning_output_tokens"], 1);
        assert_eq!(value["evidence"]["usage"]["total_tokens"], 20);
        assert!(value["evidence"]["usage"].get("input_bytes").is_none());
        assert!(value["evidence"]["usage"].get("output_bytes").is_none());

        let stderr_path = value["artifacts"]["stderr"].as_str().unwrap();
        assert!(
            !serde_json::to_string(&value)
                .unwrap()
                .contains("stderr marker")
        );
        let stderr = fs::read_to_string(stderr_path).unwrap_or_default();
        assert!(stderr.contains("--ignore-user-config"));
        assert!(stderr.contains("--ephemeral"));
        assert!(stderr.contains("<--sandbox><workspace-write>"));
        assert!(stderr.contains("--cd"));
        assert!(stderr.contains("--json"));
        assert!(stderr.contains("--output-schema"));
        assert!(stderr.contains("--output-last-message"));
        assert!(stderr.contains("model_reasoning_effort=\"high\""));
        assert!(stderr.contains("shell_environment_policy.inherit=\"core\""));
        assert!(!stderr.contains("danger-full-access"));
        assert!(!stderr.contains("dangerously-bypass-approvals-and-sandbox"));
        assert_eq!(value["exit_code"], 0);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn codex_delegation_classifies_process_nonzero_exit() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "process_nonzero");
        assert_eq!(value["status"], "process_nonzero_exit");
        assert_eq!(value["exit_code"], 7);
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn codex_delegation_classifies_missing_report() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "missing");
        assert_eq!(value["status"], "missing_report");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn codex_delegation_classifies_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "invalid");
        assert_eq!(value["status"], "invalid_json");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn codex_delegation_classifies_schema_invalid_report() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "invalid_schema");
        assert_eq!(value["status"], "invalid_report_schema");
        assert_eq!(value["report"], Value::Null);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn codex_delegation_classifies_oversized_report() {
        let root = tempfile::tempdir().unwrap();
        let value = run_fake(root.path(), "oversized");
        assert_eq!(value["status"], "oversized_report");
        assert_eq!(value["report"], Value::Null);

        let report_path = value["artifacts"]["report"].as_str().unwrap();
        assert!(fs::metadata(report_path).unwrap().len() > DEFAULT_MAX_REPORT_BYTES as u64);
        fs::remove_dir_all(value["artifacts"]["directory"].as_str().unwrap()).unwrap();
    }

    #[test]
    fn parent_result_is_bounded_when_report_fills_the_report_budget() {
        let evidence = Evidence {
            thread_id: Some("thread-test".to_owned()),
            usage: Some(
                [("total_tokens".to_owned(), Value::from(20_u64))]
                    .into_iter()
                    .collect(),
            ),
            ..Evidence::default()
        };
        let report = json!({
            "status": "completed",
            "summary": "x".repeat(1200),
            "base_commit": "b75d1f7",
            "changed_files": vec!["x".repeat(512); 128],
            "checks": [],
            "unresolved": [],
            "requested_model": "test-model",
            "requested_effort": "high",
            "observed_model": null,
            "observed_effort": null,
        });
        let result = DelegationResult {
            backend: DelegationBackend::Codex,
            status: Status::Success,
            requested_model: "test-model".to_owned(),
            requested_reasoning_effort: "high".to_owned(),
            observed_model: None,
            observed_reasoning_effort: None,
            report: Some(report),
            evidence,
            exit_code: Some(0),
            artifacts_truncated: false,
            artifacts: ArtifactPaths {
                directory: PathBuf::from("/private/tmp/temote-codex"),
                events: PathBuf::from("/private/tmp/temote-codex/events.jsonl"),
                stderr: PathBuf::from("/private/tmp/temote-codex/stderr.log"),
                report: PathBuf::from("/private/tmp/temote-codex/report.json"),
                schema: PathBuf::from("/private/tmp/temote-codex/schema.json"),
            },
        };
        let encoded = serialize_parent_result(&result).unwrap();
        assert!(encoded.len() <= MAX_PARENT_RESULT_BYTES);
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["status"], "parent_result_oversized");
        assert_eq!(value["original_status"], "success");
        assert!(value.get("report").is_none());
    }

    #[test]
    fn delegation_backend_selection_is_explicit() {
        assert_eq!(
            DelegationBackend::parse("codex").unwrap(),
            DelegationBackend::Codex
        );
        assert_eq!(
            DelegationBackend::parse("opencode").unwrap(),
            DelegationBackend::OpenCode
        );
        assert_eq!(DelegationBackend::Codex.name(), "codex");
        assert_eq!(DelegationBackend::OpenCode.name(), "opencode");
        for value in ["Codex", "OPENCODE", "", "codex extra", "other"] {
            assert!(
                DelegationBackend::parse(value).is_err(),
                "backend accepted {value:?}"
            );
        }
    }

    fn fixture_result() -> DelegationResult {
        DelegationResult {
            backend: DelegationBackend::Codex,
            status: Status::Success,
            requested_model: "test-model".to_owned(),
            requested_reasoning_effort: "high".to_owned(),
            observed_model: Some("observed-model".to_owned()),
            observed_reasoning_effort: Some("medium".to_owned()),
            report: Some(json!({
                "status": "completed",
                "summary": "ok",
                "base_commit": "b75d1f7",
                "changed_files": [],
                "checks": ["cargo test"],
                "unresolved": [],
                "requested_model": "test-model",
                "requested_effort": "high",
                "observed_model": "observed-model",
                "observed_effort": "medium",
            })),
            evidence: Evidence {
                thread_id: Some("thread-test".to_owned()),
                usage: Some(
                    [("total_tokens".to_owned(), Value::from(20_u64))]
                        .into_iter()
                        .collect(),
                ),
                observed_model: Some("observed-model".to_owned()),
                observed_reasoning_effort: Some("medium".to_owned()),
            },
            exit_code: Some(0),
            artifacts_truncated: false,
            artifacts: ArtifactPaths {
                directory: PathBuf::from("/private/tmp/temote-codex"),
                events: PathBuf::from("/private/tmp/temote-codex/events.jsonl"),
                stderr: PathBuf::from("/private/tmp/temote-codex/stderr.log"),
                report: PathBuf::from("/private/tmp/temote-codex/report.json"),
                schema: PathBuf::from("/private/tmp/temote-codex/schema.json"),
            },
        }
    }

    #[test]
    fn normalized_result_keeps_requested_and_observed_distinct() {
        let result = fixture_result();
        let normalized = result.normalize();
        assert_eq!(normalized.backend, DelegationBackend::Codex);
        assert_eq!(normalized.requested_model, "test-model");
        assert_eq!(normalized.requested_variant, "high");
        assert_eq!(normalized.observed_model.as_deref(), Some("observed-model"));
        assert_eq!(normalized.observed_variant.as_deref(), Some("medium"));
        assert_eq!(normalized_to_json(&normalized), result_to_json(&result));
    }

    #[test]
    fn parent_result_json_shape_is_frozen_for_compatibility() {
        let result = fixture_result();
        assert_eq!(
            result_to_json(&result),
            json!({
                "status": "success",
                "requested": {
                    "model": "test-model",
                    "reasoning_effort": "high",
                },
                "observed": {
                    "model": "observed-model",
                    "reasoning_effort": "medium",
                },
                "report": {
                    "status": "completed",
                    "summary": "ok",
                    "base_commit": "b75d1f7",
                    "changed_files": [],
                    "checks": ["cargo test"],
                    "unresolved": [],
                    "requested_model": "test-model",
                    "requested_effort": "high",
                    "observed_model": "observed-model",
                    "observed_effort": "medium",
                },
                "evidence": {
                    "thread_id": "thread-test",
                    "usage": { "total_tokens": 20 },
                },
                "exit_code": 0,
                "artifacts_truncated": false,
                "artifacts": {
                    "directory": "/private/tmp/temote-codex",
                    "events": "/private/tmp/temote-codex/events.jsonl",
                    "stderr": "/private/tmp/temote-codex/stderr.log",
                    "report": "/private/tmp/temote-codex/report.json",
                    "schema": "/private/tmp/temote-codex/schema.json",
                },
            })
        );
    }

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
        let options = Options::new_opencode("task", "opencode-go/test-model", None, missing);
        let error = run_with_options(options).unwrap_err();
        assert!(
            error.contains("OpenCode backend unavailable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn opencode_prompt_and_variant_bounds_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let mut options = opencode_options(root.path(), "success", None);
        options.prompt = "x".repeat(MAX_OPENCODE_PROMPT_BYTES);
        assert!(validate_options(&options).is_err());

        let options = opencode_options(root.path(), "success", Some(""));
        assert!(validate_options(&options).is_err());

        let mut options = opencode_options(root.path(), "success", Some("high"));
        options.model = "x".repeat(MAX_ARGUMENT_BYTES + 1);
        assert!(validate_options(&options).is_err());
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

    fn diagnose_fake_with_timeouts(
        root: &Path,
        mode: &str,
        model: Option<&str>,
        timeouts: OpenCodeDiagnosticTimeouts,
    ) -> Value {
        let binary = fake_opencode_diagnostic(root, mode);
        let diagnostics = opencode_diagnostics(&binary, model, timeouts).unwrap();
        diagnostics_to_json(&diagnostics)
    }

    #[test]
    fn opencode_diagnostics_missing_binary_is_unavailable_without_false_ready() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing/opencode");
        let diagnostics = opencode_diagnostics(
            &missing,
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

        let diagnostics =
            opencode_diagnostics(&missing, None, OpenCodeDiagnosticTimeouts::default()).unwrap();
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
}
