//! Bounded waiting over read-only backend reconciliation. This layer never
//! starts or controls a task, and budgets both probe time and probe frequency.
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep_until, timeout_at};

pub(super) async fn for_update<F, Fut>(
    task_id: &str,
    after_revision: Option<u64>,
    wait_ms: u64,
    mut probe: F,
) -> Result<Value>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let mut last_revision = None;
    let mut interval = Duration::from_millis(500);
    loop {
        let view = match timeout_at(deadline, probe()).await {
            Ok(result) => result?,
            Err(_) => break,
        };
        let revision = view.get("revision").and_then(Value::as_u64);
        let status = view.get("status").and_then(Value::as_str);
        // Terminal, input-wait, and uncertain states need caller attention even
        // if its cursor already names this revision. Return scoped evidence as
        // supplied by the backend rather than flattening execution outcomes.
        if after_revision.is_none()
            || revision != after_revision
            || !matches!(status, Some("running" | "accepted" | "not_modified"))
            || view
                .get("recovery_state")
                .is_some_and(|state| !state.is_null())
            || view.get("reconciliation_required").and_then(Value::as_bool) == Some(true)
            || view.get("reconciliation_deferred").and_then(Value::as_bool) == Some(true)
        {
            return Ok(view);
        }
        last_revision = revision;
        let next_probe = (Instant::now() + interval).min(deadline);
        sleep_until(next_probe).await;
        if Instant::now() >= deadline {
            break;
        }
        interval = (interval * 2).min(Duration::from_secs(2));
    }
    let mut result = json!({
        "task_id": task_id,
        "status": if last_revision.is_some() { "not_modified" } else { "wait_timeout" },
        "wait": { "outcome": "timeout" },
    });
    if let Some(revision) = last_revision {
        result["revision"] = json!(revision);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn unchanged_deadline_is_compact_and_does_not_churn_revision() {
        let probes = AtomicUsize::new(0);
        let result = for_update("t", Some(7), 20, || {
            probes.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(
                json!({"status":"running", "revision":7, "evidence":null}),
            ))
        })
        .await
        .unwrap();
        assert_eq!(
            result,
            json!({"task_id":"t", "status":"not_modified", "revision":7, "wait":{"outcome":"timeout"}})
        );
        assert_eq!(probes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn terminal_and_input_states_preserve_evidence_even_at_same_revision() {
        for status in [
            "completed",
            "failed",
            "interrupted",
            "waiting_approval",
            "waiting_input",
            "unknown",
        ] {
            let expected =
                json!({"status":status, "revision":7, "evidence":{"evidence_id":"scoped"}});
            let result = for_update("t", Some(7), 30_000, || {
                std::future::ready(Ok(expected.clone()))
            })
            .await
            .unwrap();
            assert_eq!(result, expected);
        }
    }

    #[tokio::test]
    async fn slow_probe_respects_deadline_without_claiming_an_observed_revision() {
        let result = for_update("t", Some(7), 20, || async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(json!({"status":"completed", "revision":8}))
        })
        .await
        .unwrap();
        assert_eq!(result["status"], "wait_timeout");
        assert!(result.get("revision").is_none());
    }

    #[tokio::test]
    async fn owner_fence_and_probe_errors_propagate_without_replaying_a_mutation() {
        let result = for_update("t", Some(7), 30_000, || {
            std::future::ready(Err(anyhow::anyhow!("owner changed")))
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "owner changed");
    }

    #[tokio::test]
    async fn uncertainty_returns_without_waiting_for_another_owner() {
        for key in ["reconciliation_required", "reconciliation_deferred"] {
            let mut view = json!({"status":"running", "revision":7});
            view[key] = json!(true);
            let result = for_update("t", Some(7), 30_000, || {
                std::future::ready(Ok(view.clone()))
            })
            .await
            .unwrap();
            assert_eq!(result, view);
        }
    }

    #[tokio::test]
    async fn wait_returns_the_next_semantic_revision_without_losing_task_state() {
        let probes = AtomicUsize::new(0);
        let result = for_update("t", Some(7), 2_000, || {
            let second = probes.fetch_add(1, Ordering::Relaxed) > 0;
            std::future::ready(Ok(if second {
                json!({"status":"running", "revision":8, "task_id":"t", "generation":2})
            } else {
                json!({"status":"running", "revision":7})
            }))
        })
        .await
        .unwrap();
        assert_eq!(result["revision"], 8);
        assert_eq!(result["generation"], 2);
        assert_eq!(probes.load(Ordering::Relaxed), 2);
    }
}
