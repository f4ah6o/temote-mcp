use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::{child_env, config::Session, sandbox};

pub const MAX_ITEM_GET_ITEMS: usize = 100;
const MAX_ITEM_QUERY_BYTES: usize = 4 * 1024;
const MAX_ITEM_GET_INPUT_BYTES: usize = 128 * 1024;
const MAX_SCOPE_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemGetRequest {
    pub items: Vec<String>,
    pub vault: Option<String>,
    pub account: Option<String>,
}

impl ItemGetRequest {
    pub fn new(items: Vec<String>, vault: Option<String>, account: Option<String>) -> Result<Self> {
        validate_items(&items)?;
        validate_scope(vault.as_deref(), "vault")?;
        validate_scope(account.as_deref(), "account")?;
        Ok(Self {
            items,
            vault,
            account,
        })
    }

    pub fn approval_summary(&self) -> String {
        format!(
            "items: {}\nvault: {}\naccount: {}",
            self.items.len(),
            if self.vault.is_some() {
                "configured"
            } else {
                "default"
            },
            if self.account.is_some() {
                "configured"
            } else {
                "default"
            }
        )
    }
}

pub async fn item_get(session: &Session, request: &ItemGetRequest) -> Result<Vec<Value>> {
    let list = run_op(
        &list_command(request),
        session,
        None,
        "list candidate items",
    )
    .await?;
    let overviews: Vec<Value> =
        serde_json::from_str(&list).context("1Password CLI returned invalid item-list JSON")?;
    let selected = resolve_overviews(&overviews, &request.items)?;
    let selected_count = selected.len();
    let input = serde_json::to_vec(&selected).context("failed to encode 1Password item batch")?;
    anyhow::ensure!(
        input.len() <= MAX_ITEM_GET_INPUT_BYTES,
        "resolved 1Password item batch exceeds {MAX_ITEM_GET_INPUT_BYTES} bytes"
    );

    let output = run_op(
        &get_command(request),
        session,
        Some(&input),
        "get item batch",
    )
    .await?;
    let items = parse_json_sequence(&output)?;
    anyhow::ensure!(
        items.len() == selected_count,
        "1Password CLI returned {} item(s) for a {} item batch",
        items.len(),
        selected_count
    );
    Ok(items)
}

async fn run_op(
    command: &[String],
    session: &Session,
    stdin: Option<&[u8]>,
    operation: &'static str,
) -> Result<String> {
    let output = sandbox::run_unrestricted_with_env(
        command,
        &session.cwd,
        stdin,
        &HashMap::new(),
        child_env::SENSITIVE_ENV_NAMES,
    )
    .await
    .with_context(|| format!("failed to run 1Password CLI to {operation}"))?;
    anyhow::ensure!(
        !output.truncated,
        "1Password CLI output exceeded {} bytes while trying to {operation}",
        sandbox::MAX_COMMAND_OUTPUT_BYTES
    );
    anyhow::ensure!(
        output.status == 0,
        "1Password CLI failed to {operation}: {}",
        output.stderr.trim()
    );
    Ok(output.stdout)
}

fn list_command(request: &ItemGetRequest) -> Vec<String> {
    let mut command = vec![
        "op".to_owned(),
        "item".to_owned(),
        "list".to_owned(),
        "--format=json".to_owned(),
        "--cache=true".to_owned(),
    ];
    if let Some(vault) = &request.vault {
        command.push("--vault".to_owned());
        command.push(vault.clone());
    }
    if let Some(account) = &request.account {
        command.push("--account".to_owned());
        command.push(account.clone());
    }
    command
}

fn get_command(request: &ItemGetRequest) -> Vec<String> {
    let mut command = vec![
        "op".to_owned(),
        "item".to_owned(),
        "get".to_owned(),
        "-".to_owned(),
        "--format=json".to_owned(),
        "--cache=true".to_owned(),
    ];
    if let Some(account) = &request.account {
        command.push("--account".to_owned());
        command.push(account.clone());
    }
    command
}

fn validate_items(items: &[String]) -> Result<()> {
    anyhow::ensure!(!items.is_empty(), "items must not be empty");
    anyhow::ensure!(
        items.len() <= MAX_ITEM_GET_ITEMS,
        "items must contain at most {MAX_ITEM_GET_ITEMS} entries"
    );
    let mut total = 0usize;
    for item in items {
        anyhow::ensure!(!item.is_empty(), "item queries must not be empty");
        anyhow::ensure!(
            item.len() <= MAX_ITEM_QUERY_BYTES,
            "item query exceeds {MAX_ITEM_QUERY_BYTES} bytes"
        );
        anyhow::ensure!(
            !item.chars().any(char::is_control),
            "item queries must not contain control characters"
        );
        total = total
            .checked_add(item.len())
            .context("1Password item query size overflow")?;
        anyhow::ensure!(
            total <= MAX_ITEM_GET_INPUT_BYTES,
            "item queries exceed {MAX_ITEM_GET_INPUT_BYTES} bytes in total"
        );
    }
    Ok(())
}

fn validate_scope(value: Option<&str>, name: &str) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    anyhow::ensure!(
        value.len() <= MAX_SCOPE_BYTES,
        "{name} exceeds {MAX_SCOPE_BYTES} bytes"
    );
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "{name} must not contain control characters"
    );
    Ok(())
}

fn resolve_overviews(overviews: &[Value], queries: &[String]) -> Result<Vec<Value>> {
    let mut selected = Vec::with_capacity(queries.len());
    let mut selected_ids = HashSet::new();

    for query in queries {
        let id_matches = overviews
            .iter()
            .filter(|item| item.get("id").and_then(Value::as_str) == Some(query.as_str()))
            .collect::<Vec<_>>();
        let matches = if id_matches.is_empty() {
            overviews
                .iter()
                .filter(|item| item.get("title").and_then(Value::as_str) == Some(query.as_str()))
                .collect::<Vec<_>>()
        } else {
            id_matches
        };
        anyhow::ensure!(
            !matches.is_empty(),
            "no 1Password item matched query {query:?}; use an exact item ID or title"
        );
        anyhow::ensure!(
            matches.len() == 1,
            "1Password item query {query:?} is ambiguous; specify a vault or item ID"
        );
        let item = matches[0];
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .context("1Password item overview is missing id")?;
        if selected_ids.insert(id.to_owned()) {
            selected.push(item.clone());
        }
    }
    Ok(selected)
}

fn parse_json_sequence(input: &str) -> Result<Vec<Value>> {
    serde_json::Deserializer::from_str(input)
        .into_iter::<Value>()
        .collect::<serde_json::Result<Vec<_>>>()
        .context("1Password CLI returned invalid item JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(items: &[&str]) -> ItemGetRequest {
        ItemGetRequest::new(
            items.iter().map(|item| (*item).to_owned()).collect(),
            Some("Private Vault".to_owned()),
            Some("work".to_owned()),
        )
        .unwrap()
    }

    #[test]
    fn item_get_commands_are_read_only_and_batch_through_stdin() {
        let request = request(&["item-a", "item-b"]);
        assert_eq!(
            list_command(&request),
            vec![
                "op",
                "item",
                "list",
                "--format=json",
                "--cache=true",
                "--vault",
                "Private Vault",
                "--account",
                "work",
            ]
        );
        assert_eq!(
            get_command(&request),
            vec![
                "op",
                "item",
                "get",
                "-",
                "--format=json",
                "--cache=true",
                "--account",
                "work",
            ]
        );
        for command in [list_command(&request), get_command(&request)] {
            assert!(!command.iter().any(|part| matches!(
                part.as_str(),
                "create" | "edit" | "delete" | "archive" | "move"
            )));
        }
    }

    #[test]
    fn approval_summary_never_contains_item_or_scope_values() {
        let request = request(&["very-secret-item-name", "second-secret-name"]);
        let summary = request.approval_summary();
        assert_eq!(summary, "items: 2\nvault: configured\naccount: configured");
        for forbidden in [
            "very-secret-item-name",
            "second-secret-name",
            "Private Vault",
            "work",
        ] {
            assert!(!summary.contains(forbidden));
        }
    }

    #[test]
    fn overview_resolution_prefers_ids_deduplicates_and_rejects_ambiguity() {
        let overviews = vec![
            json!({"id":"id-a","title":"Shared title"}),
            json!({"id":"id-b","title":"Shared title"}),
            json!({"id":"id-c","title":"Unique title"}),
        ];
        let resolved = resolve_overviews(
            &overviews,
            &[
                "id-a".to_owned(),
                "Unique title".to_owned(),
                "id-a".to_owned(),
            ],
        )
        .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0]["id"], "id-a");
        assert_eq!(resolved[1]["id"], "id-c");

        assert!(
            resolve_overviews(&overviews, &["Shared title".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            resolve_overviews(&overviews, &["missing".to_owned()])
                .unwrap_err()
                .to_string()
                .contains("no 1Password item matched")
        );
    }

    #[test]
    fn parses_concatenated_pretty_json_objects_from_op_batch_output() {
        let values = parse_json_sequence("{\n  \"id\": \"a\"\n}\n{\n  \"id\": \"b\"\n}\n").unwrap();
        assert_eq!(values, vec![json!({"id":"a"}), json!({"id":"b"})]);
    }

    #[test]
    fn validation_bounds_queries_and_scopes_before_op_execution() {
        assert!(ItemGetRequest::new(Vec::new(), None, None).is_err());
        assert!(
            ItemGetRequest::new(vec!["x".to_owned(); MAX_ITEM_GET_ITEMS + 1], None, None).is_err()
        );
        assert!(ItemGetRequest::new(vec!["bad\nquery".to_owned()], None, None).is_err());
        assert!(ItemGetRequest::new(vec!["ok".to_owned()], Some(String::new()), None).is_err());
        assert!(
            ItemGetRequest::new(vec!["ok".to_owned()], None, Some("bad\taccount".to_owned()))
                .is_err()
        );
    }

    #[test]
    fn generated_item_query_validation_matches_reference_model() -> noprop::TestResult {
        crate::test_support::run(0x4f50_4954_454d_4745, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=MAX_ITEM_GET_ITEMS + 4);
            let mut items = (0..count)
                .map(|_| crate::test_support::safe_component(ctx))
                .collect::<Vec<_>>();
            if !items.is_empty() && noprop::sample_bool(ctx) {
                let index = noprop::sample_usize_in(ctx, 0..items.len());
                items[index].push('\n');
            }
            let total = items
                .iter()
                .try_fold(0usize, |sum, item| sum.checked_add(item.len()));
            let expected = !items.is_empty()
                && items.len() <= MAX_ITEM_GET_ITEMS
                && items.iter().all(|item| {
                    !item.is_empty()
                        && item.len() <= MAX_ITEM_QUERY_BYTES
                        && !item.chars().any(char::is_control)
                })
                && total.is_some_and(|bytes| bytes <= MAX_ITEM_GET_INPUT_BYTES);
            assert_eq!(validate_items(&items).is_ok(), expected);
            Ok(())
        })
    }
}
