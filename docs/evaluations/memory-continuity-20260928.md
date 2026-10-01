# Memory continuity qualification — 2026-09-28

## Starting state and packet boundary

Repository: `f4ah6o/temote-mcp`.
The original checkout is `/home/hirohito-fujita/src/temote-mcp-df`, a colocated
jj/Git workspace. Git HEAD was detached at `9f79a3338c8acfe00791c1f4e390be22715b2525`;
jj's working revision was `ca65669ee7ed2ff34be14e4d4f71f491428429c0` on
`codex/20260928-bare-clone`, merging main `e0d6c767`. Git's index contained
unmerged entries for three files and earlier Fabric deployment/metadata work.
The exact Git index/worktree diffs, jj diff and status, original binary and
Linux sandbox helper are retained owner-locally under the original checkout's
ignored `dogfood/runs/memory-20260928-start/`. No existing change was discarded.

This packet uses a separate Git worktree at
`/home/hirohito-fujita/src/temote-memory-plane`, branch
`codex/20260928-memory-plane`, initially without upstream. Its clean base is
`e0d6c7674f4d8d43999c77979687ca37cdd04ea7`, which already contains the Fabric
deployment and authoritative generated metadata implementation. PR #78 and its
bare-repository work remain separate.

Code inspection confirmed O1/O2 and C0/C1. Cloud context only exposed a binding
readiness helper; C2-C5 were not implemented. The canonical contract is
`issues/done/20260926-cloud-observation-knowledge-plane.md`.

## Baseline

An immutable Git archive of the base was built independently of candidate
edits. Both baseline binaries are preserved in ignored
`dogfood/runs/memory-20260928/baseline-bin/`; the exact source is retained in
`baseline-full-source/`. `baseline-identity.json` records their separate SHA-256
identities and explicitly marks knowledge as `not_implemented`.
The binary SHA-256 values are `temote-mcp`
`e5d1e7131c7f4df6dc1f04cf1faa27677a09a2888efb8014d9eab65e469d43d2` and
`temote-linux-sandbox`
`744c1457fed0a1b924da1901e29dee3f5289035d61dc8afd6d8b71069c6da108`. Scenario
`memory-continuity` revision 1 fingerprint: `d927d6f6fd369423a7516afcd7eb82662e60f28963f76b0c46c0d43139400d54`.

- `cargo build --locked` against the archived manifest: pass.
- `cargo test --locked -- --test-threads=1` against that manifest: pass;
  pre-existing ignored gates remain unrun.
- Archived gateway `node --test`: pass.
- `python3 -m dogfood validate` and `python3 -m unittest dogfood.test_protocol`:
  pass (13 protocol tests).
- Baseline `temote-mcp doctor`: zero failures, two provider warnings.

The lack of baseline knowledge is not a successful empty projection.

## Shared implementation contract

Host execution retains authority and the local journal remains the recovery
source. Observation synchronization uses the existing authenticated host
channel. Repository keys are stable forge identities; unresolved identity must
not become repository knowledge. Default replication excludes free-text
previews; opted-in bounded previews remain untrusted data.

Fabric resolves authorized D1 replicas before host routing. Only its generated
context schemas add repository-scoped requests; host/local context and unrelated
session tools keep their session requirements. Worker output must carry checked
raw-observation support. Execution completion is not implementation verification.

Projection commits use D1 transaction fencing, and an ingest transaction records
durable pending work. Queue delivery is a wake-up signal; a scheduled sweep
recovers delivery failure without a later observation. Config generations must
be monotonic to prevent old consumers from reverting a new projection.

## Environment gates

Available implementation-agent API profiles were used as requested:
`gpt-6-luna` / `max` for implementation and tests, `gpt-6-sol` / `high` for
authority and concurrency design review. These selectors are separate from the
runtime extractor model.

The existing OpenCode Go credential was present; an independent bounded,
non-secret `glm-5.3-flash` transport probe returned HTTP 200, one choice, terminal
`stop`, and content. This is transport proof only, not knowledge qualification.
No API key, request body or response body is retained in this report.

Read-only Cloudflare inspection confirmed the existing `temote-mcp-gateway`
deployment, its routing DO bindings and observation D1
`83e896c8-393e-41b8-b2a7-f686a8a108fc`. No Temote memory Queue or extractor
binding was configured. This packet has not changed those remote resources.

## Candidate evidence

The C2–C5 feature behavior and live-model memory-continuity run were qualified
at source head `83a533aa75d9001fa95aa9fcfbef78951123d642`. Follow-up commit
`efea482ec6dda2e547bdec9bfa12a3e55f7f5752` makes a narrow logging-privacy
correction described below; the model scenario was not rerun on its binary.
The live scenario passed all nine assertions and the final documentation/issue
diff received independent approval. Cloudflare remote qualification remains
**NOT RUN**. Fixture success does not substitute for live-model qualification.

### Completed packet checks before upstream integration

- C2 independent review: three passes / seven aspects, **approve** on immutable
  patch SHA-256 `bd19e9d28d6daac0307ccba35655aaea71c1c225e47b26bc8b4bd698780b07f4`.
  Six-file manifest and copied-code adversarial results are retained under
  `dogfood/runs/memory-20260928/c2-review-third.*`.
  It covers restart/backoff, lost-ACK replay, retained terminal records, gap
  accounting, strict repository identities, and healthy-source progress after
  unreadable/ahead-of-journal sources. Earlier review findings were fixed and
  rechecked; implementation self-reports were not used as review approval.
- `cargo check --no-default-features --all-targets`: pass after removing the
  accidental dependency on the optional URL crate.
- Candidate `cargo test --locked -- --test-threads=1`, format and full-target
  clippy checks: pass after the C2 fixes.
- Complete gateway suite: 163/163 pass at the pre-integration snapshot. Later
  frozen targeted suites additionally cover all 11 workerd recovery cases,
  29 extractor/security assertions, and 7 simultaneous authorized changes.
- Python protocol/scenario checks: 26/26 pass; all seven scenarios validate.
- `just linux-sandbox-acceptance`: pass (18 Linux runtime tests and the explicit
  ignored pinned-workspace descriptor host test).
- `npm run deploy:dry-run` in `gateway/`: pass. This builds the Worker with its
  declared bindings; it does not provision Queue/D1 or deploy to Cloudflare.
- Authoritative tool generation: two successful `just generate-tools` runs
  produced identical bytes for all three public artifacts;
  `just check-generated` passed.
- Wrangler's actual local D1 migration runner applied 0001–0003 successfully
  in an isolated persistence directory; the target was explicitly `--local`.

### Known bounded operation limits

Automatic synchronization follows observations appended by normal Temote
operations, including terminal task reconciliation. This packet does not add
an independent backend poller that discovers completion when every client has
stopped requesting task state.

An ACK-loss retry assumes the same preview policy. Changing that policy before
the immutable batch is acknowledged can produce `conflicting_replay`;
acknowledged previews cannot be undisclosed. The documented recovery preserves
the durable source and requires an explicitly authorized policy choice. Source
gaps remain partial after later records synchronize.

### Later local qualification and independent review

- The final authority review approved the 47-file integrated patch at head
  0f43abee9cbfcca1cd30796acb646cc97bee542c, based on
  48596907b92b09d706181c3e6b886052e6bf8c16. Its manifest matched the reviewed
  head and working tree after independent tests.
- The provider/envelope review returned **approve-with-nits** for the 48-file
  manifest at target dae7a1c6bfb6fc45092bf7b602ed31f726f7c8ef. This is not a
  final review of the ending candidate HEAD.
- cargo fmt --all -- --check, cargo clippy --all-targets -- -D warnings,
  cargo check --no-default-features --all-targets, and
  cargo test --locked -- --test-threads=1 completed successfully in the
  recorded Rust runs. The final stored Rust test log reports 835 unit tests
  passed and 1 ignored, with no failures; integration test binaries also report
  no failures.
- The earlier offline Gateway log reported 197/197. At frozen head
  `57a75293660853b4ed88e62ed93ca903162a3b38`, the retained Node 22.23.3 local
  CI-compatible run (not GitHub CI) reports 201/201 tests passing, 0
  failed/skipped, exit 0 (log SHA-256
  `289c284893e73ee158de6224aeec28cb15ed00bbab236a6f2877b342d53c6423`). The
  da449 offline-harness review subset reported 11/11 passing. The terminal
  handshake suite at 57a7529 reports 15/15 passing, exit 0 (SHA-256
  `cde5499150ca4f802c79b9e123a59a1cb0a6cb63eb7a9cdcc78dfb4acf152b26`).
- The real workerd/D1 integration suite covers migration preservation,
  transaction rollback and fencing, concurrent claims, Queue ACK loss,
  DB-commit retry, restart recovery, scheduled outbox repair, poison-attempt
  bounds, and projection rebuild. The D1 migration runner applied migrations
  0001–0003 to an isolated **local** D1 database (22, 5, and 37 SQL commands
  respectively). No remote D1 migration was run.
- python3 -m dogfood validate validates eight checked-in scenario IDs.
  This validates scenario schemas; it does not run all scenarios.
- Two `just generate-tools` runs produced identical bytes for all three public
  artifacts and `just check-generated` passed. The gateway deploy dry-run built
  the Worker with declared bindings; this did not deploy or verify a remote
  target. The worker statement bound is at most 602 D1 statements per
  repository invocation, within the Paid 1,000-statement limit; Free's
  50-statement worst-case limit is not qualified.
- At frozen head `57a75293660853b4ed88e62ed93ca903162a3b38`,
  `python3 -m unittest dogfood.test_protocol dogfood.test_memory_continuity -q`
  completed with 35/35 passing. The separate SQLite schema check
  `python3 gateway/test/cloud_observation_schema_sqlite.py` completed with
  10/10 passing; it is not a D1 runtime test. The actual workerd/D1 worker suite
  has 13 recovery tests in the recorded review run. These are local gates, not
  live-scenario assertions.
- The local D1 migration, generator, Rust, dry-run, and test evidence above
  does not establish a Cloudflare remote deployment or remote qualification.
- At final source head `83a533aa75d9001fa95aa9fcfbef78951123d642`, the full
  Gateway suite passed 238/238 with no failures or skips on Node 24.21.0 and
  again on Node 22.23.3. The Node 22 run is a local CI-compatible runtime
  check, not GitHub CI (summary SHA-256
  `6a519bd432fc5ed8e2250e412c324bd41d8bcc0505be9405c3df6f98897079a6`, log
  SHA-256 `8d3646023209c0c59ace898ed5a2fcc4c939e7b0803400922105516b94bc596f`).
  The Node 24 log SHA-256 is
  `beb3d1a786d4e36bb1e3d10c52c57e3faa9b8821f41fbea74fbe50afb4aaf9e1`.
  Python dogfood/protocol tests passed 37/37 (log SHA-256
  `48199581b9a4cfae1882b79f923885ce8704fda63ad490afc7a1db2fe837da54`).
  Formatting, scenario validation (eight schemas), diff check, and deploy
  dry-run passed; Rust code has not changed since the recorded 835-test run.
- An independent high-effort authority review approved the Policy5 source at
  `83a533aa75d9001fa95aa9fcfbef78951123d642`; review artifact SHA-256 is
  `bd8dcce0b6c57c2fda8bca067cd354741a7f2a3ad01a2200ea42c570902eb21a`. It
  matched the reviewed patch, confirmed the earlier 46 reviewed files were
  unchanged, and passed 87 Gateway checks including actual D1 coverage plus
  37 Python tests. This is an independent code review, not the final review of
  the documentation diff recorded below.

### Baseline and live memory-continuity scenario

The baseline scenario artifact has run ID
`4b2bffbc-72e8-4c30-a50a-648a3d1defe6` and is retained under
`dogfood/runs/memory-continuity-baseline-348f3eae-9525-4c5e-a718-a02fcdb1cc7a/`.
It used scenario `memory-continuity` revision 1 and the fingerprint recorded
above. Its outcome is `not_implemented`; its manifest has no extractor
configuration and live synthesis is NOT RUN. The baseline's absent knowledge
is not a passing empty result.

Candidate runs `ae1e618f-ba5b-4645-8fe1-e75ff638dd70`,
`d29a9fe6-287d-41c4-b25e-9bdc6421be05`, and
`dced0085-3c46-4b57-b7ae-102dc2feeb3a` ended blocked. The last failed with
`invalid_support` after its configured retry limit. Subsequent quote-prompt
work reconstructed the provider request from the same 16 observations using
the corrected prompt; it did not reuse the original provider response. That
representative replay accepted zero items in 4.4 seconds. It is a diagnostic,
not a committed live projection or scenario qualification.

The quote prompt patch was independently reviewed and approved at head
`2404f6429bfe4de796745beee8807b77f4285a69`; patch SHA-256 is
`046839c74c224fa9af7e7a1112fbb9bfab65c2e77eb2d01402752551dfd45f14`. The
review artifact SHA-256 is
`84c5804817b9e4346d6ac7ea37aa2bf63a84658bd1df3b3df726c441ee71a1f4`.
The review checked 58 independent gateway checks and 32 Python tests. Its
bounded prompt accounting was 3,559 bytes; oversized-input rejection made
zero timer and provider calls (the normal provider timeout remains enabled).
The later offline-harness patch was independently approved at `da4496bcb6455d04d8ce4dbd2accebe211c57695`; its two-file patch SHA-256 is
`1eb9b5b67497a1e2a58ec734dded2527f4d5943db87a6bf10910fda8de3d54a8`.
That review found the earlier 46 approved files unchanged and ran 11/11 scoped
tests. It verifies the owned child process exits before host absence is used as
proof; it does not by itself verify repository context retrieval.

An earlier live attempt `dd37ca27-9ffd-4c02-86ed-583b0166fd34` failed the
host-offline step: after SIGTERM, the dedicated host remained visible through
the 15-second absence wait. It produced no final D1 artifact, Task B, or
offline proof; it is NOT QUALIFIED. The subsequent SIGINT/offline-proof
harness correction passed its independent review and scoped tests.

The live attempt `021a8200-789d-4419-b55a-bf2c5ad58f15` passed 7/9 scenario
assertions; unresolved-context and cross-head context assertions failed. The
subsequent sessionless-reader change fixed the cross-head path. Policy4 run
`85e294a0-817d-4d6d-a238-e2ad06262250` passed 8/9, with only
`task_a_unresolved_surfaced` failing. Its raw provider response was not
retained, so this failure does not establish that the model omitted the item.
Review found a scenario acceptance-helper bug: the helper accepted only
`current` items, while the contract permits supported unresolved items.
Policy5 fixed that acceptance path and tightened declaration parsing; prior
outcomes remain historical and are not reclassified.

An earlier live attempt was
`memory-continuity-candidate-live-final-da4496b-20260928`. Its safe failure
manifest records scenario revision 1, the same fingerprint as baseline, source
head `da4496bcb6455d04d8ce4dbd2accebe211c57695`, and an empty working diff at
run time. The coding backend was `codex` with `gpt-5.6-luna` / `max`; the live
extractor was `glm-5.3-flash` with reasoning effort `low`, timeout 60,000 ms,
input budget 32,768 bytes, content output budget 8,192 bytes, response-envelope
budget 32,768 bytes, three attempts, and batch size 16. Task A terminal
observation and source synchronization passed; its projection predicate passed,
and the dedicated source-host offline proof passed. Public context assertions
were NOT RUN. Task B became ready, but its projection predicate did not
complete before the wait timed out with stable code `PROJECTION_B_TIMEOUT`. At
the diagnostic snapshot Task B had zero terminal observations; `memory_runs`
were completed and showed no worker error. The repository-scoped pending count
was five and included the still-running Task B observation, so this early wait
timeout does not establish an outbox or worker defect. Queue replay and full
scenario assertions were NOT RUN. The run is blocked and live synthesis is
**NOT QUALIFIED**. Partial D1 knowledge rows do not establish the omitted assertions.

The frozen candidate head is
`57a75293660853b4ed88e62ed93ca903162a3b38`. Its nine-file terminal-harness
delta was independently approved; patch SHA-256 is
`50d7da83026cdf09625fa29a6d4c0b304d67a31744d2f495b52e3d43a3f48dcd`, and
review artifact SHA-256 is
`11da0d93e2c5b41b241bf1ef6bb67ca175299bdfaf9b17e91673690ddf11694b`. The
review matched all nine patch files to HEAD, retained the prior 44 reviewed
files unchanged, and independently passed 15 Gateway tests, 35 Python tests,
offline `npm ci` (40 packages) and fresh Miniflare/esbuild imports. A separate
child-process probe exited in 206 ms after the driver completed despite a long
task budget. At that review point no live trial had run on the head; the later
Policy5 run below supersedes that interim status. No raw task text, provider
request/response, or secret is included here.

### Final Policy5 live candidate

Candidate run `6f691adf-811c-46e1-9b1d-0a3e8ef90802` used source head
`83a533aa75d9001fa95aa9fcfbef78951123d642`, scenario
`memory-continuity` revision 1, fingerprint
`d927d6f6fd369423a7516afcd7eb82662e60f28963f76b0c46c0d43139400d54`, and
input SHA-256
`4958baa4922c80a8165ebafcaaa6b9b3d5e2fe07ce2621ea1bc45ba489a837d7`, matching
the baseline scenario and input. The candidate scenario artifact SHA-256 is
`607850dfb551d3a4a087ef8da1d6b267fb72e0693c961cf28bc3bd683623b780`, the
safe summary SHA-256 is
`cb8da12486031f79f7092e8757eaf8495e6efa9c8f6a452071d5643195fbbcd5`, and the
Queue replay artifact SHA-256 is
`7dc86fefbb64147883a37ba55a768d63dffd6968d6c6e994de22b30bb0417840`.

The live task used backend `codex`, model `gpt-5.6-luna`, effort `max`. The
extractor made a real OpenCode Go `glm-5.3-flash` request at reasoning effort
`low`, with 60,000 ms timeout, 32,768-byte input budget, 8,192-byte knowledge
content budget, 32,768-byte response-envelope budget, three attempts, and batch
size 16. The baseline used the same scenario/input and task selectors, but its
extractor configuration and timing were not recorded; it had no extractor
configuration and synthesis was NOT RUN. No comparison of extractor settings,
runtime, or task friction is claimed.

All nine live assertions passed: Task A's current repository constraint and
supported unresolved item were surfaced with support; another head retrieved
repository context; the context stayed within budget; the same context was available with source host A
offline; Task B's explicit constraint change became current and superseded the
old constraint; Queue replay added no knowledge/support/supersession/run or
checkpoint growth; and an unrelated repository remained isolated. The harness
used two dedicated local Temote hosts; local O1 journals ended at 114/114 and
173/173 with `degraded=false`. The ordinary tasks contained no memory-saving
or summarization instruction. The terminal update was recorded when ordinary
task polling observed completion, then synchronized automatically. This does
not add backend-autonomous polling when no client requests task state.

Replay counts were dispatch 34→35, knowledge 10→10, support 31→31,
supersession 2→2, memory runs 34→34, and checkpoint 287→287. These counts are
from the local harness D1 replay proof. Its ephemeral D1 was disposed after
the run, so it did not preserve cloud Worker last-success/lag or per-kind
counts; no such values are inferred here. The Cloudflare read-only inventory
found the existing Worker and observation D1 readable (HTTP 200), but no
`MEMORY_ENABLED` binding, observed `MEMORY_API_KEY`, or `temote-memory` Queue.
Read-only Cloudflare access was available and no remote mutation was made.
Remote qualification remains **NOT RUN** because an approved rollout scope and
dedicated remote test client/host credentials were not established.

Policy5 performs the real extractor call and validates the complete response
before deterministically projecting exact constraint and unresolved clauses
from the initial contiguous, unquoted, unfenced owner-declaration block.
Repository-level constraints can become `current`; owner-declared unresolved
items are stored as `supported` and are returned by the resolver. This direct
declaration projection does not rescue provider failure or invalid output.
Additional model-derived proposals remain subject to support, scope, and
schema validation. The live result qualifies the tested scenario; it does not
claim a general model accuracy rate.

| Required comparison gate | Result | Evidence |
| --- | --- | --- |
| issue_completion | Pass (local C2–C5 scope) | Child issue C2–C5 and parent O3–O4 progress are updated with evidence; optional C6/R2 remains open. |
| memory_pipeline | Pass | Workerd/D1/Queue recovery tests and live model synthesis completed; the live Queue replay was idempotent. |
| knowledge_quality | Pass | Live assertions checked supported/current constraints, unresolved context, supersession, support, and bounded output. |
| head_switch | Pass | A separate head resolved repository context and saw Task B's changed policy. |
| offline_host | Pass | Repository context assertions passed after the dedicated source host was stopped. |
| tenancy | Pass | The live unrelated-repository isolation assertion passed. |
| retry_recovery | Pass | D1 recovery tests and live Queue replay/no-growth assertion passed. |
| tests | Pass (local) | Rust 835 passed/1 ignored; Gateway 238/238 on Node 24 and 22; Python 37/37; scenario validation, format, diff check, dry-run, and independent review passed. |
| final_diff | Pass | Independent final-docs review approved the frozen diff in three passes across seven aspects; artifact SHA-256 `f71fd61b989a584d931f4d2845ed6149e760aff2cfb008cba42fd610d1b21618`. |

The comparison gate map is the nine-key owner-local file
`dogfood/runs/memory-20260928/memory-continuity-policy5-independent-gates.json`;
its nine values are all `pass`. It deliberately excludes
Cloudflare remote qualification, which has its own owner-local gate artifact.

Local implementation qualification and live-model scenario qualification
pass. After the final-diff gate passed, the dogfood comparison exited 0 with
`qualification: qualified`. The comparison report is
`dogfood/runs/memory-continuity-comparison-6f691adf-811c-46e1-9b1d-0a3e8ef90802.json`,
SHA-256 `a242b51ddb93cba514b9fbf7ce579ca64606e9ec2f6f691b1d983fd7201f1e78`;
all 18 scenario and external gate entries passed, with the same input,
selectors, identified binaries, and an honest `not_implemented` baseline.
Cloudflare remote qualification remains NOT RUN. No claim of reduced task
friction or improved task success is made; the baseline had no knowledge
implementation and did not record extractor configuration or comparable
timing.

### Cloudflare remote gate

**NOT RUN.** The read-only inventory confirmed that the existing Worker and
observation D1 were reachable, but did not show a memory enablement binding, a
configured `MEMORY_API_KEY`, or the `temote-memory` Queue. Read-only Cloudflare
access was available; an approved rollout scope and dedicated remote test
client/host credentials were not established. No remote resource was changed.
The separate gate result is retained in
`dogfood/runs/memory-20260928/memory-continuity-policy5-cloudflare-remote-gate.json`.

### PR81 CodeQL snapshot and logging correction

At PR head `0fed`, nine check results were terminal: eight succeeded and the
aggregate CodeQL check failed on high finding #197,
`rust/cleartext-logging`, at `src/gateway.rs:800`, where ordinary ACK
diagnostics wrote `session_id`. All individual CodeQL analyses, Gateway, Linux
and macOS Rust, and release-plan checks succeeded; the aggregate CodeQL check
was the only failure. This observed failure remains part of the history.

Commit `efea482ec6dda2e547bdec9bfa12a3e55f7f5752` removes `session_id` only
from ordinary ACK stderr diagnostics. It retains records, revisions, cursors,
and status reporting, with no suppression; authorization, ACK cursors, sync,
model calls, D1 writes, and knowledge selection are unchanged. An independent
review approved the exact fix in three passes across seven aspects; artifact
SHA-256 `785d0241c1deb754425c6ddb7b0fdfaa717a3fb3c95a1c23841f2a78a8d1f5ed`.

Post-fix local Rust checks completed: the main suite passed 835 tests, the
library suite passed 151, and integration binaries passed. Formatting, clippy,
no-default-features checks, diff check, and 14 scoped Gateway tests passed.
Owner-local summaries and logs are retained with mode `0600` under
`dogfood/runs/memory-20260928/codeql-privacy-checks/`.

The qualified live run remains run
`6f691adf-811c-46e1-9b1d-0a3e8ef90802` at source head `83a533aa75d9001fa95aa9fcfbef78951123d642`.
Its preserved Policy5 binary and Linux helper are retained under
`dogfood/runs/memory-20260928/policy5-live-bin/`. The binary and helper built
after the logging correction are separate files under
`dogfood/runs/memory-20260928/codeql-privacy-bin/`, identified by
`codeql-privacy-binary-identity.json`; no live model run used them. The
previously qualified comparison therefore remains evidence for the 83a533aa
source behavior and does not claim a model rerun on efea482.

The owner artifact for the historical 0fed check snapshot is
`dogfood/runs/memory-20260928/github-pr81-checks-0fed9b8.json` (SHA-256
`e9c74d393f06251fa946e864b90c2af5d34fe9265f80b37048b6e05f339b1330`).
Current-tip CI results are represented by PR81's check statuses; this
historical evaluation does not treat the 0fed artifact as the final-tip result.

### Adapter v3 validation follow-up

The subsequent extractor change advances the adapter version from 2 to 3;
prompt version 4 and policy version 5 are unchanged. A present provider
`finish_reason` is accepted only when it is `stop`; any other present value
fails without advancing the checkpoint. A genuinely absent field remains
allowed. A repository with no owner-scoped replication source records is
reported partial and stale. A recorded source with zero observation rows is
evaluated from its cursor and gap state; a healthy zero high-water mark can be
fresh. Worker `ready_empty` describes extraction state separately from source
freshness.

Producer-version isolation prevents completed runs under the prior adapter
from being reused as v3 work. Deploying v3 requires a monotonic
`MEMORY_PROJECTION_GENERATION` increase to rebuild from retained observations.
The rebuild does not alter raw observations; the previous valid projection
remains readable and is marked stale until the new projection is published.
This follow-up has not had a new real-model scenario run or Cloudflare remote
deployment. The qualified live run and comparison above remain tied to source
head `83a533aa75d9001fa95aa9fcfbef78951123d642` and adapter v2.

Local verification for the adapter-v3 follow-up working tree, based on source
HEAD `b8173594666a1c4002126a75cde1dfd015bb5b73`, completed as follows:

- `npm test` in `gateway/` passed 251/251, including new context coverage and
  the content-filter test against workerd/D1. The focused cloud-context
  resolver run passed 23/23, including actual workerd/D1 coverage.
- `cargo test` passed 835 main tests, 151
  library tests, and integration test binaries. `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, and
  `cargo check --no-default-features --all-targets` passed.
- `python3 -m unittest discover -t . -s dogfood -p 'test_*.py' -q`
  passed 37/37; `python3 -m dogfood validate` validated eight scenarios;
  `just check-generated`, `npm run deploy:dry-run` in `gateway/`, and
  `git diff --check` passed.
- An earlier full Gateway attempt failed on a syntax error in a new test; its
  log was retained. The later full run completed successfully.

These are local implementation checks, not an adapter-v3 live-model or
Cloudflare remote qualification. Independent review of this follow-up diff
was still pending at this report snapshot.

### Policy v6 / adapter v4 validation follow-up

Independent review of the memory plane found two projection defects on the
reviewed head `bc6fcc6b2bff1cde6fb8e7783ad5f2631122cf64`. The fix advances
the extraction policy version from 5 to 6 and the adapter version from 3
to 4; prompt version 4 and producer schema version 1 are unchanged.

- Policy reinstatement (A→B→A): re-stating policy text that an in-batch
  change already superseded was folded into the dead staged item as extra
  support, so the reinstatement never received its own state transition
  and the intermediate policy stayed current. `prepareProjection` now
  merges support only into a live staged item, excludes rows superseded
  earlier in the same batch from live-row matching and conflict
  targeting, and `canonicalFirstUnion` opens a new canonical item for
  every `changed` clause instead of merging it into the earlier
  same-text group. Same-batch `[A,B,A]` and the `[A]→[B]→[A]`,
  `[A]→[B,A]`, and `[A,B]→[A]` splits all converge on the last-adopted
  policy through the fixture adapter and the real `openai_compatible`
  adapter path, verified on workerd/D1 with the public `context_resolve`
  surface.
- Supersession edge aliasing: an unchanged restatement inside a batch
  reuses the committed row (`reuseKnowledgeId`), so the conflict list
  held both the database row and its staged alias and emitted the same
  `knowledge_supersession` edge twice — a primary-key rejection that
  wedged the run with `commit_rejected` and stalled the checkpoint. The
  supersession id list is now deduplicated so one row yields at most
  one edge, while every conflict — including the staged reuse alias —
  still participates in supersession authorization (dropping it would
  have let a same-source change claim ignore earlier cross-host
  support). The same path protects the spliced summary channel. This
  defect predated the fixes above and was found by the independent
  re-review on both base and head.
- Input budget deferral: `boundedInput` truncated or emptied an
  observation's sanitized preview to fit the remaining batch window and
  still consumed its `cloud_seq`, so a policy whose metadata fit but
  whose content did not was lost while the checkpoint advanced past it.
  An observation is now selected only when its complete sanitized
  extraction input — preview, canonical clauses, change predecessors,
  and JSON escaping inside the serialized `{observations:...}` payload —
  fits the remaining window; otherwise it defers to the next batch. An
  observation that can never fit follows the existing bounded
  failure/retry/diagnostic contract without advancing the checkpoint.

Producer-version isolation prevents completed runs under the prior
adapter/policy pair from being reused as v6/v4 work. Deploying these
changes requires a monotonic `MEMORY_PROJECTION_GENERATION` increase so
the projection is rebuilt from retained observations; queue redelivery
alone does not repair gaps behind an already-advanced checkpoint. The
rebuild does not alter raw observations; the previous valid projection
remains readable and is marked stale until the new projection is
published.

Local verification for this follow-up, based on source HEAD `bc6fcc6`
plus the fix commits `008bc3d` (reinstatement) and `62b98b2` (input
budget), and the re-review fix commit for supersession-edge aliasing:

- Pre-fix reproduction captured the findings: the new reinstatement
  suite failed 4/7 cases and the new input-budget suite failed 7/11
  cases on the reviewed head; the same suites pass after the fix. The
  restate-then-change wedge case added during re-review fails on the
  reviewed head and on the first fix head, and passes after the
  conflict dedupe (8/8 reinstatement, 11/11 input-budget cases).
- `npm test` in `gateway/` passed 269/269 including real workerd/D1
  coverage; `cargo fmt --all -- --check`, `cargo clippy --all-targets --
  -D warnings`, `cargo check --no-default-features --all-targets`, the
  generated-contract check (`just check-generated`), `npm run
  deploy:dry-run`, and `git diff --check` passed.
- These are local implementation checks, not a v6/v4 live-model or
  Cloudflare remote qualification. An independent non-implementer
  re-review of `bc6fcc6..HEAD` verified both fixes, found the
  supersession-edge aliasing wedge above (fixed in the same head), and
  reported no remaining blocking findings.

### Policy v7 / adapter v5 validation follow-up

A second independent re-review of the v6/v4 head `f90ada6` found two
remaining defects, both confirmed by new real workerd/D1 regression
tests before the fix and resolved after it:

- Same-text unconfirmed absorption (R1, P1): an unchanged restatement
  (`policy is: TOML`) staged as `supported` absorbed a later explicit
  change directive for the same text (`policy has changed: TOML`,
  predecessor JSON) — the merge-and-continue gate at
  `prior.status !== "superseded"` never reached supersession
  authorization, so the change collapsed into corroborating support
  and TOML never promoted. When the re-occurrence carries an explicit
  change directive the staged item is now re-evaluated as a new
  transition: it is spliced from the commit list, tracked in
  `replacedStaged`, and re-runs the same-text match so it reuses the
  staged knowledge id and accumulated support — committed rows still
  UPDATE in place (`reuseKnowledgeId: Boolean(matching)`), staged
  reuse INSERTs a fresh row. Integrating this exposed a further
  pending-support loss: a staged item spliced while a committed row
  also matched (`matching` path) dropped its uncommitted support rows
  because the persist capacity counted the item's full support list
  against already-committed rows. The persist basis now unions the
  committed row's stored support with the spliced item's accumulated
  and pending support (deduplicated by cloud_seq/role), and only
  committed rows count against the persist capacity; without it a
  batch split at the bound persisted support only up to the batch
  edge plus the last observation (`[1..6, 12]` instead of `[1..12]`),
  leaving `support_incomplete` and the same-source ordering veto to
  reject the later legitimate change. The independent review of this
  follow-up diff then found two related batch-boundary divergences,
  both reproduced as a real workerd/D1 regression before fixing:
  an unauthorized contested directive could demote a staged `current`
  item to `supported` inside one batch (committed rows keep `current`
  through the sticky `CASE WHEN status='current'` UPDATE, so split
  orders converged but same-batch did not), and the re-evaluated item's
  `valid_from` was taken from the incoming directive support instead of
  the earliest merged support carried by the spliced item. The
  unauthorized branch now preserves `current` the same way persisted
  rows do, and `valid_from` carries the staged item's `validFrom` when
  reusing its identity; a cross-session contested-neighbor regression
  test covers both the same-batch and split orders.
- Deterministic item/support bound at admission (R2, P2):
  `boundedInput` bounded bytes only, so a byte-fitting backlog whose
  canonical items exceeded `MAX_EXTRACTED_ITEMS` (13 changed-clause
  instructions, 11141 bytes inside a 12288-byte window) wedged the
  provider path on `projection_too_large` through fixed-range retry,
  while the fixture path silently returned 12 items and the worker
  advanced the checkpoint past the 13th observation. `boundedInput`
  now also simulates the deterministic bounds as it grows the
  selection — canonical group/support counts via
  `createCanonicalTracker` (changed clauses open new groups exactly
  as `canonicalFirstUnion` does) and cumulative fixture item count —
  so the admitted observations are a complete contiguous prefix that
  both extractor paths consume fully. The deferred tail rides the
  existing still-pending outbox/`wakeMemory` chain; a single
  observation that alone exceeds the bound still follows the bounded
  failure contract, and `fixtureExtract` keeps a loud
  `projection_too_large` for any unbounded caller.

Producer-version isolation now covers the corrected semantics:
policy version advances 6 -> 7 and adapter version 4 -> 5, so
completed and exhausted runs identified under v6/v4 are never reused
as v7/v5 work. Deploying requires a monotonic
`MEMORY_PROJECTION_GENERATION` increase to rebuild from retained
observations — checkpoints already advanced past observations the
old admission dropped, and queue redelivery alone does not repair
gaps behind a checkpoint. The previous valid projection remains
readable and is marked stale until the new projection is published;
raw observations are unchanged.

Local verification for this follow-up, based on source HEAD `f90ada6`
(uncommitted fix diff):

- Pre-fix reproduction on the final regression tests failed 3/13
  reinstatement cases and 6/20 input-budget cases; the same suites
  pass after the fix, extended by the contested-neighbor regression
  (15/15 reinstatement, 20/20 input-budget). The 13-observation
  corpus splits `[12]+[1]`, `[6]+[7]`, one-by-one, bulk-synced
  multi-run, and batch-size-6 all converge on the same final JSON
  policy, current summary, and support history through real
  workerd/D1 and `context_resolve`; queue replay/rebuild stays
  duplicate-free.
- `npm test` in `gateway/` passed 286/286 including the existing
  canonical-cap contract tests, now asserting both admission deferral
  and fail-closed internal caps; `just check-generated` 10/10, `npm
  run deploy:dry-run` (bundle dry-run, not a remote deployment),
  `git diff --check`, Node 20.20.2 focused suites 34/34 (pre-B3 regression set), and Python
  dogfood 37/37 passed. Rust gates were not re-run this round — the
  diff touches no Rust or generated sources (NOT RUN).
- These are local implementation checks, not a v7/v5 live-model or
  Cloudflare remote qualification. The earlier 9/9 live-model record
  applies to its own tested head and is not carried over. An
  independent non-implementer re-review of this uncommitted diff found
  three blocking findings across two rounds — the contested-neighbor
  demotion and `valid_from` skew above, plus a sticky-check operand
  order that consulted the stale committed row before the fresher
  staged decision — all reproduced pre-fix, fixed, and re-verified;
  final verdict APPROVE (15/15 reinstatement, 72/72 focused, 286/286
  `npm test`).

A later P1 review finding covered the worker bootstrap gap: `0003`
creates `memory_outbox` empty and only ingest writes rows, so a
database migrated with retained observations never woke the worker —
the documented "rebuild from retained observations" could not start at
all once the source host was already offline. The new migration
`0004_memory_worker_bootstrap.sql` seeds one pending outbox row per
`(owner_id, repository_key)` at its retained `MAX(cloud_seq)`, using
the same upsert semantics as ingest so already-wired repositories keep
their durable queued/attempt state; observations with a NULL or empty
`repository_key` never produce rows. The regression test stages
0001+0002, seeds retained observations directly, applies 0003 (empty
outbox, sweep finds nothing due, no checkpoint — the reported wedge),
then applies 0004 and observes the real queue/scheduled-sweep path
consume both repositories to completion (TOML current over superseded
JSON, YAML current, checkpoints at the retained heads) and stay
unrequeued on re-application. `npm test` passed 287/287. Bootstrap is
a scheduling-path fix, not a producer-semantics change: no policy or
adapter version advance, and no generation bump is required for the
initial backlog drain itself. Deploying the earlier v7/v5 semantic
changes still requires the monotonic
`MEMORY_PROJECTION_GENERATION` rebuild described above; on a database
that still holds unconsumed retained observations the 0004 backfill is
what lets that rebuild actually start.

## Ending state

The live-qualified feature source was HEAD
`83a533aa75d9001fa95aa9fcfbef78951123d642`; the follow-up logging fix is commit
`efea482ec6dda2e547bdec9bfa12a3e55f7f5752`. At the start of this adapter-v3
follow-up, branch `codex/20260928-memory-plane` was at HEAD
`b8173594666a1c4002126a75cde1dfd015bb5b73` and tracked
`origin/codex/20260928-memory-plane`. The v3 source and documentation updates
were in the working tree for review at this report snapshot. The original
checkout and its pre-existing change boundary were left intact.

The v7/v5 follow-up described above is staged in the same worktree as an
uncommitted patch on top of HEAD `f90ada6fe4b705516da6993930a6cdbc7f39c275`
(`gateway/src/memory/{worker,extractor,config}.js`, three memory test
files, this document, `docs/gateway.md`, and the tracking issue). The
later bootstrap fix adds `gateway/migrations/0004_memory_worker_bootstrap.sql`,
the `DEFAULT_MIGRATIONS`/`MIGRATIONS` enumerations, and the staged-migration
regression test in `gateway/test/memory-continuity-integrity.test.mjs`.
Per the task contract, commit and push are deferred pending explicit approval.
