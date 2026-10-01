# DC1: finish capability-driven Devin Cloud SWE-2 service tier

Status: ready / implementation already in draft PR #63; bring contract snapshots and gates green before merge
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Source backend: `issues/done/20260924-devin-cloud-backend.md`
PR: #63 (`feat/20260926-devin-swe2-priority`)

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

Draft PR #63 contains the implementation direction. Treat it as continuation work rather than restarting the feature. Review the current diff, resolve contract snapshot/fingerprint drift, and keep changes limited to the tier contract.

## 4. Acceptance

- [ ] `swe_tier=promo` preserves the requested SWE-2 mode/effort.
- [ ] `swe_tier=priority` resolves only a unique account-visible priority/fast UID compatible with the requested effort.
- [ ] missing/ambiguous catalog fails closed; no guessed UID is sent.
- [ ] non-SWE-2 mode + `swe_tier` is rejected.
- [ ] operation fingerprint distinguishes service tier.
- [ ] tool schema, gateway routed metadata/snapshots, docs, and fingerprints agree.
- [ ] no secret-bearing catalog/auth data appears in status or ordinary output.
- [ ] `cargo fmt --all -- --check` PASS.
- [ ] focused Devin Cloud tests PASS.
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
