//! The explicit local frontend for retained delegated tasks.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

use crate::local_tasks::TaskAction;

pub(crate) struct TaskInvocation {
    pub session_id: String,
    pub action: TaskAction,
}

pub(crate) const USAGE: &str = "Usage: temote task <start|list|get|control> --local --session-id <SESSION_ID> [options]\n\
  start:   --backend <codex|opencode|devin_acp|devin_cloud> --operation-id <UUID> --task <TEXT> [--model <MODEL>] [--effort <EFFORT>] [--continue-from-task <UUID> (Codex)] [--agent <AGENT>] [--variant <VARIANT>] [--workspace-requirement managed_commands] [--cloud]\n\
  list:    [--limit <1..128>]\n\
  get:     --backend <BACKEND> --task-id <UUID> [--after-revision <N>] [--wait-ms <0..30000>]\n\
  control: --backend <BACKEND> --task-id <UUID> --operation-id <UUID> --action <steer|resume|interrupt> [--input <TEXT>]\n\
Other start selectors: --title, --devin-mode, --swe-tier, --repo (repeatable), --max-acu-limit. The local control packet is limited to 64 KiB.\n\
The caller supplies a fresh operation ID for each new start/control action. Retry an uncertain response only with the same ID and identical request; use list/get to reconcile. Evidence detail is available through MCP evidence_read. Legacy `delegate` remains a separate one-shot compatibility command.\n";

pub(crate) fn parse(raw: &[String]) -> Result<TaskInvocation, String> {
    parse_inner(raw).map_err(|error| format!("{error:#}\n{USAGE}"))
}

fn parse_inner(raw: &[String]) -> Result<TaskInvocation> {
    let Some(operation) = raw.first() else {
        bail!("task operation is required");
    };
    ensure!(
        matches!(operation.as_str(), "start" | "list" | "get" | "control"),
        "unknown task operation"
    );
    let mut options = BTreeMap::new();
    let mut local = false;
    let mut index = 1;
    while index < raw.len() {
        let key = raw[index].as_str();
        if key == "--local" {
            ensure!(!local, "duplicate --local");
            local = true;
            index += 1;
            continue;
        }
        ensure!(key.starts_with("--"), "unexpected positional task argument");
        let key = key.trim_start_matches("--").replace('-', "_");
        ensure!(
            key != "schema_version" && key != "operation",
            "reserved task option"
        );
        if key == "repo" {
            let value = raw.get(index + 1).context("missing --repo value")?;
            let repos = options
                .entry("repos".to_owned())
                .or_insert_with(|| json!([]));
            repos.as_array_mut().expect("repo list").push(json!(value));
            index += 2;
            continue;
        }
        let key = if key == "session" {
            "session_id".to_owned()
        } else {
            key
        };
        ensure!(
            !options.contains_key(&key),
            "duplicate --{}",
            key.replace('_', "-")
        );
        if key == "cloud" {
            options.insert(key, json!(true));
            index += 1;
        } else {
            let value = raw.get(index + 1).context("missing task option value")?;
            let parsed = match key.as_str() {
                "limit" | "after_revision" | "wait_ms" | "max_acu_limit" => json!(
                    value
                        .parse::<u64>()
                        .context("task option must be an unsigned integer")?
                ),
                "session_id"
                | "backend"
                | "operation_id"
                | "task"
                | "model"
                | "effort"
                | "agent"
                | "variant"
                | "workspace_requirement"
                | "title"
                | "devin_mode"
                | "swe_tier"
                | "task_id"
                | "action"
                | "input" => json!(value),
                "continue_from_task" => {
                    let task_id = uuid::Uuid::parse_str(value)
                        .context("--continue-from-task must be a UUID")?;
                    json!(task_id)
                }
                _ => bail!("unknown task option --{}", key.replace('_', "-")),
            };
            options.insert(key, parsed);
            index += 2;
        }
    }
    ensure!(local, "task operations require --local");
    ensure!(
        options.contains_key("session_id"),
        "--session-id is required"
    );
    let mut object: Map<String, Value> = options.into_iter().collect();
    if let Some(task_id) = object.remove("continue_from_task") {
        ensure!(operation == "start", "--continue-from-task requires start");
        ensure!(
            object.get("backend").and_then(Value::as_str) == Some("codex"),
            "--continue-from-task requires the Codex backend"
        );
        object.insert(
            "continuation".to_owned(),
            json!({"type":"previous_task","task_id":task_id}),
        );
    }
    let session_id = object
        .remove("session_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .context("invalid session_id")?;
    object.insert("operation".to_owned(), json!(operation));
    ensure!(
        !session_id.is_empty() && session_id.len() <= 128,
        "invalid session_id"
    );
    let action: TaskAction = serde_json::from_value(Value::Object(object))
        .map_err(|_| anyhow::anyhow!("invalid typed local task options"))?;
    Ok(TaskInvocation { session_id, action })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn local_cli_is_typed_and_explicit() {
        assert!(parse_inner(&args(&["list", "--local", "--session-id", "s"])).is_ok());
        assert!(parse_inner(&args(&["list", "--session-id", "s"])).is_err());
        assert!(
            parse_inner(&args(&[
                "start",
                "--local",
                "--session-id",
                "s",
                "--backend",
                "codex",
                "--operation-id",
                "00000000-0000-0000-0000-000000000000",
                "--task",
                "x",
                "--model",
                "m",
                "--effort",
                "high",
                "--argv",
                "sh"
            ]))
            .is_err()
        );
    }

    #[test]
    fn continuation_flag_is_typed_and_codex_only() {
        let id = "0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa";
        let start = |backend: &str, source: &str| {
            args(&[
                "start",
                "--local",
                "--session-id",
                "s",
                "--backend",
                backend,
                "--operation-id",
                id,
                "--task",
                "follow up",
                "--model",
                "gpt",
                "--effort",
                "high",
                "--continue-from-task",
                source,
            ])
        };
        let parsed = parse_inner(&start("codex", id)).unwrap();
        let TaskAction::Start { continuation, .. } = parsed.action else {
            panic!("start expected")
        };
        assert_eq!(
            continuation,
            Some(crate::codex_app_server::CodexContinuation::PreviousTask {
                task_id: uuid::Uuid::parse_str(id).unwrap()
            })
        );
        assert!(parse_inner(&start("codex", "invalid")).is_err());
        assert!(parse_inner(&start("devin_acp", id)).is_err());
    }
}
