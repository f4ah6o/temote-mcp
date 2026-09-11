use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde_json::{Map, Value};

use super::*;

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

pub(super) fn default_binary() -> PathBuf {
    PathBuf::from("codex")
}

pub(super) fn run_codex(options: Options) -> Result<DelegationResult, String> {
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

pub(super) fn validate_options(options: &Options) -> Result<(), String> {
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
    Ok(())
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
    super::filtered_child_environment(environment, CODEX_CHILD_ENV_ALLOWLIST)
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

#[cfg(test)]
mod tests {
    use super::super::*;
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
}
