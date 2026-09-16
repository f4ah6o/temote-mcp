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
