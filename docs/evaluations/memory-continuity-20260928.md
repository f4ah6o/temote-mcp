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

Implementation packets are committed locally. Live synthesis, final continuity,
independent integrated review and remote qualification are pending. Results below must be updated from terminal runs;
fixture success must not stand in for a live model or remote deployment.

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
