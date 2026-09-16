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
