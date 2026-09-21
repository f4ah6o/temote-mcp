# Phase 2.4 repair — independent Astra Pro review

Date: 2026-09-18 (Asia/Tokyo)
Reviewer and decision owner: this ChatGPT.com GPT-6 Astra Pro
Current decision: REQUEST_CHANGES
Repair status: NOT IMPLEMENTED; two structured OpenCode startup attempts returned exit 1 (EPERM) without a job_id
Next roadmap packet: BLOCKED; do not start structured-worktree-remove
Commit / push: not authorized, not performed

## Fixed input

- Repository: /home/hirohito-fujita/src/local-mcp (f4ah6o/temote-mcp)
- HEAD: 39fddd514af06964f89410ca70711382a7d53806
- origin/main: a32e75219fddd71114d7c1c7f29e4e71bef8c358
- Existing four commits: a0ee6116cc2ba7a20c56313daef62e24d2fad6df, 8350d80aeaa2d42a5ce8cdf27d72357cf52b2bed, f460bd7075de658f486e9d449af6e3e62435f2ba, 39fddd514af06964f89410ca70711382a7d53806.
- Input repair: 8 files, 2573 insertions, 240 deletions; index empty.
- SHA-256 of git diff --binary: 4f99d1ab6fba06f0e66f4a612e315a35c2d21f2b76d156b698eb6efb0d202ddf.
- SHA-256 of git ls-files --stage: 737548a18b6f9fc3593f0d2959bb103635920aa93792dc8be3c67a7577bf28e7.
- Existing .tmp/, .wt/, and the four untracked appserver polished issues are unrelated and must be preserved.

The current repository's done issue notes contain author/self-verification results. A final independent verdict for this exact eight-file fingerprint was not located in the inspected docs/issues. Those self-verification claims are not the basis for this decision.

## Independently observed evidence

Evidence directory (ignored build output, not a committed artifact):
`target/phase24-astra-20260918-778a3b2f/`

- `initial.diff`, `initial-status.txt`, `initial-input.sha256`: initial state.
- `sandboxed-check.log`: independently executed `just sandboxed-check`, exit 101. Library subset: 118 passed, 1 failed. `sandbox::linux::helper::tests::pinned_workspace_descriptor_survives_a_path_swap` failed at helper.rs:894 because its nested host acceptance requires a root-owned, non-writable /usr/bin/bwrap. Subsequent recipes were not executed; they are not PASS.
- `source-snapshot/`: a copy of the current tracked source contents. The reviewer appended independent tests only in this copy; the original eight-file diff stayed unchanged.
- `independent-probes.log`: `cargo test --manifest-path target/phase24-astra-20260918-778a3b2f/source-snapshot/Cargo.toml --target-dir target --lib --all-features --locked phase24_astra_ -- --nocapture`, exit 101, 0 passed / 5 failed. These tests use disposable repositories and sentinels only, not the user's real Git metadata.

## Findings and parent-owned repair scope

### P24-B01 — missing-path handling removes non-Git protection (BLOCKING)

`src/sandbox/linux/helper.rs:471-493` skips every absent read-only path. It is not limited to missing paths whose parent is already read-only. Ordinary-command and local-agent policies still enumerate missing top-level .git/.agents/.codex specifically to prevent creation (sandbox.rs:64-73), but the emitted sandbox has no corresponding protection while the workspace is writable.

Independent test `phase24_astra_missing_top_level_metadata_is_not_silently_omitted` fails: the rendered argument vector contains the writable workspace mount and no missing .git protection. Also inspect missing objects/info and objects/pack under the writable objects mount; do not claim a read-only common root protects descendants re-exposed writable.

Required repair: preserve the existing protected-metadata contract, distinguish absent Git entries protected by an actually read-only parent from entries under a writable mount, and retain R2 absence/readability semantics. Do not solve this by disabling sandbox protection or globally accepting missing paths.

### P24-B02 — staged Git apply/cleanup can target another repository (BLOCKING)

`src/sandbox.rs:640-684` re-resolves string paths during host-side apply and Drop. There is no pinned metadata-directory/parent identity or no-follow apply authority.

Independently reproduced:

- After prepare, substituting common_dir with a symlink to another disposable repository makes apply return success and changes that repository's HEAD. Drop also removes that other repository's index.lock.
- A symlinked logs parent lets apply overwrite an outside sentinel.

Tests: `phase24_astra_apply_rejects_retargeted_common_directory`, `phase24_astra_apply_rejects_symlinked_reflog_parent` both FAIL.

Required repair: anchor staging, target parents, atomic writes and owned-lock cleanup to verified metadata directory entities; fail closed on replacement/symlink/special-file state. Never remove a lock merely because its old pathname exists. Preserve unrelated repository/worktree state.

### P24-B03 — staging lock and lifetime handling (HIGH)

`src/sandbox.rs:548-596` copies state before acquiring index/HEAD locks. A competing operation can commit between the snapshot and acquisition, after which stale state may be applied. This ordering defect is established by source inspection, not claimed as a concurrency runtime reproduction.

Independently reproduced:

- With an existing HEAD.lock, prepare creates index.lock then returns an error without releasing its own index.lock.
- Drop leaves the private staging directory behind.

Tests: `phase24_astra_head_lock_collision_preserves_other_lock_and_releases_own_index_lock`, `phase24_astra_drop_removes_private_staging_directory` both FAIL.

Required repair: acquire and own locks before reading the snapshot; roll back only owned locks on every failure; remove private staging on every terminal path; do not apply an unmodified stale snapshot after launch/setup failure. Add deterministic ordering/error-path tests.

### P24-B04 — pinned workspace and protected overlays use different sources (REVIEW REQUIRED)

The helper uses --bind-fd for the workspace but subsequently re-reads existence and mounts protected paths from host path strings. The descriptor-survives-swap test constructs an empty read_only_paths vector, so it does not exercise the production protected overlays. Inspect and test a post-pin workspace replacement with absent/different protected metadata: the bound original workspace must not lose .git/.agents/.codex protection or receive another workspace's protected metadata. This is a source-level concern; no nested host reproduction is claimed in this initial record.

### P24-B05 — repository-local gate contains an unclassified host test (HIGH)

The added helper fd-pinning test invokes actual bubblewrap inside the library subset, while `just sandboxed-check` promises a deterministic normal-session subset. Split deterministic fd/argument tests from explicit host execution, and ensure the latter is actually invoked by the Linux host gate. Do not silently skip the acceptance or label it PASS here.

## Repair boundaries

- The parent ChatGPT owns specifications, review, completion and next-packet decisions.
- OpenCode may implement one bounded repair packet at a time only after this REQUEST_CHANGES record.
- Initial repair packet should address the coherent primary Git staging lifecycle (P24-B02/B03) in src/sandbox.rs plus focused tests. Remaining findings are separate bounded repairs.
- No Phase 2.5/remove/prune/branch cleanup, unrelated appserver repair, commit, push, runtime upgrade, credential operation, or global configuration change.
- Existing dirty/untracked work must remain intact.

## Finalization

Final independent verdict for this exact source fingerprint: **REQUEST_CHANGES**.
The original eight-file product diff has not been modified by this review or the failed worker attempts. No issue was moved to done, no next roadmap implementation packet was started, and no commit/push or branch/worktree cleanup occurred.

### Additional independently executed checks

| Target | Result |
| --- | --- |
| `cargo test --bin temote-mcp --all-features --locked managed_worktree::tests:: -- --nocapture` | PASS: 17 tests |
| configured-src production approval identity, exact test | PASS: 1 test |
| bounded approval metadata, exact test | PASS: 1 test |
| bounded/path-free worktree input, exact test | PASS: 1 test |
| `cargo test --bin temote-mcp --all-features --locked agent_git` | PASS: 30 tests; 1 ignored host acceptance was NOT RUN |
| generic sandbox library tests | PASS: 37 tests |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| `cargo check --no-default-features --all-targets --locked` | PASS with dead-code warnings; not a warning-free result |
| `npm run test:sandbox --prefix gateway` | PASS |
| `git diff --check` | PASS |
| `just sandboxed-check` | FAIL: 118 library tests passed and the helper host-bwrap test failed; separate successful checks do not change this gate result |
| independent reviewer regressions | FAIL: 0 passed, 5 failed |
| Linux nested sandbox/host acceptance and live provider success | NOT RUN / not established; no PASS claim |

The reviewer snapshot initially shared the ordinary Cargo target directory. Its extra test-module count remained observable in a later cached library test binary. To eliminate build-provenance ambiguity, the original source was then built and tested using the separate target directory `target/phase24-astra-20260918-778a3b2f/original-build`: generic sandbox 37 PASS (103 filtered, original 140-test library), managed-worktree 17 PASS, routed gateway contract snapshot 1 PASS, and public fingerprint snapshot 1 PASS. Those isolated results are authoritative for these checks. No source or security policy was changed to obtain them.

The R3 configured-src approval fixture was inspected and executed. It enters the production named-root lookup in a child process and checks derived workspace/branch/approval fields. The R4 regression was inspected and executed as part of the 17-test managed-worktree suite: it constructs a real linked worktree of the selected repository on the requested branch outside the managed namespace and verifies the direct-child rejection. These are valid positive pieces of review evidence, but do not resolve P24-B01/B02/B03/B04/B05.

### OpenCode repair attempt and recovery

The parent fixed a single bounded repair scope for P24-B02/P24-B03: `src/sandbox.rs` primary staging, owned locks, safe apply and cleanup, plus focused tests. OpenCode was instructed not to change other source files, issues, contracts, runtime settings or the existing worktree. It was not asked to decide approval or next-packet eligibility.

Both structured `local_agent_run` calls used `agent=opencode`, `model=opencode-go/deepseek-v4.1-flash`, `access=workspace_write` and returned:

```text
JSON-RPC code: -32000
exit_code: 1
stderr:
Error: Unexpected error

EPERM : failed to spawn process
```

No `job_id` was returned. After the first failure, `job_list` showed no new job, `session_info` showed the original session active, and `git status` still showed the original eight modified source files plus this review document. An identical safe retry returned the same failure. `job_list` again showed no repair job or running job. This is a failed worker-start attempt, not evidence that the whole session or OpenAI execution platform was stopped.

`issues/doing/20260916-local-agent-opencode-eperm.md` already tracks this launch symptom and a pending rebuilt-runtime canary. Its historical root-cause notes were read, but are not proof of the cause of today's two failures. No duplicate EPERM issue was created, no unsupported direct-executable bypass was attempted, and no runtime upgrade or provider/credential change was performed.

### Durable review evidence

The following files are new untracked review artifacts, not product repairs:

- `docs/evaluations/phase24-repair-astra-20260918-778a3b2f.md`: this verdict and bounded repair specification.
- `docs/evaluations/phase24-repair-astra-20260918-778a3b2f-independent-probes.patch`: the five reviewer tests only, based on the unchanged current eight-file source. SHA-256: `b84ab5ba3fe466ece446e1a5dc8efbd050277ebecdd9e42fd46c01ef852aa632`. Do not mistake this for a repair patch.
- `docs/evaluations/phase24-repair-astra-20260918-778a3b2f-validation.json`: exact commands/results and worker attempts. SHA-256: `53c539828bc0be4dabdabe608d77a68897c51ba1c1bf26d151b85ad5571f7419`.

Logs and the review-only source copy remain under the ignored `target/phase24-astra-20260918-778a3b2f/` directory. The patch and validation JSON above preserve reproduction details without requiring those ignored files to be committed.

Rechecked source diff SHA-256: `4f99d1ab6fba06f0e66f4a612e315a35c2d21f2b76d156b698eb6efb0d202ddf` (unchanged).
Rechecked index-entries SHA-256: `737548a18b6f9fc3593f0d2959bb103635920aa93792dc8be3c67a7577bf28e7` (unchanged).
Issue inventory: open 6, doing 8, polished 22, done 62 (unchanged).

Next eligible product work remains Phase 2.4 repair, not structured-worktree-remove. Resume the bounded staging repair only with a working authorized implementation worker; separately repair protected missing-path/fd-overlay handling and test classification. After each repair, inspect incremental diff/scope, run focused tests and the full repository-local gate, then independently re-review. All supported host/live paths require their own actual evidence.
