#[cfg(test)]
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Map, Value, json};
use uuid::Uuid;

mod codex;
mod opencode;

pub(crate) const DEFAULT_MAX_REPORT_BYTES: usize = 4096;

const ARTIFACT_DIRECTORY_PREFIX: &str = "temote-codex-delegation-";
const MAX_PARENT_RESULT_BYTES: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
const REPORT_FIELDS: &[&str] = &[
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
const MAX_REPORT_SUMMARY_CHARS: usize = 1200;
const MAX_REPORT_COMMIT_CHARS: usize = 200;
const MAX_REPORT_ARGUMENT_CHARS: usize = 256;
const MAX_REPORT_ARRAY_ITEMS: usize = 128;
const MAX_REPORT_ARRAY_ITEM_CHARS: usize = 512;
const MAX_EVIDENCE_STRING_BYTES: usize = 256;
const MAX_EVIDENCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENT_LINE_BYTES: usize = 128 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_ARTIFACT_TEMP_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

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

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) backend: DelegationBackend,
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) session: Option<String>,
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
            session: None,
            codex_binary,
            opencode_binary: opencode::default_binary(),
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
            session: None,
            codex_binary: codex::default_binary(),
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
  temote-mcp delegate --backend opencode --model <provider/model> --session <ID> [--variant <VARIANT>] --prompt <PROMPT>
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
    let diagnostics = opencode::diagnose_default(
        requested_model.as_deref(),
        opencode::OpenCodeDiagnosticTimeouts::default(),
    )?;
    serde_json::to_string(&opencode::diagnostics_to_json(&diagnostics))
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
        session: None,
        codex_binary: codex::default_binary(),
        opencode_binary: opencode::default_binary(),
        timeout: None,
    })
}

fn parse_generic_args(args: &[String]) -> Result<Options, String> {
    let mut backend = None;
    let mut model = None;
    let mut reasoning_effort = None;
    let mut variant = None;
    let mut session = None;
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
            "--session" => session = Some(argument_value(args, &mut index, flag)?),
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
            if variant.is_some() || session.is_some() {
                return Err(format!(
                    "--variant and --session are only supported by the opencode backend\n\n{}",
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

    let opencode_binary = if backend == DelegationBackend::OpenCode {
        let override_value = opencode::bin_override_value().map_err(delegation_override_error)?;
        resolve_delegation_opencode_binary(backend, override_value.as_deref())
            .map_err(delegation_override_error)?
    } else {
        opencode::default_binary()
    };

    Ok(Options {
        backend,
        prompt,
        model,
        reasoning_effort,
        variant,
        session,
        codex_binary: codex::default_binary(),
        opencode_binary,
        timeout: None,
    })
}

fn delegation_override_error(error: opencode::OpenCodeExecutableError) -> String {
    format!("{}\n\n{}", error.delegation_message(), generic_usage())
}

fn resolve_delegation_opencode_binary(
    backend: DelegationBackend,
    override_value: Option<&str>,
) -> Result<PathBuf, opencode::OpenCodeExecutableError> {
    match backend {
        DelegationBackend::Codex => Ok(opencode::default_binary()),
        DelegationBackend::OpenCode => {
            opencode::resolve_opencode_executable(override_value, &opencode::default_binary())
                .map(opencode::ResolvedOpenCodeExecutable::into_path)
        }
    }
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
        DelegationBackend::Codex => codex::run_codex(options),
        DelegationBackend::OpenCode => opencode::run_opencode(options),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    Exited,
    TimedOut,
}

fn wait_with_bounded_artifacts_timeout(
    child: &mut Child,
    events_file: File,
    stderr_file: File,
    timeout: Duration,
) -> Result<(WaitOutcome, bool, bool), String> {
    wait_with_bounded_capture(
        child,
        events_file,
        stderr_file,
        Some(timeout),
        "OpenCode delegation",
        "JSON events",
    )
}

fn wait_with_bounded_artifacts(
    child: &mut Child,
    events_file: File,
    stderr_file: File,
) -> Result<bool, String> {
    let (_, stdout_truncated, stderr_truncated) = wait_with_bounded_capture(
        child,
        events_file,
        stderr_file,
        None,
        "Codex delegation",
        "JSONL",
    )?;
    Ok(stdout_truncated || stderr_truncated)
}

fn wait_with_bounded_capture(
    child: &mut Child,
    events_file: File,
    stderr_file: File,
    timeout: Option<Duration>,
    label: &str,
    stdout_kind: &str,
) -> Result<(WaitOutcome, bool, bool), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} stderr pipe is unavailable"))?;

    let (outcome, stdout_result, stderr_result) = std::thread::scope(|scope| {
        let stdout_handle = scope.spawn(|| capture_artifact(stdout, events_file));
        let stderr_handle = scope.spawn(|| capture_artifact(stderr, stderr_file));
        let outcome = wait_for_child(child, timeout, label);
        (outcome, stdout_handle.join(), stderr_handle.join())
    });

    let stdout_truncated = stdout_result
        .map_err(|_| format!("{label} stdout capture thread panicked"))?
        .map_err(|error| format!("could not capture {label} {stdout_kind}: {error}"))?;
    let stderr_truncated = stderr_result
        .map_err(|_| format!("{label} stderr capture thread panicked"))?
        .map_err(|error| format!("could not capture {label} stderr: {error}"))?;
    Ok((outcome?, stdout_truncated, stderr_truncated))
}

fn wait_for_child(
    child: &mut Child,
    timeout: Option<Duration>,
    label: &str,
) -> Result<WaitOutcome, String> {
    match timeout {
        None => {
            child
                .wait()
                .map_err(|error| format!("could not wait for {label}: {error}"))?;
            Ok(WaitOutcome::Exited)
        }
        Some(timeout) => {
            let deadline = Instant::now() + timeout;
            loop {
                if child
                    .try_wait()
                    .map_err(|error| format!("could not wait for {label}: {error}"))?
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
        }
    }
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

fn filtered_child_environment<I>(environment: I, allowlist: &[&str]) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    environment
        .into_iter()
        .filter(|(key, _)| child_environment_key_allowed(key, allowlist))
        .collect()
}

fn child_environment_key_allowed(key: &OsStr, allowlist: &[&str]) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    key.starts_with("LC_") || allowlist.contains(&key)
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
        DelegationBackend::Codex => codex::validate_options(options),
        DelegationBackend::OpenCode => opencode::validate_options(options),
    }
}

fn create_artifacts() -> Result<Artifacts, String> {
    let parent = artifact_temp_root();
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

fn artifact_temp_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_ARTIFACT_TEMP_ROOT.with(|value| value.borrow().clone()) {
        return root;
    }
    std::env::temp_dir()
}

#[cfg(test)]
pub(super) struct TestArtifactTempRootGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for TestArtifactTempRootGuard {
    fn drop(&mut self) {
        TEST_ARTIFACT_TEMP_ROOT.with(|value| {
            *value.borrow_mut() = self.previous.take();
        });
    }
}

#[cfg(test)]
pub(super) fn test_artifact_temp_root(root: &Path) -> TestArtifactTempRootGuard {
    let previous =
        TEST_ARTIFACT_TEMP_ROOT.with(|value| value.borrow_mut().replace(root.to_path_buf()));
    TestArtifactTempRootGuard { previous }
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
    if object.len() != REPORT_FIELDS.len()
        || REPORT_FIELDS
            .iter()
            .any(|field| !object.contains_key(*field))
    {
        return false;
    }

    matches!(
        object.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "blocked" | "needs_decision")
    ) && bounded_report_string(object.get("summary"), MAX_REPORT_SUMMARY_CHARS)
        && bounded_report_string(object.get("base_commit"), MAX_REPORT_COMMIT_CHARS)
        && bounded_report_array(
            object.get("changed_files"),
            MAX_REPORT_ARRAY_ITEMS,
            MAX_REPORT_ARRAY_ITEM_CHARS,
        )
        && bounded_report_array(
            object.get("checks"),
            MAX_REPORT_ARRAY_ITEMS,
            MAX_REPORT_ARRAY_ITEM_CHARS,
        )
        && bounded_report_array(
            object.get("unresolved"),
            MAX_REPORT_ARRAY_ITEMS,
            MAX_REPORT_ARRAY_ITEM_CHARS,
        )
        && bounded_report_string(object.get("requested_model"), MAX_REPORT_ARGUMENT_CHARS)
        && bounded_report_string(object.get("requested_effort"), MAX_REPORT_ARGUMENT_CHARS)
        && bounded_nullable_report_string(object.get("observed_model"), MAX_REPORT_ARGUMENT_CHARS)
        && bounded_nullable_report_string(object.get("observed_effort"), MAX_REPORT_ARGUMENT_CHARS)
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

    #[test]
    fn delegation_opencode_binary_resolution_keeps_codex_unaffected() {
        let binary =
            resolve_delegation_opencode_binary(DelegationBackend::Codex, Some("relative/opencode"))
                .unwrap();
        assert_eq!(binary, opencode::default_binary());
    }

    #[cfg(unix)]
    #[test]
    fn delegation_opencode_binary_resolution_uses_the_valid_override() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let override_path = root.path().join("opencode-override");
        fs::write(&override_path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&override_path, fs::Permissions::from_mode(0o700)).unwrap();
        let binary = resolve_delegation_opencode_binary(
            DelegationBackend::OpenCode,
            Some(override_path.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(binary, fs::canonicalize(&override_path).unwrap());
    }

    #[test]
    fn delegation_opencode_binary_resolution_rejects_invalid_overrides() {
        let error = resolve_delegation_opencode_binary(
            DelegationBackend::OpenCode,
            Some("relative/opencode"),
        )
        .unwrap_err();
        assert_eq!(error, opencode::OpenCodeExecutableError::NotAbsolute);
        assert!(!error.delegation_message().contains("relative/opencode"));
    }
}
