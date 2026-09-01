use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{child_env, config::Session, sandbox};

pub const MAX_ITEM_GET_ITEMS: usize = 100;
const MAX_ITEM_QUERY_BYTES: usize = 4 * 1024;
const MAX_ITEM_GET_INPUT_BYTES: usize = 128 * 1024;
const MAX_SCOPE_BYTES: usize = 4 * 1024;
const COALESCING_WINDOW: Duration = Duration::from_millis(15);
const MAX_PENDING_READS_PER_SCOPE: usize = 128;
const MAX_SCOPE_BRIDGES: usize = 64;

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ItemScope {
    vault: Option<String>,
    account: Option<String>,
}

impl From<&ItemGetRequest> for ItemScope {
    fn from(request: &ItemGetRequest) -> Self {
        Self {
            vault: request.vault.clone(),
            account: request.account.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ScopeKey {
    session_id: String,
    scope: ItemScope,
}

type PendingResult = std::result::Result<Vec<Value>, String>;

struct PendingRead {
    items: Vec<String>,
    response: oneshot::Sender<PendingResult>,
}

struct ScopeEntry {
    sender: mpsc::Sender<PendingRead>,
    last_used: Instant,
}

#[derive(Default)]
struct CoalescerState {
    scopes: HashMap<ScopeKey, ScopeEntry>,
}

fn coalescer() -> &'static Mutex<CoalescerState> {
    static COALESCER: OnceLock<Mutex<CoalescerState>> = OnceLock::new();
    COALESCER.get_or_init(|| Mutex::new(CoalescerState::default()))
}

pub async fn item_get_coalesced(session: &Session, request: &ItemGetRequest) -> Result<Vec<Value>> {
    let sender = scope_sender(session, ItemScope::from(request));
    let (response, receiver) = oneshot::channel();
    sender
        .try_send(PendingRead {
            items: request.items.clone(),
            response,
        })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                anyhow::anyhow!("1Password read coalescing queue is full")
            }
            mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("1Password read coalescing worker is unavailable")
            }
        })?;
    receiver
        .await
        .context("1Password read coalescing worker stopped before responding")?
        .map_err(anyhow::Error::msg)
}

fn scope_sender(session: &Session, scope: ItemScope) -> mpsc::Sender<PendingRead> {
    let key = ScopeKey {
        session_id: session.id.clone(),
        scope: scope.clone(),
    };
    let now = Instant::now();
    let mut state = coalescer().lock().unwrap();
    state.scopes.retain(|_, entry| !entry.sender.is_closed());
    if let Some(entry) = state.scopes.get_mut(&key) {
        entry.last_used = now;
        return entry.sender.clone();
    }
    if state.scopes.len() >= MAX_SCOPE_BRIDGES
        && let Some(oldest) = state
            .scopes
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
    {
        state.scopes.remove(&oldest);
    }

    let (sender, receiver) = mpsc::channel(MAX_PENDING_READS_PER_SCOPE);
    state.scopes.insert(
        key,
        ScopeEntry {
            sender: sender.clone(),
            last_used: now,
        },
    );
    tokio::spawn(run_scope_worker(session.clone(), scope, receiver));
    sender
}

struct CollectedBatch {
    batch: Vec<PendingRead>,
    deferred: Option<PendingRead>,
    channel_closed: bool,
}

async fn run_scope_worker(
    session: Session,
    scope: ItemScope,
    mut receiver: mpsc::Receiver<PendingRead>,
) {
    let mut deferred = None;
    loop {
        let first = match deferred.take() {
            Some(request) => request,
            None => match receiver.recv().await {
                Some(request) => request,
                None => return,
            },
        };
        let collected = collect_batch(first, &mut receiver, COALESCING_WINDOW).await;
        deferred = collected.deferred;
        process_pending_batch(&session, &scope, collected.batch).await;
        if collected.channel_closed && deferred.is_none() {
            return;
        }
    }
}

async fn collect_batch(
    first: PendingRead,
    receiver: &mut mpsc::Receiver<PendingRead>,
    window: Duration,
) -> CollectedBatch {
    let mut total_items = first.items.len();
    let mut batch = vec![first];
    let mut deferred = None;
    let mut channel_closed = false;
    if total_items < MAX_ITEM_GET_ITEMS {
        let window = tokio::time::sleep(window);
        tokio::pin!(window);
        loop {
            tokio::select! {
                _ = &mut window => break,
                request = receiver.recv() => {
                    let Some(request) = request else {
                        channel_closed = true;
                        break;
                    };
                    if !batch_accepts(total_items, request.items.len()) {
                        deferred = Some(request);
                        break;
                    }
                    total_items += request.items.len();
                    batch.push(request);
                    if total_items == MAX_ITEM_GET_ITEMS {
                        break;
                    }
                }
            }
        }
    }
    CollectedBatch {
        batch,
        deferred,
        channel_closed,
    }
}

fn batch_accepts(current_items: usize, next_items: usize) -> bool {
    current_items
        .checked_add(next_items)
        .is_some_and(|total| total <= MAX_ITEM_GET_ITEMS)
}

async fn process_pending_batch(session: &Session, scope: &ItemScope, batch: Vec<PendingRead>) {
    let overviews = match list_overviews(session, scope).await {
        Ok(overviews) => overviews,
        Err(error) => {
            let message = format!("{error:#}");
            for pending in batch {
                let _ = pending.response.send(Err(message.clone()));
            }
            return;
        }
    };

    let request_items = batch
        .iter()
        .map(|pending| pending.items.clone())
        .collect::<Vec<_>>();
    let (selected, plans) = plan_batch(&overviews, &request_items);
    if selected.is_empty() {
        deliver_batch_results(
            batch,
            plans
                .into_iter()
                .map(|plan| match plan {
                    Ok(_) => Err("1Password read batch resolved no items".to_owned()),
                    Err(error) => Err(error),
                })
                .collect(),
        );
        return;
    }

    let fetched = match fetch_selected(session, scope, &selected).await {
        Ok(items) => items,
        Err(error) => {
            let message = format!("{error:#}");
            let results = plans
                .into_iter()
                .map(|plan| match plan {
                    Ok(_) => Err(message.clone()),
                    Err(error) => Err(error),
                })
                .collect();
            deliver_batch_results(batch, results);
            return;
        }
    };
    let results = project_batch_results(&fetched, plans);
    deliver_batch_results(batch, results);
}

fn plan_batch(
    overviews: &[Value],
    requests: &[Vec<String>],
) -> (Vec<Value>, Vec<std::result::Result<Vec<String>, String>>) {
    let mut selected = Vec::new();
    let mut selected_ids = HashSet::new();
    let mut plans = Vec::with_capacity(requests.len());
    for queries in requests {
        match resolve_overviews(overviews, queries) {
            Ok(items) => {
                let mut ids = Vec::with_capacity(items.len());
                for item in items {
                    let Some(id) = item.get("id").and_then(Value::as_str) else {
                        plans.push(Err("1Password item overview is missing id".to_owned()));
                        ids.clear();
                        break;
                    };
                    ids.push(id.to_owned());
                    if selected_ids.insert(id.to_owned()) {
                        selected.push(item);
                    }
                }
                if !ids.is_empty() {
                    plans.push(Ok(ids));
                }
            }
            Err(error) => plans.push(Err(format!("{error:#}"))),
        }
    }
    (selected, plans)
}

fn project_batch_results(
    fetched: &[Value],
    plans: Vec<std::result::Result<Vec<String>, String>>,
) -> Vec<PendingResult> {
    let mut by_id = HashMap::with_capacity(fetched.len());
    for item in fetched {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return plans
                .into_iter()
                .map(|plan| match plan {
                    Ok(_) => Err("1Password item result is missing id".to_owned()),
                    Err(error) => Err(error),
                })
                .collect();
        };
        if by_id.insert(id.to_owned(), item).is_some() {
            return plans
                .into_iter()
                .map(|plan| match plan {
                    Ok(_) => Err("1Password item result contains duplicate ids".to_owned()),
                    Err(error) => Err(error),
                })
                .collect();
        }
    }

    plans
        .into_iter()
        .map(|plan| {
            let ids = plan?;
            ids.into_iter()
                .map(|id| {
                    by_id.get(&id).map(|item| (*item).clone()).ok_or_else(|| {
                        format!("1Password item batch did not return resolved id {id}")
                    })
                })
                .collect()
        })
        .collect()
}

fn deliver_batch_results(batch: Vec<PendingRead>, results: Vec<PendingResult>) {
    debug_assert_eq!(batch.len(), results.len());
    for (pending, result) in batch.into_iter().zip(results) {
        let _ = pending.response.send(result);
    }
}

async fn list_overviews(session: &Session, scope: &ItemScope) -> Result<Vec<Value>> {
    let list = run_op(
        &list_command_for_scope(scope),
        session,
        None,
        "list candidate items",
    )
    .await?;
    serde_json::from_str(&list).context("1Password CLI returned invalid item-list JSON")
}

async fn fetch_selected(
    session: &Session,
    scope: &ItemScope,
    selected: &[Value],
) -> Result<Vec<Value>> {
    let selected_count = selected.len();
    let input = serde_json::to_vec(selected).context("failed to encode 1Password item batch")?;
    anyhow::ensure!(
        input.len() <= MAX_ITEM_GET_INPUT_BYTES,
        "resolved 1Password item batch exceeds {MAX_ITEM_GET_INPUT_BYTES} bytes"
    );
    let output = run_op(
        &get_command_for_scope(scope),
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

fn list_command_for_scope(scope: &ItemScope) -> Vec<String> {
    let mut command = vec![
        "op".to_owned(),
        "item".to_owned(),
        "list".to_owned(),
        "--format=json".to_owned(),
        "--cache=true".to_owned(),
    ];
    if let Some(vault) = &scope.vault {
        command.push("--vault".to_owned());
        command.push(vault.clone());
    }
    if let Some(account) = &scope.account {
        command.push("--account".to_owned());
        command.push(account.clone());
    }
    command
}

fn get_command_for_scope(scope: &ItemScope) -> Vec<String> {
    let mut command = vec![
        "op".to_owned(),
        "item".to_owned(),
        "get".to_owned(),
        "-".to_owned(),
        "--format=json".to_owned(),
        "--cache=true".to_owned(),
    ];
    if let Some(account) = &scope.account {
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
        let scope = ItemScope::from(&request);
        assert_eq!(
            list_command_for_scope(&scope),
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
            get_command_for_scope(&scope),
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
        for command in [
            list_command_for_scope(&scope),
            get_command_for_scope(&scope),
        ] {
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
    fn coalesced_batch_isolates_resolution_errors_and_fans_out_by_id() {
        let overviews = vec![
            json!({"id":"id-a","title":"Shared title"}),
            json!({"id":"id-b","title":"Shared title"}),
            json!({"id":"id-c","title":"Unique title"}),
        ];
        let requests = vec![
            vec!["id-a".to_owned(), "id-a".to_owned()],
            vec!["Shared title".to_owned()],
            vec!["id-b".to_owned(), "Unique title".to_owned()],
        ];
        let (selected, plans) = plan_batch(&overviews, &requests);
        assert_eq!(
            selected
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["id-a", "id-b", "id-c"]
        );
        assert!(plans[1].as_ref().unwrap_err().contains("ambiguous"));

        let fetched = vec![
            json!({"id":"id-c","title":"Unique title","fields":[]}),
            json!({"id":"id-a","title":"Shared title","fields":[]}),
            json!({"id":"id-b","title":"Shared title","fields":[]}),
        ];
        let results = project_batch_results(&fetched, plans);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().len(), 1);
        assert_eq!(results[0].as_ref().unwrap()[0]["id"], "id-a");
        assert!(results[1].as_ref().unwrap_err().contains("ambiguous"));
        assert_eq!(
            results[2]
                .as_ref()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["id-b", "id-c"]
        );
    }

    #[tokio::test]
    async fn microbatch_collects_nearby_calls_and_defers_overflow() {
        let (sender, mut receiver) = mpsc::channel(4);
        let (first_tx, _first_rx) = oneshot::channel();
        let (second_tx, _second_rx) = oneshot::channel();
        sender
            .try_send(PendingRead {
                items: vec!["b".to_owned()],
                response: second_tx,
            })
            .unwrap();
        let collected = collect_batch(
            PendingRead {
                items: vec!["a".to_owned()],
                response: first_tx,
            },
            &mut receiver,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(collected.batch.len(), 2);
        assert!(collected.deferred.is_none());

        let (first_tx, _first_rx) = oneshot::channel();
        let (overflow_tx, _overflow_rx) = oneshot::channel();
        sender
            .try_send(PendingRead {
                items: vec!["x".to_owned(); 50],
                response: overflow_tx,
            })
            .unwrap();
        let collected = collect_batch(
            PendingRead {
                items: vec!["y".to_owned(); 60],
                response: first_tx,
            },
            &mut receiver,
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(collected.batch.len(), 1);
        assert_eq!(collected.deferred.as_ref().unwrap().items.len(), 50);
    }

    #[tokio::test]
    async fn canceled_caller_does_not_break_other_fanout() {
        let (cancelled_tx, cancelled_rx) = oneshot::channel();
        drop(cancelled_rx);
        let (active_tx, active_rx) = oneshot::channel();
        let batch = vec![
            PendingRead {
                items: vec!["id-a".to_owned()],
                response: cancelled_tx,
            },
            PendingRead {
                items: vec!["id-b".to_owned()],
                response: active_tx,
            },
        ];
        deliver_batch_results(
            batch,
            vec![
                Ok(vec![json!({"id":"id-a"})]),
                Ok(vec![json!({"id":"id-b"})]),
            ],
        );
        let active = active_rx.await.unwrap().unwrap();
        assert_eq!(active, vec![json!({"id":"id-b"})]);
    }

    #[test]
    fn coalescing_scope_is_session_scoped_and_batch_size_is_bounded() {
        let scope = ItemScope {
            vault: Some("vault".to_owned()),
            account: Some("account".to_owned()),
        };
        let first = ScopeKey {
            session_id: "session-a".to_owned(),
            scope: scope.clone(),
        };
        let second = ScopeKey {
            session_id: "session-b".to_owned(),
            scope,
        };
        assert_ne!(first, second);
        assert!(batch_accepts(99, 1));
        assert!(!batch_accepts(100, 1));
        assert!(!batch_accepts(usize::MAX, 1));
    }

    #[test]
    fn generated_coalescing_batch_limit_matches_reference_model() -> noprop::TestResult {
        crate::test_support::run(0x4f50_434f_414c_4553, 512, |ctx| {
            let current = noprop::sample_usize_in(ctx, 0..=MAX_ITEM_GET_ITEMS + 16);
            let next = noprop::sample_usize_in(ctx, 0..=MAX_ITEM_GET_ITEMS + 16);
            let expected = current
                .checked_add(next)
                .is_some_and(|total| total <= MAX_ITEM_GET_ITEMS);
            assert_eq!(batch_accepts(current, next), expected);
            Ok(())
        })
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
