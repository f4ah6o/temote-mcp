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
- The stored complete Gateway run reports 166/166 tests passing. The root later
  reported a 185/185 full Gateway run; its terminal result is not yet included
  in the retained log set, so the final consolidated test count remains pending.
- The real workerd/D1 integration suite covers migration preservation,
  transaction rollback and fencing, concurrent claims, Queue ACK loss,
  DB-commit retry, restart recovery, scheduled outbox repair, poison-attempt
  bounds, and projection rebuild. The D1 migration runner applied migrations
  0001–0003 to an isolated **local** D1 database (22, 5, and 37 SQL commands
  respectively). No remote D1 migration was run.
- python3 -m dogfood validate validates eight checked-in scenario IDs.
  This validates scenario schemas; it does not run all scenarios.
- Two just generate-tools runs produced identical bytes for the three public
  artifacts and just check-generated passed. The gateway deploy dry-run built
  the Worker with its declared bindings; this did not deploy it or verify a
  remote target. The worker statement bound is at most 602 D1 statements per
  repository invocation, suitable for the Paid 1,000-statement limit; Free's
  50-statement worst-case limit is not qualified.

### Baseline and live memory-continuity scenario

Baseline run 348f3eae-9525-4c5e-a718-a02fcdb1cc7a used scenario
memory-continuity revision 1 and the same scenario fingerprint as the
candidate runs. Its outcome is not_implemented; live synthesis is NOT RUN,
and knowledge-dependent assertions are not_implemented (Queue replay and
offline proof are not_run). The baseline's absent knowledge is not a passing
empty result.

Candidate runs ae1e618f-ba5b-4645-8fe1-e75ff638dd70,
d29a9fe6-287d-41c4-b25e-9bdc6421be05, and
dced0085-3c46-4b57-b7ae-102dc2feeb3a all ended blocked with live synthesis
NOT QUALIFIED. They used scenario revision 1, the same fingerprint as the
baseline, and coding selectors codex / gpt-5.6-luna / max. The final attempt
used glm-5.3-flash with reasoning effort low, a 60,000 ms timeout, 32,768-byte
input budget, 8,192-byte content output budget, batch size 16, and three
extraction attempts. Its bounded projection run failed with invalid_support
at the retry limit. The task-A terminal observation and source sync gates
passed, but task-A projection remained blocked. Partial D1 knowledge rows
existed; they do not qualify the failed batch or its assertions. The head-switch,
Task-B supersession, offline-host, Queue-replay, and tenancy live gates were
not_run; no offline proof file was produced.

| Qualification gate | Interim result | Evidence |
| --- | --- | --- |
| issue_completion | Partial | C2–C5 implementation and local tests exist; the live scenario and final child-issue progress update remain incomplete. |
| memory_pipeline | Local integration passes; live blocked | Workerd/D1/Queue recovery tests pass; live candidate stopped on invalid_support. |
| knowledge_quality | Not qualified | The live support-validation assertion did not pass. |
| head_switch | Not run live | The candidate did not reach Task B or alternate-head resolution. |
| offline_host | Not run live | The dedicated host-offline proof was not reached; local resolver tests do cover repository reads without an online host. |
| tenancy | Not run live | Local owner/repository isolation tests pass; live unrelated-repository assertion was not reached. |
| retry_recovery | Local D1 recovery tests pass; live gate not run | Commit failure, ACK loss, restart, outbox sweep, poison cap, and generation rebuild have workerd tests. |
| tests | Local checks pass; final aggregate pending | Recorded Rust/generation/local D1 checks pass; the later 185/185 Gateway result still needs its retained terminal summary. |
| final_diff | Pending | The reviewed patches precede the ending candidate HEAD; no final diff review has been recorded. |
| cloudflare_remote | Not run | No remote migration, Queue provisioning, binding change, deploy, or remote E2E was performed. |

No live model qualification or Cloudflare remote qualification is claimed.
The next candidate trial must pass support validation before attempting the
Task-B, alternate-head, offline-host, Queue-replay, and tenancy gates. The final
ending HEAD and its full test/review/diff evidence will be recorded after that
trial terminates.
