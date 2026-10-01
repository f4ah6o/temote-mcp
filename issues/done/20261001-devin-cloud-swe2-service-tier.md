# DC1: finish capability-driven Devin Cloud SWE-2 service tier

Status: done — implementation already on main in `9151effc09367d925df64ce929105f9a8312a418`; draft PR #63 is superseded
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Source backend: `issues/done/20260924-devin-cloud-backend.md`
Historical PR: #63 (`feat/20260926-devin-swe2-priority`), superseded by the implementation already on main

## 1. Goal

Finish the independent SWE-2 service-tier axis for Devin Cloud without conflating it with SWE-2 reasoning effort.

Public intent:

```text
devin_mode = swe-2-medium | swe-2-high | swe-2-max   # reasoning effort
swe_tier   = promo | priority                         # service tier
```

The backend must preserve the requested effort and select a priority/fast account-visible model only when the installed Devin capability catalog proves one unique compatible UID.

## 2. Fixed decisions

- `promo` does not rewrite the requested SWE-2 effort. Promotion eligibility is upstream account state.
- `priority` / fast selection is capability-driven from bounded `devin models list --format json` output.
- Never synthesize or guess a priority UID/suffix.
- A missing, malformed, failed, or ambiguous catalog fails closed with an actionable capability result.
- The capability probe must not expose credentials or unbounded catalog data in ordinary task output.
- Request fingerprint/idempotency must include the service-tier input so replay with different tier fails as `operation_conflict`.
- Non-SWE-2 modes must reject `swe_tier` rather than silently ignoring it.
- Devin ACP `--cloud` remains a separate transport and does not acquire this API-v3 tier contract.

## 3. Existing implementation state

Current main already contains the tier selection, generated contract, and schema-versioned start fingerprint. `start_request_fingerprint` preserves schema-v1 receipts without the tier key; replay uses the retained fingerprint version/effective mode before catalog discovery. Schema-v1 requests adding a new tier fail as `OPERATION_CONFLICT` before a side effect. `swe_priority_resolves_catalog_uid_and_replays_without_rediscovery`, `schema_v1_replay_rejects_new_swe_tier_before_side_effect`, and `observer_migration_promotes_legacy_record_and_preserves_start_replay_marker` cover these boundaries. Do not merge the older duplicate PR #63 over this implementation. Repository-local tests were not rerun during this documentation triage; live entitlement proof remains pending in the matrix.

## 4. Acceptance

- [ ] `swe_tier=promo` preserves the requested SWE-2 mode/effort.
- [ ] `swe_tier=priority` resolves only a unique account-visible priority/fast UID compatible with the requested effort.
- [ ] missing/ambiguous catalog fails closed; no guessed UID is sent.
- [ ] non-SWE-2 mode + `swe_tier` is rejected.
- [ ] operation fingerprint distinguishes service tier while retaining exact replay for pre-tier schema-v1 receipts.
- [ ] tool schema, gateway routed metadata/snapshots, docs, and fingerprints agree.
- [ ] no secret-bearing catalog/auth data appears in status or ordinary output.
- [ ] `cargo fmt --all -- --check` PASS.
- [ ] focused Devin Cloud tests PASS, including legacy receipt replay after migration and rejection of a changed tier before remote side effects.
- [ ] `cargo test` PASS.
- [ ] `cargo clippy --all-targets -- -D warnings` PASS.
- [ ] `cargo check --no-default-features --all-targets` PASS.
- [ ] gateway contract/tests PASS.
- [ ] `git diff --check` PASS.

## 5. Live acceptance

Do not keep this implementation packet open for entitlement-dependent proof after repository gates pass. Account-visible `promo|priority` behavior remains in `issues/open/20260908-live-acceptance-matrix.md`.

## 6. Non-goals

- Reworking the already-landed Devin Cloud create/get/control backend.
- Re-testing the generic suspended→resume lifecycle in repository-local tests only.
- Changing Devin ACP cloud relay semantics.

## 7. Triage disposition (2026-10-02)

Archived because the implementation is already present on main, not because this triage reran every acceptance command. The checklist above preserves the original gate inventory. New regressions belong in a focused follow-up; account-dependent verification stays explicitly pending in the live matrix.
