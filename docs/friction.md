# Friction candidates and publication

The friction consumer is independent of coding-task execution and memory
checkpoints. It reads bounded observation batches and owner-local structured
friction events. Its source key includes host, session ID, session start time,
process ID and canonical scope; a same-name replacement receives a separate
checkpoint. A consumer ID and generation fence reject stale writers. Corrupt
observation input stops that source; event-store degradation is recorded in
the checkpoint and makes its candidates ineligible. A worker can continue
other sources after a source error.

The first deterministic episode detector groups repeated Temote
reconciliation observations by backend operation. One observation remains
`insufficient_evidence`; two or more exact references in the same fenced
instance become a `temote_friction` candidate. The classification contract
also represents `target_repository_bug`, `upstream_transient`, and
`known_existing_issue`, but those require independently verified evidence
or scoped issue identity. A transient or project bug is never silently
treated as a Temote defect. Candidates retain bounded structural references,
separate facts and hypotheses, recurrence, and acceptance criteria. Compacted
or missing support cannot be published.
An exact owner-local `issues/open/` index match can be attached through
`Consumer::mark_known_issue`; later scans preserve that classification and
only a scoped recurrence update can be handed to the publisher.

Publication is a separate durable outbox. It requires explicit owner export
opt-in, redaction review, unexpired Temote repository write authorization,
and a supported Temote-friction candidate. The generated Markdown uses fixed
text plus UUID/revision support handles; it excludes raw prompts, local paths,
private excerpts, and candidate-supplied prose. The outbox persists one
operation ID before an external effect. The publisher uses the existing typed
Codex task orchestration in a separately authorized Temote repository
session. It checks the retained exact operation receipt before the first
start. A crash gap marked `reconciling` or `uncertain` cannot start another
task without that receipt. No Git or GitHub operation is run directly by
Temote, and no PR is merged automatically.

The publication task uses a durable `Temote-Friction-Fingerprint` marker in
the issue and PR body and the deterministic `codex/friction/<fingerprint>` branch.
It reads the exact issue, branch, push, and PR state before each write; after
an uncertain write it reads back rather than repeating the write. Unsupported
or unavailable remotes leave reconciliation required.

Owner-only Unix CLI commands:

```text
temote-mcp friction scan --local <SESSION_ID> --consumer-id <ID> --generation <N>
temote friction scan-many --local --session-id <ID> --session-id <ID> --consumer-id <ID> --generation <N>
temote-mcp friction preview --local <FINGERPRINT>
temote-mcp friction known-issue --local <FINGERPRINT> --issue-ref issues/open/<NAME>.md --consumer-id <ID> --generation <N>
temote-mcp friction publish --local <FINGERPRINT> --publication-session-id <ID> --temote-repo-root <PATH> --model <MODEL> --effort <EFFORT> --authorization-expires-at <UNIX_SECONDS> --export-opt-in --redaction-approved --temote-repo-write
temote-mcp friction record-pr --local <FINGERPRINT> --pr-url https://github.com/f4ah6o/temote-mcp/pull/<N> --operator-attested
temote-mcp friction reconcile-pr --local <FINGERPRINT>
```

`scan` reads one bounded batch from a current managed session and prints
structural candidate metadata. `scan-many` takes 1–16 distinct sources and
loads and consumes each independently. A corrupt or missing source produces a
structural `degraded` result while healthy sources advance their own checkpoints;
no raw errors or excerpts are printed. `preview` is read-only and never prepares an
outbox. `publish` binds the outbox to the exact publication session instance,
model, and effort, then delegates a bounded typed task using the durable
operation ID. It requires a normal session scoped exactly to the authorized
Temote root. The accepted task receipt is not evidence of a completed coding
task, verification PASS, delivery, or an open PR. The operator may later
record a PR URL after review. This is stored as `pr_attested`, distinct from
an observed open PR. `reconcile-pr` uses a fixed read-only delegated Codex
observer in the original full publication session scope, model, and effort.
Its stable operation UUID, attempted dispatch, and task receipt are durable;
later calls use exact TaskGet and retained receipt only. Missing or unknown
receipts cannot trigger a fresh dispatch. The observer checks the exact Temote
repository, fingerprint marker, local issue identity, deterministic branch,
head/base revisions, and one open unmerged matching PR. Only a completed
native structured report satisfying those checks records `pr_observed`. A
caller-supplied PR URL alone never does. Live GitHub publication remains a
separate owner-run integration gate.

The observer also reconciles a retained failed or interrupted publication task
after a possible branch, push, or PR effect. The task must be quiescent, and
the original full permission and directory scope must still match. Legacy
outboxes without that fence remain uncertain. An observed open PR is a remote
fact; it does not turn failed execution into verification or delivery PASS.

The worker scheduler and direct hook adapters are still separate integration
work. Candidate or publication failures do not change the target coding-task
result. A candidate's support is evidence, not permission to export it.
