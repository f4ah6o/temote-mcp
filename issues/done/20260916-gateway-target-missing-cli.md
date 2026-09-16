# Gateway deploy preflight CLI: make `target_missing` reachable

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260911-gateway-deployment-target.md`
Prepared historical fix: `04ddd76` on `codex/20260915-completion-launcher`

## Goal

When a valid hostname/config is supplied with neither `--route` nor `--custom-domain`, the deployment-preflight CLI must reach the evaluator and return the existing fixed `target_missing` classification instead of exiting early with usage code 2.

## Scope

- inspect current `gateway/scripts/deployment-preflight.mjs` first;
- semantically re-apply only the still-missing behavior from historical commit `04ddd76` if current `main` does not already cover it;
- add a command-level regression test in `gateway/test/deployment-preflight.test.mjs`;
- preserve existing evaluator behavior for target-present, mismatch, remote-unknown, and unsafe `workers_dev`;
- perform no Cloudflare/DNS/Access mutation and use no credentials.

Do not merge or cherry-pick the old branch wholesale.

## Acceptance

- no-target CLI invocation reaches `target_missing`;
- existing deployment-preflight tests remain green;
- `(cd gateway && npm test)` passes;
- `git diff --check` passes.

Live Cloudflare route/domain verification remains only in `issues/open/20260908-live-acceptance-matrix.md`.

## Implementation notes (2026-09-16)

Current `main` did not already cover this: `parseArgs` still threw usage when neither `--route` nor `--custom-domain` was supplied, and the test file only exercised `evaluateDeploymentPreflight` directly. The historical `04ddd76` change was re-applied semantically, not cherry-picked:

- `gateway/scripts/deployment-preflight.mjs`: the argument guard now rejects only a simultaneous `--route` + `--custom-domain` combination, so a valid hostname/config without a target reaches `targetResult` and returns `target_missing`.
- `gateway/test/deployment-preflight.test.mjs`: added the `spawnSync` CLI helper plus two command-level tests (no-target emits `target_missing` JSON with exit 1; route+custom-domain together stays a usage error with exit 2 and empty stdout).

Observed behavior:

- `node scripts/deployment-preflight.mjs --hostname gateway.example.com --config wrangler.toml` -> `{"status":"target_missing","workers_dev":false,"target":{"kind":"custom_domain","status":"target_missing"},"remote":{"status":"not_checked"}}`, exit 1; no hostname/config path in output.
- `node scripts/deployment-preflight.mjs --hostname gateway.example.com --route 'gateway.example.com/*' --custom-domain gateway.example.com --config wrangler.toml` -> usage on stderr, exit 2, empty stdout.
- Existing evaluator behavior for target-present, mismatch, remote-unknown, unsafe `workers_dev`, and target-value non-disclosure is unchanged.

Gates:

- `(cd gateway && npm test)`: PASS (70/70; deployment-preflight file 7/7 including the 2 new CLI tests).
- `just sandboxed-check`: exit 0. lib 110/110, targeted bin subsets 3/3, 40/40, 5/5, 6/6, gateway 70/70, fmt/clippy/`--no-default-features` PASS, `git diff --check` PASS.
- host/CI-only: NOT RUN (Linux nested sandbox runtime tests, full binary/local Unix-socket integration suite, ignored supervisor/process-boundary E2E).
- No Cloudflare credential, DNS, route, or Access mutation was performed.

## 2026-09-16 completion

All packet acceptance items are met repository-locally. The remaining live Cloudflare route/domain verification is now a row in the Cloudflare Worker section of `issues/open/20260908-live-acceptance-matrix.md`; the source tracker `issues/open/20260911-gateway-deployment-target.md` is closed by this packet plus that live row.
