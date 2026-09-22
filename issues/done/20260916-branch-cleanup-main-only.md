# Final branch/worktree cleanup to main only

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Depends on: all prior roadmap phases complete

## Goal

Produce the final reviewed cleanup manifest for obsolete local/remote development branches and linked worktrees after their useful content has been integrated or explicitly rejected. OpenCode does the audit/disposition only; the parent Temote coordinator performs each destructive deletion after revalidation.

## Safety rules

- never reset/stash/delete another worker's dirty or untracked files;
- inspect each worktree status before removal;
- branch deletion is allowed only when the roadmap records `integrated`, `already-covered`, or `rejected` for its unique content;
- `agent/local-mcp-cloudflare` has no merge base with current main: review `a49beb1` capability only, never merge the tree;
- completion branches are deletable only after their salvage packets are done;
- do not delete release tags.

## Required proof

For every candidate, record branch name, local/remote tip SHA, worktree path, cleanliness, disposition (`integrated`, `already-covered`, or `rejected`), and the exact structured cleanup operation that the parent should use. Do not delete a branch/worktree in this packet.

The parent then re-reads current status/tips immediately before each deletion. After parent cleanup, record:

```text
git status --short --branch
git branch --format='%(refname:short)'
git branch -r --format='%(refname:short)'
git worktree list --porcelain
```

Expected development branches: local `main` and remote `origin/main` only.

## Final issue cleanup

Move completed implementation/tracker issues to `done/` and superseded/non-reproducible duplicates to `closed/`. The final actionable counts for `open/`, `doing/`, and `polished/` must be zero.

## 2026-09-22 disposition manifest (audit complete; parent executes deletions)

Recorded state at audit time (revalidate tips/cleanliness immediately before each op):

```text
main = 7b53bc3 (origin/main); local only other refs are the devin/* PR branches below;
linked worktree: /tmp/wt-helper-preflight (clean) on devin/1790097069-helper-generation-preflight.
```

### Branch dispositions

| Branch | Remote tip | Disposition | Exact operation (parent runs after revalidation) |
| --- | --- | --- | --- |
| `origin/feat/developer-execution-broker-agent` | `6e6b327` | integrated (PR #13 merged) | `git push origin --delete feat/developer-execution-broker-agent` |
| `origin/devin/1790085858-opencode-server-backend` | `ef3ada3` | integrated (PR #15 merged) | `git push origin --delete devin/1790085858-opencode-server-backend` |
| `origin/devin/1790087290-sandbox-capability-grants` | `fbe942c` | integrated (PR #16 merged) | `git push origin --delete devin/1790087290-sandbox-capability-grants` |
| `origin/devin/1790090364-macos-pinned-git` | `4d601d0` | integrated (PR #17 merged) | `git push origin --delete devin/1790090364-macos-pinned-git` |
| `origin/devin/1790094373-macos-pinned-git-identity` | `b946762` | rejected (PR #19 closed; superseded by merged PR #17 `9a7d5a5`) | `git push origin --delete devin/1790094373-macos-pinned-git-identity` + `git branch -D devin/1790094373-macos-pinned-git-identity` |
| `origin/codex/20260915-completion-activity` | `112b3cd` | already-covered (PR #27 audit: main strict superset) | `git push origin --delete codex/20260915-completion-activity` |
| `origin/codex/20260915-completion-evaluation` | `221e2ba` | integrated (evidence distilled, PR #30) | `git push origin --delete codex/20260915-completion-evaluation` |
| `origin/codex/20260915-complete-open-work` | `7503918` | already-covered (aggregate of all audited completion branches) | `git push origin --delete codex/20260915-complete-open-work` |
| `origin/codex/eval-t05-c-r2-incomplete` | `0054b96` | rejected (incomplete untested patch; superseded; SHA recorded in `docs/evaluations/completion-20260915-salvaged-evidence.md`) | `git push origin --delete codex/eval-t05-c-r2-incomplete` |
| `agent/local-mcp-cloudflare` | absent | not present locally or on origin — nothing to delete; safety note stands (`a49beb1` capability only, never merge) | none |
| `devin/1790093620-close-branch-delete-issue` (`5cdc4c7`) | — | in-flight PR #18 | delete branch after merge |
| `devin/1790094387-flake-issues` (`25e1bdb`) | — | in-flight PR #20 | delete branch after merge |
| `devin/1790095551-session-gc-plan-race` (`8d0942e`) | — | in-flight PR #21 | delete branch after merge |
| `devin/1790096148-packed-refs-mask-eacces` (`7ab29ac`) | — | in-flight PR #22 | delete branch after merge |
| `devin/1790097069-helper-generation-preflight` (`bc88dc0`) | — | in-flight PR #23 | delete branch after merge |
| `devin/1790098345-ingress-process-ownership` (`9f53c97`) | — | in-flight PR #24 | delete branch after merge |
| `devin/1790099111-upgrade-runtime-observation` (`c2cc21d`) | — | in-flight PR #25 | delete branch after merge |
| `devin/1790100104-agent-repo-triage-e2e` (`3558745`) | — | in-flight PR #26 | delete branch after merge |
| `devin/1790100398-activity-salvage-audit` (`10c39fe`) | — | in-flight PR #27 | delete branch after merge |
| `devin/1790100705-appserver-salvage-audit` (`9d7430d`) | — | in-flight PR #28 | delete branch after merge |
| `devin/1790100883-upgrade-salvage-audit` (`8acb110`) | — | in-flight PR #29 | delete branch after merge |
| `devin/1790101232-evidence-salvage` (`e48ce5c`) | — | in-flight PR #30 | delete branch after merge |
| `devin/<cleanup PR branch>` | — | in-flight (this manifest) | delete branch after merge |

Delete each merged local `devin/*` with `git branch -d <branch>`; for in-flight PRs the remote side is deleted automatically on merge (GitHub branch deletion) or via `git push origin --delete`.

### Worktree dispositions

| Worktree | Branch | Cleanliness | Disposition |
| --- | --- | --- | --- |
| `/tmp/wt-helper-preflight` | `devin/1790097069-helper-generation-preflight` | clean, no untracked changes | remove after PR #23 merges: `git worktree remove /tmp/wt-helper-preflight` then `git branch -d devin/1790097069-helper-generation-preflight` |

### Issue directory dispositions (executed in this packet)

- `doing/` → `done/`: `20260908-07-client-safe-upgrade-reconnect`, `20260908-08-codex-delegation-dogfood-and-app-server`, `20260914-local-activity-viewer`, `20260916-github-pr-broker`, `20260916-local-agent-opencode-eperm`, `20260916-local-agent-reasoning-effort` (docs gap closed in this packet), `20260916-normal-session-ci-sandbox-friction` (main CI green at `7b53bc3`), `20260916-package-manager-broker-coverage-and-state`, `20260916-repo-scoped-github-account-selection`, `20260922-opencode-run-cli-compatibility`. `20260922-macos-pinned-git-identity` was already under `done/` (superseded by merged PR #17).
- `open/` → `done/` umbrellas: `20260915-agent-development-network-access`, `20260916-agent-mode-git-broker-gh-git-integration`, `20260916-agent-mode-repository-triage-end-to-end`, `20260916-temote-managed-worktree-broker`, `20260916-upgrade-process-group-friction`.
- Remaining live evidence folded into `issues/open/20260908-live-acceptance-matrix.md` "Repository-completion live residuals" (12 rows) plus the two salvaged-evidence row annotations.
- `open/` keeps only: `20260908-live-acceptance-matrix.md` (the single allowed open tracker) and `20260922-agent-server-backends-cli-deprecation.md` (direction umbrella with pending Phase-3 implementation — stays open; its Phase-2 live parity evidence is the last residuals row).
- `polished/`: every remaining file is closed by its own merging PR (issues/done moves ride in PRs #18, #21-#29); this packet is the last audit-only one.

### Post-cleanup expected state

`git branch` shows only `main`; `git branch -r` shows `origin/HEAD` + `origin/main`; `git worktree list` shows only `/home/ubuntu/repos/temote-mcp`. Release tags untouched. Parent must re-run the recorded `git status/branch/worktree` proof commands immediately before executing the deletions.
