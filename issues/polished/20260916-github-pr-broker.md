# Add bounded host-side GitHub PR list/get/close operations

Status: polished
Model: opencode-go/deepseek-v4.1-flash
Parent: `issues/open/20260916-agent-mode-repository-triage-end-to-end.md`
Depends on: `20260916-git-shim-network-gh-git.md`

## Goal

Provide the minimum GitHub PR capability needed for repository triage without giving the local agent unrestricted GitHub network/API access.

## Scope

- configured current repository only;
- list open PR summaries with bounded fields;
- get one exact PR number;
- close one exact PR number;
- use the same repo-scoped managed credential selection contract as structured GitHub workflow operations;
- return fixed bounded errors and no raw token/header.

Do not implement arbitrary REST endpoints, merge, review, comment, release, issue mutation, or cross-repository access in this packet.

## Acceptance

Pure/PBT tests prove repository/PR-number containment, response bounding, secret non-echo, and no global `gh auth` mutation. Live close is Phase 4/triage fixture acceptance.
