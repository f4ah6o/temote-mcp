use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Map, Value, json};
use uuid::Uuid;

pub(crate) const DEFAULT_MAX_REPORT_BYTES: usize = 4096;

const ARTIFACT_DIRECTORY_PREFIX: &str = "temote-codex-delegation-";
const MAX_PARENT_RESULT_BYTES: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;
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

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub(crate) prompt: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) codex_binary: PathBuf,
}

impl Options {
    #[cfg(test)]
    fn new(prompt: &str, model: &str, reasoning_effort: &str, codex_binary: PathBuf) -> Self {
        Self {
            prompt: prompt.to_owned(),
            model: model.to_owned(),
            reasoning_effort: reasoning_effort.to_owned(),
            codex_binary,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    Success,
    ProcessNonzeroExit,
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

    let prompt = match (prompt, prompt_file) {
        (Some(_), Some(_)) => {
            return Err("--prompt and --prompt-file cannot be combined".to_owned());
        }
        (Some(prompt), None) => prompt,
        (None, Some(path)) => read_prompt_file(Path::new(&path))?,
        (None, None) => return Err(format!("a prompt is required\n\n{}", usage())),
    };

    let model = model.ok_or_else(|| format!("--model is required\n\n{}", usage()))?;
    let reasoning_effort =
        reasoning_effort.ok_or_else(|| format!("--reasoning-effort is required\n\n{}", usage()))?;

    Ok(Options {
        prompt,
        model,
        reasoning_effort,
        codex_binary: PathBuf::from("codex"),
    })
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
    let artifacts = create_artifacts()?;
    let paths = artifacts.paths.clone();
    let reasoning_override = format!(
        "model_reasoning_effort={}",
        serde_json::to_string(&options.reasoning_effort)
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
            status: Status::ProcessNonzeroExit,
            requested_model: options.model,
            requested_reasoning_effort: options.reasoning_effort,
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
        status: report_status,
        requested_model: options.model,
        requested_reasoning_effort: options.reasoning_effort,
        observed_model: evidence.observed_model.clone(),
        observed_reasoning_effort: evidence.observed_reasoning_effort.clone(),
        report,
        evidence,
        exit_code,
        artifacts_truncated,
        artifacts: paths,
    })
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

fn validate_options(options: &Options) -> Result<(), String> {
    if options.model.is_empty()
        || options.model.len() > MAX_ARGUMENT_BYTES
        || options.model.contains('\0')
    {
        return Err(format!(
            "Codex model must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    if options.reasoning_effort.is_empty()
        || options.reasoning_effort.len() > MAX_ARGUMENT_BYTES
        || options.reasoning_effort.contains('\0')
    {
        return Err(format!(
            "Codex reasoning effort must be non-empty, NUL-free, and at most {MAX_ARGUMENT_BYTES} bytes"
        ));
    }
    if options.prompt.is_empty()
        || options.prompt.len() > MAX_PROMPT_BYTES
        || options.prompt.contains('\0')
    {
        return Err(format!(
            "Codex prompt must be non-empty, NUL-free, and at most {MAX_PROMPT_BYTES} bytes"
        ));
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
                    "could not create Codex delegation artifact directory: {error}"
                ));
            }
        }
    }
    Err("could not allocate a unique Codex delegation artifact directory".to_owned())
}

fn create_artifacts_in(directory: &Path) -> Result<Artifacts, String> {
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure Codex delegation artifacts: {error}"))?;

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
        .map_err(|error| format!("could not create private Codex delegation artifact: {error}"))
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
    json!({
        "status": result.status.as_str(),
        "requested": {
            "model": result.requested_model,
            "reasoning_effort": result.requested_reasoning_effort,
        },
        "observed": {
            "model": result.observed_model,
            "reasoning_effort": result.observed_reasoning_effort,
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
}
