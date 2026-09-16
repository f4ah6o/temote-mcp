# Agent mode completion roadmap: issues zero / main only

Status: active roadmap
Created: 2026-09-16
Coordinator: ChatGPT / Temote parent
Implementation worker after Phase 0: OpenCode `opencode-go/deepseek-v4.1-flash`
Canonical branch: `main`

## Exit criteria

This roadmap is complete only when all of the following are true:

- `issues/open/` has no actionable issue files.
- `issues/doing/` and `issues/polished/` have no unfinished issue files; completed work is under `issues/done/` or intentionally rejected/superseded work under `issues/closed/`.
- local and remote development branches have been reviewed and either integrated/salvaged or explicitly discarded; the only remaining development branch is `main`.
- stale linked worktrees have been removed only after proving they contain no uncommitted work that belongs to another task/worker.
- `main` is synchronized with `origin/main` and repository-local gates are green.
- host/CI/live-only acceptance is recorded as PASS or, if the product intentionally drops that path, the owning issue is closed with the decision and evidence. Do not convert NOT RUN into PASS.

## Operating rules

1. Work only from current `main`; do not merge old completion branches wholesale.
2. Before every packet, run `git status --short --branch` and preserve unrelated dirty/untracked work.
3. One implementation packet should target one coherent behavior and normally one commit-sized diff.
4. OpenCode may edit workspace files but must not be relied on for unrestricted `.git` mutation. Parent Temote handles staging/commit/push until the Git broker work is complete.
5. Every packet names exact focused tests. After focused tests, run `just sandboxed-check`; host-only lines remain NOT RUN.
6. Do not weaken protected `.git`/`.agents`/`.codex`, secret isolation, path containment, or network policy to make a test pass.
7. When work is already landed on `main`, do not reimplement it. Reduce the remaining issue to docs/live acceptance or close it into the live matrix.

## Phase 0 — make the implementation worker usable

Current connected Temote runtime is older than current `main`. A live OpenCode canary on 2026-09-16 failed before job creation with:

```text
EPERM : failed to spawn process
```

Current `main` already contains the source-side fix (`81356a2` allow libuv local-agent socketpairs) and classification work (`9b76590`). Before assigning repository work to OpenCode:

- rebuild/install/deploy the current Temote runtime using the supported release/deployment path;
- reconnect/restart the managed Temote session using that runtime;
- confirm the connected tool schema includes the current `local_agent_run` contract;
- run read-only OpenCode canary with `opencode-go/deepseek-v4.1-flash`;
- run workspace-write canary that creates/removes one temporary workspace file only;
- record the canary in `issues/doing/20260916-local-agent-opencode-eperm.md` and the live matrix.

Do not create a second EPERM issue. If the rebuilt runtime still fails, continue the existing doing issue with the fixed failure classification.

## Phase 1 — finish small repository-local friction packets

Run these polished packets in order; each is intentionally sized for DeepSeek Flash:

1. `issues/polished/20260916-gateway-target-missing-cli.md` — re-apply/reconcile the prepared `04ddd76` CLI `target_missing` fix against current `main`; do not merge the old launcher branch.
2. `issues/polished/20260916-release-toolchain-contract.md`
3. `issues/polished/20260916-sandbox-setup-activity-classification.md`
4. `issues/polished/20260916-codex-direct-cli-private-state.md`
5. `issues/polished/20260916-connected-surface-contract-parity.md`
6. `issues/polished/20260916-session-orphan-gc.md`
7. `issues/polished/20260916-agent-network-mode-policy.md`

The parent `issues/open/20260915-agent-development-network-access.md` is tracking-only. Close it from the bounded child packet plus Phase 4 live network evidence; never hand the umbrella directly to OpenCode.

After each packet: focused tests -> `just sandboxed-check` -> diff review -> parent stages/commits/pushes -> move packet to `done/` and update/supersede the source issue.

## Phase 2 — transparent Git developer UX

Complete in this order; do not start the next packet until the previous contract is stable:

1. `20260916-git-shim-switch-create.md`
2. `20260916-git-shim-add-commit.md`
3. `issues/polished/20260916-managed-worktree-create-list.md`
4. `issues/polished/20260916-managed-worktree-session-integration.md`
5. `issues/polished/20260916-structured-worktree-remove.md`
6. `issues/polished/20260916-managed-worktree-prune.md`
7. `20260916-git-shim-worktree.md`
8. `20260916-git-shim-network-gh-git.md`
9. `issues/polished/20260916-structured-branch-delete.md`
10. `issues/polished/20260916-git-shim-cleanup-commands.md`
11. `20260916-github-pr-broker.md`
12. `20260916-agent-repository-triage-e2e.md`

`issues/open/20260916-temote-managed-worktree-broker.md`, `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`, and `issues/open/20260916-agent-mode-repository-triage-end-to-end.md` are umbrella trackers only. New worktree creation must use `~/src/worktrees/<repo>/<task>`; the old `<repo>/.wt/<name>` convention is legacy state to preserve, not the target policy. `issues/doing/20260916-repo-scoped-github-account-selection.md` owns remaining live credential evidence. The former concrete blocker issues moved to `issues/closed/` are regression evidence only and must not be reimplemented independently.

## Phase 3 — salvage the 2026-09-15 completion branches

The completion family diverged before current `main`; current main also contains a later combined landing (`914d9b8`) and many 2026-09-16 fixes. Therefore **do not merge these branches wholesale**. Port only still-missing behavior after a current-main semantic diff.

1. `20260916-completion-activity-branch-salvage.md`
2. `20260916-completion-upgrade-branch-salvage.md`
3. `20260916-completion-appserver-branch-salvage.md`
4. `20260916-completion-evidence-branch-salvage.md`

Each of the first three salvage packets is audit/reconciliation only. If it finds a missing coherent behavior, create one new Flash-sized `issues/polished/` child with exact focused tests before changing code.

After the upgrade salvage audit, execute the separate upgrade-friction packets in order:

1. `issues/polished/20260916-upgrade-helper-generation-preflight.md`
2. `issues/polished/20260916-upgrade-ingress-process-ownership.md`
3. `issues/polished/20260916-upgrade-runtime-observation-consistency.md`

The parent `issues/open/20260916-upgrade-process-group-friction.md` is tracking-only and closes from those packets plus Phase 4 fresh-client/live evidence.

Branches covered by this phase:

- `codex/20260915-complete-open-work`
- `codex/20260915-completion-activity`
- `codex/20260915-completion-appserver`
- `codex/20260915-completion-baseline`
- `codex/20260915-completion-evaluation`
- `codex/20260915-completion-launcher`
- `codex/20260915-completion-upgrade`
- `codex/20260915-test-runtime-isolation`
- `codex/eval-t05-c-r2-incomplete`

For each candidate commit: inspect actual behavior on current main first, port the narrow missing behavior, run focused regression, then record whether the old commit is `salvaged`, `already-covered`, or `rejected`.

## Phase 4 — live acceptance and product decisions

Use `issues/open/20260908-live-acceptance-matrix.md` as the single live-only tracker. Fold implementation-complete live checks into it instead of keeping duplicate open issues.

Before running the matrix, finish these existing `doing` trackers without reimplementing their landed core:

- `issues/doing/20260916-local-agent-opencode-eperm.md`: Phase 0 rebuilt-runtime canaries, then done if regressions are green.
- `issues/doing/20260916-local-agent-reasoning-effort.md`: docs + clean full gate + rebuilt-runtime Codex effort canary only; `a8df54b` is already on `main`.
- `issues/doing/20260916-normal-session-ci-sandbox-friction.md`: confirm the existing host/CI gate remains green; repository-local `just sandboxed-check` is already the supported normal-session path.
- `issues/doing/20260916-package-manager-broker-coverage-and-state.md`: run current connected-runtime structured network acceptance; core broker is already on `main` (`69371fc`).
- `issues/doing/20260916-repo-scoped-github-account-selection.md`: repository-local extension is owned by the Git network packet; this tracker then needs only live multi-account evidence.
- `issues/doing/20260914-local-activity-viewer.md`, `issues/doing/20260908-07-client-safe-upgrade-reconnect.md`, and `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md`: close repository-local residuals from Phase 3 audits/children, then retain only the explicit live rows in this matrix.

At minimum re-run:

- OpenCode local agent on rebuilt runtime;
- Codex/Vite+ local agent on supported Linux/macOS environments;
- connected public tool-schema parity;
- package-manager structured network operations;
- repo-scoped GitHub workflow/tag operations without changing global `gh` active account;
- activity viewer / upgrade reconnect process-boundary E2E on supported OSes;
- CI host Linux sandbox acceptance.

A live path that is no longer intended to be supported must be explicitly removed from product scope and docs before closing the matrix.

## Tracker closure rule

The umbrella/tracker files are not implementation queues. Close them only when every referenced bounded child is done and any remaining live requirement has a row in the live matrix. This rule applies to:

- `issues/open/20260916-agent-mode-git-broker-gh-git-integration.md`;
- `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`;
- `issues/open/20260916-temote-managed-worktree-broker.md`;
- `issues/open/20260915-agent-development-network-access.md`;
- `issues/open/20260916-upgrade-process-group-friction.md`;
- `issues/open/20260911-gateway-deployment-target.md`.

Immediately before Phase 5, the only allowed open issue is the live matrix; once all supported live rows are resolved, move that matrix to `done/` too.

## Phase 5 — branch/worktree consolidation

Use `issues/polished/20260916-branch-cleanup-main-only.md` only after Phases 0-4 and the structured cleanup operations are complete. OpenCode may audit/disposition branches, but destructive branch/worktree deletion is executed by the parent Temote coordinator after the exact tip/worktree cleanliness proof.

Branch handling notes from the 2026-09-16 inventory:

- Already merged into `main`: `feat/nested-onepassword-secret-resolution`, `fix/20260915-session-list-backend-routing`, `fix/legacy-supervisor-bootstrap`. These are deletion candidates after worktree cleanliness checks.
- `agent/local-mcp-cloudflare` has no merge base with current `main`; never merge it wholesale. Review only its unique historical `a49beb1` capability against current Cloudflare/public HTTP implementation, then discard or port narrowly.
- `docs/20260910-developer-execution-broker` and `feat/developer-execution-broker-agent` are highly stale one-commit tails; review the one unique change each, then close/delete rather than merging old trees.
- completion branches are handled only by Phase 3 salvage packets.

Final proof:

```text
git status --short --branch
git branch --format='%(refname:short)'
git branch -r --format='%(refname:short)'
git worktree list --porcelain
```

Expected development branch set: `main` locally and `origin/main` remotely. Release tags such as `latest` are not branches and are unaffected.
