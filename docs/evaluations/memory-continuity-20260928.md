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
`issues/open/20260926-cloud-observation-knowledge-plane.md`.

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

Implementation packets are committed locally. Local code and workerd/D1
qualification is separate from live-model synthesis and Cloudflare remote
qualification. The live scenario has run, but it is **not qualified**; the final
candidate HEAD and Cloudflare remote gates remain pending. Fixture and local
integration success do not substitute for either gate.

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
  `57a75293660853b4ed88e62ed93ca903162a3b38`, the retained Node 22.23.3 CI
  run reports 201/201 tests passing, 0 failed/skipped, exit 0 (log SHA-256
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

### Baseline and live memory-continuity scenario

Baseline run `348f3eae-9525-4c5e-a718-a02fcdb1cc7a` used scenario
`memory-continuity` revision 1 and the fingerprint recorded above. Its outcome
is `not_implemented`; its manifest has no extractor configuration and live
synthesis is NOT RUN. The baseline's absent knowledge is not a passing empty
result.

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

The latest live attempt is
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
task budget. No live trial has been run on this head; the next live run is
pending. No raw task text, provider request/response, or secret is included here.

| Qualification gate | Interim result | Evidence |
| --- | --- | --- |
| issue_completion | Partial | C2–C5 implementation and local tests exist; full live scenario and final issue progress remain incomplete. |
| memory_pipeline | Local integration passes; live blocked | Workerd/D1/Queue recovery tests pass; the latest candidate timed out waiting for Task B projection. |
| knowledge_quality | Not qualified | Task A projection predicate passed, but public context assertions and full scenario assertions were not run. |
| head_switch | Not qualified | Task B became ready, but Task B projection and alternate-head context assertions were not run. |
| offline_host | Process proof passed; retrieval not run | The dedicated host absence proof passed; context retrieval/assertions while offline were not run. |
| tenancy | Not run live | Local owner/repository isolation tests pass; the live unrelated-repository assertion was not reached. |
| retry_recovery | Local D1 recovery tests pass; live replay not run | Commit failure, ACK loss, restart, outbox sweep, poison cap, and generation rebuild have workerd tests; Queue replay is NOT RUN in this live candidate. |
| tests | Local checks pass; final diff-bound aggregate pending | Rust checks, Node 22.23.3 Gateway 201/201, terminal harness 15/15, Python 35/35, SQLite 10/10, D1 recovery tests, generator checks, and deploy dry-run are recorded. |
| final_diff | Code review approved; report commit pending | The frozen-head nine-file code delta is independently approved; this evaluation update remains uncommitted and needs final-diff inclusion. |
| cloudflare_remote | Not run | No remote migration, Queue provisioning, binding change, deployment, or remote E2E was performed. |

No live model qualification or Cloudflare remote qualification is claimed.
The next candidate trial must complete Task B projection before the public
context, supersession, Queue-replay, and tenancy assertions can be evaluated.
Cloudflare remote qualification remains a separate NOT RUN gate. The final
ending HEAD and complete test/review/diff evidence will be recorded after the
next trial and final review terminate.
