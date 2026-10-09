//! Deterministic Change graph planning and reconciled delivery receipts.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::change::{ChangeBase, ChangeRecord, ChangeStore};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryStep {
    pub change_id: String,
    pub revision: String,
    pub head_ref: String,
    pub base_ref: String,
    pub base_change_id: Option<String>,
    pub base_revision: Option<String>,
    pub method: DeliveryMethod,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryMethod {
    SinglePr,
    StackedPr,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryPlan {
    pub steps: Vec<DeliveryStep>,
    pub fingerprint: String,
}

pub(crate) fn plan(changes: &[ChangeRecord]) -> Result<DeliveryPlan> {
    let mut by_id = BTreeMap::new();
    for change in changes {
        ensure!(
            by_id.insert(change.change_id.as_str(), change).is_none(),
            "duplicate Change identity"
        );
    }
    let mut done = BTreeSet::new();
    let mut steps = Vec::new();
    while done.len() < by_id.len() {
        let mut progressed = false;
        for (&id, &change) in &by_id {
            if done.contains(id) {
                continue;
            }
            let parent = match &change.base {
                ChangeBase::OriginMain => None,
                ChangeBase::Change(parent) => Some(parent.as_str()),
            };
            if let Some(parent) = parent {
                let Some(base) = by_id.get(parent) else {
                    bail!("missing delivery parent for {id}");
                };
                ensure!(
                    base.scope == change.scope,
                    "incompatible Change delivery parent"
                );
                if !done.contains(parent) {
                    continue;
                }
            }
            ensure!(
                change.workspace_id.is_some() && !change.allocation_pending,
                "Change workspace not allocated"
            );
            ensure!(change.writer.is_none(), "Change still has an active writer");
            let revision = change
                .materialized_revision
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing materialized revision"))?;
            let verification = change
                .verification
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing verification"))?;
            ensure!(
                verification.passed
                    && verification.revision == revision
                    && verification.record_revision <= change.revision
                    && verification.task_record_revision > 0
                    && verification.task_record_revision == change.task_record_revision,
                "verification is stale or failed"
            );
            let (base_ref, base_revision) = match parent {
                None => ("main".to_owned(), None),
                Some(id) => {
                    let base = by_id[id];
                    (format!("temote/{id}"), base.materialized_revision.clone())
                }
            };
            steps.push(DeliveryStep {
                change_id: id.into(),
                revision: revision.into(),
                head_ref: format!("temote/{id}"),
                base_ref,
                base_change_id: parent.map(str::to_owned),
                base_revision,
                method: if parent.is_some() {
                    DeliveryMethod::StackedPr
                } else {
                    DeliveryMethod::SinglePr
                },
            });
            done.insert(id);
            progressed = true;
        }
        ensure!(progressed, "Change dependency cycle");
    }
    let bytes = serde_json::to_vec(&steps)?;
    let fingerprint = format!("{:x}", Sha256::digest(bytes));
    Ok(DeliveryPlan { steps, fingerprint })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryState {
    Accepted,
    Delegated,
    ReconciliationRequired,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequestReceipt {
    pub number: u64,
    pub head_ref: String,
    pub base_ref: String,
    pub revision: String,
    pub url: String,
    pub stack_parent_pr_number: Option<u64>,
    pub stack_linked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryObservation {
    pub branch_revision: Option<String>,
    pub pull_request: Option<PullRequestReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliveryReceipt {
    pub operation_id: Uuid,
    pub plan_fingerprint: String,
    pub state: DeliveryState,
    pub delegated_task_id: Option<String>,
    pub branch_revision: Option<String>,
    pub pull_request: Option<PullRequestReceipt>,
    #[serde(default)]
    pub observation_operation_id: Option<Uuid>,
    #[serde(default)]
    pub observation_task_id: Option<String>,
}

impl DeliveryReceipt {
    pub(crate) fn is_terminal(&self) -> bool {
        self.state == DeliveryState::Completed
    }
}

/// Persist intent before invoking a coding agent. An accepted retry returns
/// the same receipt; it must be reconciled, never started under a new key.
#[cfg(test)]
pub(crate) fn accept(
    store: &ChangeStore,
    change_id: &str,
    expected_revision: u64,
    plan: &DeliveryPlan,
    operation_id: Uuid,
) -> Result<DeliveryReceipt> {
    accept_once(store, change_id, expected_revision, plan, operation_id).map(|(receipt, _)| receipt)
}

/// Returns whether this call durably claimed the intent. Only that caller
/// may begin the delegated delivery task.
pub(crate) fn accept_once(
    store: &ChangeStore,
    change_id: &str,
    expected_revision: u64,
    plan: &DeliveryPlan,
    operation_id: Uuid,
) -> Result<(DeliveryReceipt, bool)> {
    validate_plan_fresh(store, plan)?;
    let step = plan
        .steps
        .iter()
        .find(|s| s.change_id == change_id)
        .ok_or_else(|| anyhow::anyhow!("Change absent from delivery plan"))?;
    if let Some(parent) = &step.base_change_id {
        ensure!(
            store
                .get(parent)?
                .delivery
                .as_ref()
                .is_some_and(|d| d.state == DeliveryState::Completed),
            "stack parent delivery is incomplete"
        );
    }
    let current = store.get(change_id)?;
    ensure!(
        current.materialized_revision.as_deref() == Some(step.revision.as_str()),
        "delivery revision is stale"
    );
    if let Some(receipt) = current.delivery {
        ensure!(
            receipt.operation_id == operation_id && receipt.plan_fingerprint == plan.fingerprint,
            "delivery operation conflict"
        );
        return Ok((receipt, false));
    }
    ensure!(
        current.revision == expected_revision,
        "Change record revision conflict"
    );
    let receipt = DeliveryReceipt {
        operation_id,
        plan_fingerprint: plan.fingerprint.clone(),
        state: DeliveryState::Accepted,
        delegated_task_id: None,
        branch_revision: None,
        pull_request: None,
        observation_operation_id: None,
        observation_task_id: None,
    };
    store.update(change_id, expected_revision, |r| {
        r.delivery = Some(receipt.clone());
        Ok(())
    })?;
    Ok((receipt, true))
}

pub(crate) fn validate_plan_fresh(store: &ChangeStore, plan: &DeliveryPlan) -> Result<()> {
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan.steps)?));
    ensure!(
        fingerprint == plan.fingerprint,
        "delivery plan fingerprint mismatch"
    );
    for step in &plan.steps {
        let change = store.get(&step.change_id)?;
        ensure!(change.writer.is_none(), "Change still has an active writer");
        ensure!(
            change.materialized_revision.as_deref() == Some(step.revision.as_str()),
            "delivery plan revision is stale"
        );
        let verification = change
            .verification
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("delivery verification missing"))?;
        ensure!(
            verification.passed
                && verification.revision == step.revision
                && verification.task_record_revision > 0
                && verification.task_record_revision == change.task_record_revision,
            "delivery verification is stale"
        );
        if let Some(parent) = &step.base_change_id {
            let base = store.get(parent)?;
            ensure!(
                base.scope == change.scope && base.materialized_revision == step.base_revision,
                "delivery parent revision is stale or incompatible"
            );
            ensure!(
                step.base_ref == format!("temote/{parent}"),
                "delivery base ref mismatch"
            );
        } else {
            ensure!(
                step.base_ref == "main" && step.base_revision.is_none(),
                "delivery base mismatch"
            );
        }
        ensure!(
            step.head_ref == format!("temote/{}", step.change_id),
            "delivery head ref mismatch"
        );
        ensure!(
            step.method
                == if step.base_change_id.is_some() {
                    DeliveryMethod::StackedPr
                } else {
                    DeliveryMethod::SinglePr
                },
            "delivery method mismatch"
        );
    }
    Ok(())
}

/// A retained delegated task ID is a receipt, not proof that remote effects
/// happened. Duplicate starts are refused once acceptance is recorded.
pub(crate) fn record_delegation(
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    operation_id: Uuid,
    task_id: &str,
) -> Result<DeliveryReceipt> {
    crate::change::label(task_id)?;
    let updated = store.update(change_id, rev, |r| {
        let receipt = r
            .delivery
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("delivery not accepted"))?;
        ensure!(
            receipt.operation_id == operation_id,
            "delivery operation conflict"
        );
        if let Some(existing) = &receipt.delegated_task_id {
            ensure!(existing == task_id, "delivery task conflict");
        }
        if receipt.state == DeliveryState::Completed {
            return Ok(());
        }
        receipt.delegated_task_id = Some(task_id.into());
        receipt.state = DeliveryState::Delegated;
        Ok(())
    })?;
    Ok(updated.delivery.unwrap())
}

/// Persist the read-only remote observation request before starting its
/// coding-agent task. Replays keep one stable operation identity.
pub(crate) fn prepare_observation(
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    delivery_operation_id: Uuid,
    observation_operation_id: Uuid,
) -> Result<DeliveryReceipt> {
    let current = store.get(change_id)?;
    let existing = current
        .delivery
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("delivery not accepted"))?;
    ensure!(
        existing.operation_id == delivery_operation_id,
        "delivery operation conflict"
    );
    if let Some(id) = existing.observation_operation_id {
        ensure!(
            id == observation_operation_id,
            "observation operation conflict"
        );
        return Ok(existing.clone());
    }
    let updated = store.update(change_id, rev, |r| {
        let receipt = r
            .delivery
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("delivery not accepted"))?;
        ensure!(
            receipt.operation_id == delivery_operation_id,
            "delivery operation conflict"
        );
        receipt.observation_operation_id = Some(observation_operation_id);
        Ok(())
    })?;
    updated
        .delivery
        .ok_or_else(|| anyhow::anyhow!("delivery receipt missing"))
}

pub(crate) fn record_observation_task(
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    delivery_operation_id: Uuid,
    observation_operation_id: Uuid,
    task_id: &str,
) -> Result<DeliveryReceipt> {
    Uuid::parse_str(task_id)?;
    let updated = store.update(change_id, rev, |r| {
        let receipt = r
            .delivery
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("delivery not accepted"))?;
        ensure!(
            receipt.operation_id == delivery_operation_id
                && receipt.observation_operation_id == Some(observation_operation_id),
            "observation operation conflict"
        );
        if let Some(existing) = &receipt.observation_task_id {
            ensure!(existing == task_id, "observation task conflict");
        }
        receipt.observation_task_id = Some(task_id.to_owned());
        Ok(())
    })?;
    updated
        .delivery
        .ok_or_else(|| anyhow::anyhow!("delivery receipt missing"))
}

/// The caller obtains this observation through a read-only remote probe or
/// bounded delegated evidence. Absence and mismatch require reconciliation.
pub(crate) fn reconcile(
    store: &ChangeStore,
    change_id: &str,
    rev: u64,
    plan: &DeliveryPlan,
    operation_id: Uuid,
    observed: DeliveryObservation,
) -> Result<DeliveryReceipt> {
    validate_plan_fresh(store, plan)?;
    let step = plan
        .steps
        .iter()
        .find(|s| s.change_id == change_id)
        .ok_or_else(|| anyhow::anyhow!("Change absent from delivery plan"))?;
    let expected_parent_pr = if let Some(parent) = &step.base_change_id {
        Some(
            store
                .get(parent)?
                .delivery
                .and_then(|d| d.pull_request)
                .ok_or_else(|| anyhow::anyhow!("stack parent PR not reconciled"))?
                .number,
        )
    } else {
        None
    };
    let updated = store.update(change_id, rev, |r| {
        let receipt = r
            .delivery
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("delivery not accepted"))?;
        ensure!(
            receipt.operation_id == operation_id && receipt.plan_fingerprint == plan.fingerprint,
            "delivery operation conflict"
        );
        ensure!(
            r.materialized_revision.as_deref() == Some(step.revision.as_str()),
            "delivered revision is stale"
        );
        if let Some(branch) = &observed.branch_revision {
            if branch == &step.revision {
                receipt.branch_revision = Some(branch.clone());
            } else {
                receipt.state = DeliveryState::ReconciliationRequired;
                return Ok(());
            }
        }
        if let Some(pr) = observed.pull_request.as_ref() {
            if pr.number > 0
                && pr.head_ref == step.head_ref
                && pr.base_ref == step.base_ref
                && pr.revision == step.revision
                && pr.url.starts_with("https://")
                && match step.method {
                    DeliveryMethod::SinglePr => {
                        pr.stack_parent_pr_number.is_none() && !pr.stack_linked
                    }
                    DeliveryMethod::StackedPr => {
                        pr.stack_parent_pr_number == expected_parent_pr && pr.stack_linked
                    }
                }
                && observed.branch_revision.as_deref() == Some(step.revision.as_str())
            {
                if let Some(old) = &receipt.pull_request {
                    ensure!(old == pr, "conflicting remote PR identity");
                }
                receipt.pull_request = Some(pr.clone());
                receipt.state = DeliveryState::Completed;
            } else {
                receipt.state = DeliveryState::ReconciliationRequired;
            }
        } else {
            receipt.state = DeliveryState::ReconciliationRequired;
        }
        Ok(())
    })?;
    Ok(updated.delivery.unwrap())
}

/// Typed instruction for a coding-agent task. The task itself owns Git and
/// GitHub operations through the ordinary approval/sandbox boundary.
pub(crate) fn delegated_instruction(step: &DeliveryStep) -> String {
    let stack = match step.method {
        DeliveryMethod::SinglePr => "No stack link is needed.",
        DeliveryMethod::StackedPr => {
            "After verifying the parent PR and this PR's exact base/head, use the supported gh-stack link operation only if its capability is confirmed; otherwise report unsupported. Return the parent PR number and whether the stack link was observed."
        }
    };
    format!(
        "Deliver Temote Change {} at exact revision {} to head ref {} with PR base {}. First inspect existing branch and PR state; reconcile the exact head/base/revision before any mutation. Publish only this verified revision and create or update one PR. {} Do not merge, retarget an incompatible PR, force-push, reset, clean, or modify sibling workspaces. Return bounded branch and PR identity evidence. If any effect is uncertain, stop for reconciliation.",
        step.change_id, step.revision, step.head_ref, step.base_ref, stack
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{ChangeScope, Verification};
    use crate::test_support;

    fn remote_observation(pr: PullRequestReceipt) -> DeliveryObservation {
        DeliveryObservation {
            branch_revision: Some(pr.revision.clone()),
            pull_request: Some(pr),
        }
    }

    fn change(id: &str, base: ChangeBase) -> ChangeRecord {
        ChangeRecord {
            schema_version: 1,
            change_id: id.into(),
            scope: ChangeScope {
                session_id: "s".into(),
                session_started_at: 1,
                session_process_id: 1,
                canonical_root: "/tmp".into(),
                repository_id: "r".into(),
            },
            task_id: format!("task-{id}"),
            parent_task_id: None,
            parent_change_id: None,
            base,
            workspace_id: Some(format!("w-{id}")),
            provisioning_operation_id: None,
            allocation_operation_id: None,
            allocation_pending: false,
            executions: vec![],
            initial_start: None,
            writer: None,
            writer_generation: 0,
            logical_change_id: None,
            materialized_revision: Some(format!("rev-{id}")),
            latest_snapshot_operation_id: None,
            task_record_revision: 1,
            verification: Some(Verification {
                revision: format!("rev-{id}"),
                record_revision: 2,
                task_record_revision: 1,
                passed: true,
            }),
            delivery: None,
            revision: 2,
        }
    }

    #[test]
    fn graph_order_matches_reference_model() -> noprop::TestResult {
        test_support::run(0x4348_414e_4745_4752, 512, |ctx| {
            let n = noprop::sample_usize_in(ctx, 0..=12);
            let mut graph = Vec::new();
            for i in 0..n {
                let parent = if i == 0 || noprop::sample_usize_in(ctx, 0..=1) == 0 {
                    None
                } else {
                    Some(noprop::sample_usize_in(ctx, 0..=i - 1))
                };
                graph.push(change(
                    &format!("c{i}"),
                    parent.map_or(ChangeBase::OriginMain, |p| {
                        ChangeBase::Change(format!("c{p}"))
                    }),
                ));
            }
            let expected = plan(&graph).unwrap();
            graph.reverse();
            let actual = plan(&graph).unwrap();
            assert_eq!(actual, expected);
            for (index, step) in actual.steps.iter().enumerate() {
                if let Some(parent) = &step.base_change_id {
                    assert!(
                        actual.steps[..index]
                            .iter()
                            .any(|prior| &prior.change_id == parent)
                    );
                }
            }
            Ok(())
        })
    }

    #[test]
    fn cycle_missing_parent_and_stale_verification_fail() {
        let mut a = change("a", ChangeBase::Change("b".into()));
        let b = change("b", ChangeBase::Change("a".into()));
        assert!(plan(&[a.clone(), b]).is_err());
        a.base = ChangeBase::Change("missing".into());
        assert!(plan(&[a.clone()]).is_err());
        a.base = ChangeBase::OriginMain;
        a.materialized_revision = Some("new".into());
        assert!(plan(&[a]).is_err());
        let mut active = change("active", ChangeBase::OriginMain);
        active.writer = Some(crate::change::WriterLease {
            execution_id: "exec".into(),
            generation: 1,
        });
        assert!(plan(&[active]).is_err());
    }

    #[test]
    fn accepted_receipt_replays_and_remote_mismatch_stays_uncertain() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let scope = ChangeScope {
            session_id: "s".into(),
            session_started_at: 1,
            session_process_id: 1,
            canonical_root: canonical.clone(),
            repository_id: "r".into(),
        };
        let store = ChangeStore::open(&canonical.join("changes"), scope).unwrap();
        let created = store
            .create("task", None, None, ChangeBase::OriginMain)
            .unwrap();
        let bound = store
            .bind_workspace(&created.change_id, created.revision, "workspace")
            .unwrap();
        let observed = store
            .observe_revision(&created.change_id, bound.revision, "logical", "revision")
            .unwrap();
        let task_observed = store
            .observe_task_revision(&created.change_id, observed.revision, 1)
            .unwrap();
        let verified = store
            .verify(&created.change_id, task_observed.revision, "revision", true)
            .unwrap();
        let delivery_plan = plan(std::slice::from_ref(&verified)).unwrap();
        let operation = Uuid::new_v4();
        let (receipt, claimed) = accept_once(
            &store,
            &created.change_id,
            verified.revision,
            &delivery_plan,
            operation,
        )
        .unwrap();
        assert!(claimed);
        assert_eq!(receipt.state, DeliveryState::Accepted);
        let accepted_revision = store.get(&created.change_id).unwrap().revision;
        assert_eq!(
            accept_once(
                &store,
                &created.change_id,
                accepted_revision,
                &delivery_plan,
                operation
            )
            .unwrap(),
            (receipt.clone(), false)
        );
        assert!(
            accept(
                &store,
                &created.change_id,
                accepted_revision,
                &delivery_plan,
                Uuid::new_v4()
            )
            .is_err()
        );
        let observe_id = Uuid::new_v4();
        let prepared = prepare_observation(
            &store,
            &created.change_id,
            accepted_revision,
            operation,
            observe_id,
        )
        .unwrap();
        assert_eq!(prepared.observation_operation_id, Some(observe_id));
        let prepared_revision = store.get(&created.change_id).unwrap().revision;
        assert_eq!(
            prepare_observation(
                &store,
                &created.change_id,
                prepared_revision,
                operation,
                observe_id
            )
            .unwrap(),
            prepared
        );
        assert!(
            prepare_observation(
                &store,
                &created.change_id,
                prepared_revision,
                operation,
                Uuid::new_v4()
            )
            .is_err()
        );
        let observer_task = Uuid::new_v4().to_string();
        let recorded = record_observation_task(
            &store,
            &created.change_id,
            prepared_revision,
            operation,
            observe_id,
            &observer_task,
        )
        .unwrap();
        assert_eq!(
            recorded.observation_task_id.as_deref(),
            Some(observer_task.as_str())
        );
        let accepted_revision = store.get(&created.change_id).unwrap().revision;
        let step = &delivery_plan.steps[0];
        let mismatch = PullRequestReceipt {
            number: 10,
            head_ref: step.head_ref.clone(),
            base_ref: "wrong".into(),
            revision: step.revision.clone(),
            url: "https://example.test/10".into(),
            stack_parent_pr_number: None,
            stack_linked: false,
        };
        assert_eq!(
            reconcile(
                &store,
                &created.change_id,
                accepted_revision,
                &delivery_plan,
                operation,
                remote_observation(mismatch)
            )
            .unwrap()
            .state,
            DeliveryState::ReconciliationRequired
        );
        let current = store.get(&created.change_id).unwrap();
        assert_eq!(
            current
                .delivery
                .as_ref()
                .unwrap()
                .branch_revision
                .as_deref(),
            Some(step.revision.as_str())
        );
        assert!(!store.release_ready(&created.change_id).unwrap());
        let matching = PullRequestReceipt {
            number: 10,
            head_ref: step.head_ref.clone(),
            base_ref: step.base_ref.clone(),
            revision: step.revision.clone(),
            url: "https://example.test/10".into(),
            stack_parent_pr_number: None,
            stack_linked: false,
        };
        assert_eq!(
            reconcile(
                &store,
                &created.change_id,
                current.revision,
                &delivery_plan,
                operation,
                remote_observation(matching)
            )
            .unwrap()
            .state,
            DeliveryState::Completed
        );
    }

    #[test]
    fn stacked_delivery_requires_parent_receipt_and_observed_link() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let scope = ChangeScope {
            session_id: "s".into(),
            session_started_at: 1,
            session_process_id: 1,
            canonical_root: canonical.clone(),
            repository_id: "r".into(),
        };
        let store = ChangeStore::open(&canonical.join("changes"), scope).unwrap();
        let prepare = |record: ChangeRecord, workspace: &str| {
            let bound = store
                .bind_workspace(&record.change_id, record.revision, workspace)
                .unwrap();
            let observed = store
                .observe_revision(&record.change_id, bound.revision, workspace, workspace)
                .unwrap();
            let task = store
                .observe_task_revision(&record.change_id, observed.revision, 1)
                .unwrap();
            store
                .verify(&record.change_id, task.revision, workspace, true)
                .unwrap()
        };
        let parent = prepare(
            store
                .create("parent-task", None, None, ChangeBase::OriginMain)
                .unwrap(),
            "parent-w",
        );
        let child = prepare(
            store
                .create(
                    "child-task",
                    Some("parent-task"),
                    None,
                    ChangeBase::Change(parent.change_id.clone()),
                )
                .unwrap(),
            "child-w",
        );
        let plan = plan(&[child.clone(), parent.clone()]).unwrap();
        assert_eq!(plan.steps[0].change_id, parent.change_id);
        assert_eq!(plan.steps[1].method, DeliveryMethod::StackedPr);
        let child_op = Uuid::new_v4();
        assert!(accept(&store, &child.change_id, child.revision, &plan, child_op).is_err());
        let parent_op = Uuid::new_v4();
        accept(&store, &parent.change_id, parent.revision, &plan, parent_op).unwrap();
        let parent_step = &plan.steps[0];
        let parent_pr = PullRequestReceipt {
            number: 7,
            head_ref: parent_step.head_ref.clone(),
            base_ref: parent_step.base_ref.clone(),
            revision: parent_step.revision.clone(),
            url: "https://example.test/7".into(),
            stack_parent_pr_number: None,
            stack_linked: false,
        };
        reconcile(
            &store,
            &parent.change_id,
            store.get(&parent.change_id).unwrap().revision,
            &plan,
            parent_op,
            remote_observation(parent_pr),
        )
        .unwrap();
        accept(&store, &child.change_id, child.revision, &plan, child_op).unwrap();
        let child_step = &plan.steps[1];
        let mut child_pr = PullRequestReceipt {
            number: 8,
            head_ref: child_step.head_ref.clone(),
            base_ref: child_step.base_ref.clone(),
            revision: child_step.revision.clone(),
            url: "https://example.test/8".into(),
            stack_parent_pr_number: Some(7),
            stack_linked: false,
        };
        assert_eq!(
            reconcile(
                &store,
                &child.change_id,
                store.get(&child.change_id).unwrap().revision,
                &plan,
                child_op,
                remote_observation(child_pr.clone())
            )
            .unwrap()
            .state,
            DeliveryState::ReconciliationRequired
        );
        child_pr.stack_linked = true;
        assert_eq!(
            reconcile(
                &store,
                &child.change_id,
                store.get(&child.change_id).unwrap().revision,
                &plan,
                child_op,
                remote_observation(child_pr)
            )
            .unwrap()
            .state,
            DeliveryState::Completed
        );
    }
}
