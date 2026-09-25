//! Session-owned task list: a bounded read-only projection over the
//! per-backend task stores.
//!
//! Each backend store stays the source of truth. The shared list only
//! projects records filtered to the full session instance and canonical
//! scope — the same ownership `ensure_task_owner` applies — so records of
//! other sessions never mix in, even when backend stores cross boundaries.
//! A backend whose store cannot be read is reported as unconfirmed rather
//! than as an empty page, and records that fail to read or validate are
//! counted instead of silently looking like "no tasks". Task IDs are the
//! store's own; the projection never renumbers them.

use anyhow::Result;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::Backend;

/// One retained task reference projected for the owning session: the
/// common reference a caller uses to route a `task_get` or `task_control`
/// back to the right backend.
#[derive(Clone, Debug)]
pub(crate) struct ListedTask {
    pub(crate) task_id: Uuid,
    pub(crate) status: &'static str,
    pub(crate) revision: u64,
    pub(crate) generation: u64,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

/// One backend's page of the session-scoped projection.
#[derive(Debug, Default)]
pub(crate) struct BackendTaskPage {
    pub(crate) entries: Vec<ListedTask>,
    /// Records that failed to read or validate; reported so a corrupted
    /// record never silently looks like an empty task list.
    pub(crate) unreadable_records: usize,
}

/// Merge backend pages into the bounded shared task list. Entries are
/// sorted by `updated_at` descending, then backend label and task ID
/// ascending, and truncated to `limit`; every backend is reported as
/// `ok` with its unreadable-record count or as `unconfirmed` with a
/// bounded error when its store could not be read.
pub(crate) fn render(pages: Vec<(Backend, Result<BackendTaskPage>)>, limit: usize) -> Value {
    let mut tasks: Vec<(Backend, ListedTask)> = Vec::new();
    let mut backends = Map::new();
    for (backend, page) in pages {
        let label = backend.label();
        match page {
            Ok(page) => {
                for entry in page.entries {
                    tasks.push((backend, entry));
                }
                backends.insert(
                    label.to_owned(),
                    json!({
                        "status": "ok",
                        "unreadable_records": page.unreadable_records,
                    }),
                );
            }
            Err(error) => {
                backends.insert(
                    label.to_owned(),
                    json!({
                        "status": "unconfirmed",
                        "error": super::render_approval_argument(&format!("{error:#}")),
                    }),
                );
            }
        }
    }
    tasks.sort_by(|(left_backend, left), (right_backend, right)| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left_backend.label().cmp(right_backend.label()))
            .then_with(|| left.task_id.cmp(&right.task_id))
    });
    let truncated = tasks.len() > limit;
    tasks.truncate(limit);
    let tasks = tasks
        .into_iter()
        .map(|(backend, entry)| {
            json!({
                "backend": backend.label(),
                "task_id": entry.task_id,
                "status": entry.status,
                "revision": entry.revision,
                "generation": entry.generation,
                "created_at": entry.created_at,
                "updated_at": entry.updated_at,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "tasks": tasks,
        "truncated": truncated,
        "backends": Value::Object(backends),
        "retention": "per_backend_store",
    })
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "network")]
    use anyhow::anyhow;

    use super::*;

    fn listed(task_id: Uuid, updated_at: u64) -> ListedTask {
        ListedTask {
            task_id,
            status: "running",
            revision: 7,
            generation: 3,
            created_at: 41,
            updated_at,
        }
    }

    fn page(entries: Vec<ListedTask>, unreadable_records: usize) -> BackendTaskPage {
        BackendTaskPage {
            entries,
            unreadable_records,
        }
    }

    #[test]
    fn render_merges_pages_and_never_fakes_unconfirmed() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let pages = vec![
            (
                Backend::Codex,
                Ok(page(vec![listed(first, 100), listed(second, 300)], 2)),
            ),
            (
                Backend::DevinAcp,
                Ok(page(vec![listed(Uuid::new_v4(), 200)], 0)),
            ),
            #[cfg(feature = "network")]
            (Backend::OpenCode, Err(anyhow!("store offline"))),
            #[cfg(feature = "network")]
            (Backend::DevinCloud, Ok(page(Vec::new(), 0))),
        ];
        let rendered = render(pages, 50);
        let tasks = rendered["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0]["task_id"], json!(second));
        assert_eq!(tasks[0]["backend"], json!("codex"));
        assert_eq!(tasks[1]["backend"], json!("devin"));
        assert_eq!(tasks[1]["status"], json!("running"));
        assert_eq!(tasks[1]["revision"], json!(7));
        assert_eq!(tasks[1]["generation"], json!(3));
        assert_eq!(tasks[1]["created_at"], json!(41));
        assert_eq!(tasks[2]["task_id"], json!(first));
        assert_eq!(rendered["truncated"], json!(false));
        assert_eq!(rendered["retention"], json!("per_backend_store"));
        let backends = rendered["backends"].as_object().unwrap();
        assert_eq!(backends["codex"]["status"], json!("ok"));
        assert_eq!(backends["codex"]["unreadable_records"], json!(2));
        assert_eq!(backends["devin"]["status"], json!("ok"));
        #[cfg(feature = "network")]
        {
            assert_eq!(backends["opencode"]["status"], json!("unconfirmed"));
            assert!(
                backends["opencode"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("store offline")
            );
            assert_eq!(backends["devin_cloud"]["status"], json!("ok"));
            assert_eq!(backends["devin_cloud"]["unreadable_records"], json!(0));
        }
    }

    #[test]
    fn render_truncates_deterministically() {
        let pages = vec![(
            Backend::Codex,
            Ok(page(
                (0..8).map(|index| listed(Uuid::new_v4(), index)).collect(),
                0,
            )),
        )];
        let rendered = render(pages, 3);
        let tasks = rendered["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(rendered["truncated"], json!(true));
        assert_eq!(tasks[0]["updated_at"], json!(7));
        assert_eq!(tasks[1]["updated_at"], json!(6));
        assert_eq!(tasks[2]["updated_at"], json!(5));
    }

    #[test]
    fn render_orders_ties_by_backend_then_task_id() {
        let small = Uuid::from_u128(1);
        let large = Uuid::from_u128(2);
        let pages = vec![
            (Backend::DevinAcp, Ok(page(vec![listed(large, 50)], 0))),
            (Backend::Codex, Ok(page(vec![listed(small, 50)], 0))),
        ];
        let rendered = render(pages, 50);
        let tasks = rendered["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["backend"], json!("codex"));
        assert_eq!(tasks[0]["task_id"], json!(small));
        assert_eq!(tasks[1]["backend"], json!("devin"));
        assert_eq!(tasks[1]["task_id"], json!(large));
    }
}
