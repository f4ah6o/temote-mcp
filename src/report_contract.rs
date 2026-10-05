//! Shared report fields, bounds, compatibility profiles and native schema.

use serde_json::{Value, json};

pub(crate) const REPORT_FIELDS: &[&str] = &[
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
pub(crate) const MAX_REPORT_SUMMARY_CHARS: usize = 1200;
pub(crate) const MAX_REPORT_COMMIT_CHARS: usize = 200;
pub(crate) const MAX_REPORT_ARGUMENT_CHARS: usize = 256;
pub(crate) const MAX_DELEGATION_ARRAY_ITEMS: usize = 128;
pub(crate) const MAX_REPORT_ARRAY_ITEM_CHARS: usize = 512;
pub(crate) const MAX_TASK_REPORT_ARRAY_ITEMS: usize = 64;
pub(crate) const MAX_TASK_REPORT_BYTES: usize = 8 * 1024;

pub(crate) const DELEGATION_OUTPUT_SCHEMA: &str = r#"{
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportProfile {
    Delegation,
    TaskReport,
}

pub(crate) fn validate(report: &Value, profile: ReportProfile) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    let valid_status = matches!(
        object.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "blocked" | "needs_decision")
    );
    if !valid_status {
        return false;
    }
    match profile {
        ReportProfile::Delegation => {
            object.len() == REPORT_FIELDS.len()
                && REPORT_FIELDS
                    .iter()
                    .all(|field| object.contains_key(*field))
                && bounded_string(object.get("summary"), MAX_REPORT_SUMMARY_CHARS)
                && bounded_string(object.get("base_commit"), MAX_REPORT_COMMIT_CHARS)
                && ["changed_files", "checks", "unresolved"]
                    .iter()
                    .all(|field| {
                        bounded_array(
                            object.get(*field),
                            MAX_DELEGATION_ARRAY_ITEMS,
                            Some(MAX_REPORT_ARRAY_ITEM_CHARS),
                        )
                    })
                && ["requested_model", "requested_effort"]
                    .iter()
                    .all(|field| bounded_string(object.get(*field), MAX_REPORT_ARGUMENT_CHARS))
                && ["observed_model", "observed_effort"].iter().all(|field| {
                    object.get(*field).is_some_and(|value| {
                        value.is_null() || bounded_string(Some(value), MAX_REPORT_ARGUMENT_CHARS)
                    })
                })
        }
        ReportProfile::TaskReport => {
            bounded_string(object.get("summary"), MAX_REPORT_SUMMARY_CHARS)
                && ["changed_files", "checks", "unresolved"]
                    .iter()
                    .all(|field| {
                        object.get(*field).is_none_or(|value| {
                            bounded_array(Some(value), MAX_TASK_REPORT_ARRAY_ITEMS, None)
                        })
                    })
                && serde_json::to_vec(report)
                    .is_ok_and(|bytes| bytes.len() <= MAX_TASK_REPORT_BYTES)
        }
    }
}

fn bounded_string(value: Option<&Value>, max_chars: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| text.chars().count() <= max_chars)
}

fn bounded_array(value: Option<&Value>, max_items: usize, max_chars: Option<usize>) -> bool {
    value.and_then(Value::as_array).is_some_and(|items| {
        items.len() <= max_items
            && items.iter().all(|item| {
                max_chars.map_or_else(
                    || item.is_string(),
                    |limit| bounded_string(Some(item), limit),
                )
            })
    })
}

/// This is byte-identical to the pre-refactor Devin Cloud wire schema.
pub(crate) fn report_json_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["completed", "failed", "blocked", "needs_decision"]},
            "summary": {"type": "string", "maxLength": MAX_REPORT_SUMMARY_CHARS},
            "base_commit": {"type": "string"},
            "changed_files": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_TASK_REPORT_ARRAY_ITEMS},
            "checks": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_TASK_REPORT_ARRAY_ITEMS},
            "unresolved": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_TASK_REPORT_ARRAY_ITEMS},
        },
        "required": ["status", "summary"],
        "additionalProperties": false,
    })
}

/// Validate a native payload against the Cloud-compatible wire schema without
/// changing the historical, more permissive TaskReport decoding profile.
pub(crate) fn validate_native_wire(report: &Value) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    object.keys().all(|key| {
        [
            "status",
            "summary",
            "base_commit",
            "changed_files",
            "checks",
            "unresolved",
        ]
        .contains(&key.as_str())
    }) && object.get("base_commit").is_none_or(Value::is_string)
        && validate(report, ReportProfile::TaskReport)
}

/// Codex's final-message constraint requires the six task-report fields.
/// Cloud keeps its historical, lenient wire schema above.
pub(crate) fn codex_report_json_schema() -> Value {
    let mut schema = report_json_schema();
    schema["required"] = json!([
        "status",
        "summary",
        "base_commit",
        "changed_files",
        "checks",
        "unresolved"
    ]);
    schema["properties"]["base_commit"]["maxLength"] = json!(MAX_REPORT_COMMIT_CHARS);
    for field in ["changed_files", "checks", "unresolved"] {
        schema["properties"][field]["items"]["maxLength"] = json!(MAX_REPORT_ARRAY_ITEM_CHARS);
    }
    schema
}

pub(crate) fn validate_codex_native(report: &Value) -> bool {
    let Some(object) = report.as_object() else {
        return false;
    };
    object.len() == 6
        && [
            "status",
            "summary",
            "base_commit",
            "changed_files",
            "checks",
            "unresolved",
        ]
        .iter()
        .all(|field| object.contains_key(*field))
        && validate(report, ReportProfile::TaskReport)
        && bounded_string(object.get("base_commit"), MAX_REPORT_COMMIT_CHARS)
        && ["changed_files", "checks", "unresolved"]
            .iter()
            .all(|field| {
                bounded_array(
                    object.get(*field),
                    MAX_TASK_REPORT_ARRAY_ITEMS,
                    Some(MAX_REPORT_ARRAY_ITEM_CHARS),
                )
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_preserve_the_intentional_difference() {
        let minimal = json!({"status":"completed", "summary":"ok"});
        assert!(validate(&minimal, ReportProfile::TaskReport));
        assert!(!validate(&minimal, ReportProfile::Delegation));
        let strict = json!({"status":"completed","summary":"ok","base_commit":"",
            "changed_files":[],"checks":[],"unresolved":[],"requested_model":"m",
            "requested_effort":"e","observed_model":null,"observed_effort":null});
        assert!(validate(&strict, ReportProfile::Delegation));
    }

    #[test]
    fn cloud_schema_snapshot() {
        let expected = r#"{"additionalProperties":false,"properties":{"base_commit":{"type":"string"},"changed_files":{"items":{"type":"string"},"maxItems":64,"type":"array"},"checks":{"items":{"type":"string"},"maxItems":64,"type":"array"},"status":{"enum":["completed","failed","blocked","needs_decision"],"type":"string"},"summary":{"maxLength":1200,"type":"string"},"unresolved":{"items":{"type":"string"},"maxItems":64,"type":"array"}},"required":["status","summary"],"type":"object"}"#;
        assert_eq!(report_json_schema().to_string(), expected);
    }

    #[test]
    fn native_wire_rejects_fields_the_compatibility_profile_keeps() {
        let compatible = json!({"status":"completed","summary":"ok","extra":true});
        assert!(validate(&compatible, ReportProfile::TaskReport));
        assert!(!validate_native_wire(&compatible));
        assert!(validate_native_wire(
            &json!({"status":"completed","summary":"ok"})
        ));
    }
}
