# Completion residual validation: resumed connector attempt

Date: 2026-09-15 (Asia/Tokyo)

Status: **blocked; not accepted for integration**. This is new evidence from the resumed public Temote connector, not a replacement for the earlier candidate gates or frozen comparison ledger.

## Immutable inputs and review status

- S15 base: `3abf3d1553be64cb59970400d554d834eeb29d20`.
- S15 candidate: `5be77826b500b7f0a9bdc18e1731e320dedd0ffb`.
- S16 documentation candidate and tested HEAD: `8a6efff690963a0334757b161ad9729f6cd52db0`.
- Integration found at resume: `b02bafe0f6a4e0c9ee8c3d7788fe92beb08fede3` (the additional commit only saved the residual tracker).
- Evaluation found at resume: `cf9d1ffe69638a27af3f39c6b19a1b5ac704ae50` (the additional commit records the previous incomplete stop).

The parent inspected the immutable S15 diff and S16 documentation diff. This is a **partial parent static review**, not a completed independent three-pass approval. The independent Codex worker did not start successfully. S15 and S16 therefore remain unapproved, and no activity code was imported into integration by this attempt.

### Required review question: upgrade activity can reacquire an instance nonce

At S15, `src/activity_runtime.rs::upgrade_target_matches` compares only session ID, `started_at`, and `process_id`. `deliver_with_io(PendingActivity::Upgrade)` then calls `bind_session_instance` and constructs an expected session with the nonce returned by the current runtime. In contrast, ordinary `PendingActivity::Bound` updates retain their original nonce.

Consequently, the predicate itself cannot distinguish two runtime instances with the same legacy tuple. The new upgrade-target tests vary the timestamp or PID; they do not exercise recreation with an identical tuple and a different private nonce. Before declaring the nonce fence complete, independently review whether such recreation is reachable for the detached upgrade path and add deterministic coverage for that case. A same-tuple live recreation was **not reproduced** in this attempt, so this is not a claim that an observed upgrade was misattributed or that authorization was bypassed.

Any correction must preserve a verified original-instance or handoff lineage rather than simply acquiring a replacement runtime's nonce. It must also distinguish a legitimate restored session from a same-name recreated session. The S16 security wording must not be treated as evidence that this case is already covered.

### Review question: observation setup is now awaited

S15 changes `tool_activity_scope` to await `activity_runtime::emitter`, which performs a bounded socket bind exchange before the underlying tool operation. Review this additional caller-path wait against the existing nonblocking producer requirement. Delivery itself remains queued; no latency measurement or independent verdict is claimed here.

## Observed execution blockers

The original `temo` session did not exist. The retained `temote` session reported `crashed`, with `last_error` equal to `session was active when its owning supervisor stopped`. A new `temo` session and narrowly scoped completion/activity/evaluation sessions were created under the configured `src` root. They reported `active`, `permission_mode=agent`, and `yolo=false`.

The independent read-only `local_agent_run` requested `gpt-5.6-sol`. It returned JSON-RPC `-32000` containing exit code 1 and this stderr, before a reviewer result or job was created:

```text
bwrap: execvp /home/hirohito-fujita/.cargo/bin/codex: No such file or directory
```

The current connector's separate `codex_status` also failed closed with JSON-RPC `-32000`:

```text
CODEX_APP_SERVER_INCOMPATIBLE: expected 0.153.4, got temote-mcp/0.153.4 (Ubuntu 24.4.0; x86_64) unknown (temote-mcp; 2026.9.7)
```

No task-start request followed the incompatible status. These are current connector qualification failures, not new attempts of any frozen A/B/C evaluation arm. No executable override, host installation change, credential change, or sandbox relaxation was used.

Two separate `execute` calls, one for additional source inspection and one for failure-log inspection, were rejected with the following tool response:

```text
リクエストの安全性を確認できなかったため、このツールの呼び出しは OpenAI によってブロックされました。
```

Those operations were not retried through another route. This response does not establish that the session crashed or that all work was stopped. Session/job state was checked separately. The additional log inspection did not complete, so failure causes absent from the gate output below remain unknown.

## Checks actually executed

All checks below used the unchanged S16 candidate HEAD. The environment had private `HOME`, `CODEX_HOME`, XDG directories, `TMPDIR`, an independent short `TEMOTE_MCP_SOCKET_NAMESPACE`, offline Cargo, and a clone-local target. No provider credentials were placed in the test environment. These are **candidate-only checks**, not a final integration gate.

| Command / target | Result | Observed detail |
| --- | --- | --- |
| `cargo fmt --all -- --check` | PASS | Exit 0 |
| `cargo test --offline --lib -- --test-threads=1` | FAIL | 99 passed, 8 failed; multiple failures explicitly report `/var/tmp` as read-only (OS error 30) |
| `cargo test --offline --bin temote-mcp activity_producer_ -- --test-threads=1` | FAIL | 3 passed, 4 failed; individual causes not established by returned summary |
| `cargo test --offline --bin temote-mcp activity_ingress_ -- --test-threads=1` | FAIL | 3 passed, 6 failed; individual causes not established by returned summary |
| `cargo test --offline --bin temote-mcp activity_lifecycle_ -- --test-threads=1` | FAIL | 0 passed, 3 failed; individual causes not established by returned summary |
| `cargo clippy --offline --all-targets -- -D warnings` | PASS | Exit 0 |
| `cargo check --offline --no-default-features --all-targets` | PASS | Exit 0 |
| `(cd gateway && npm test)` | FAIL | Exit 1; returned top-level summary reports 1 pass and 1 fail; not a reproduced 70-test success |
| `git diff --check` | PASS | Exit 0 |
| Full unfiltered `cargo test` | NOT RUN | Filtered checks above are not the full suite |
| Explicit ignored process-boundary / upgrade reconnect E2E | NOT RUN | No final accepted integration tree; no real ingress or supervisor upgrade was attempted |
| macOS / physical multi-host / authenticated provider acceptance | NOT RUN | No corresponding current host/provider evidence was obtained |

Job `a87f2986-9884-4cd6-a2e6-9eb866397ebd` returned exit code 1. A subsequent `job_list` explicitly reported `failed`; the activity session remained `active`. The candidate worktree was clean immediately after that job. Bounded local gate output was retained under ignored `target/completion-residual-validation-20260915/`; it is not a provider transcript.

The failed commands are not relabeled PASS because earlier private-host gates passed. Conversely, an observed read-only-filesystem failure does not establish a candidate code defect. No tests or security boundaries were weakened to obtain a green result.

## Resume semantics and remaining conditions

An `Applied` resume receipt acknowledges reconciliation, not successful continuation or task completion. The previously recorded stopped-child qualification returned `Applied` plus `Interrupted`. It did not directly record a replacement-process PID and did not demonstrate completion after resume. English/Japanese usage and the Agent Skill receive the same explicit qualification in this documentation-only follow-up.

Independent S15/S16 approval, any required code corrections and their independent re-review, final integration gates, frozen-arm completion, and actual provider/OS evidence remain open. Restore a working scoped reviewer/runtime and a test environment supporting the required filesystem/socket/process boundaries without weakening containment; retain the frozen arm model/effort, driver, base, prompt, and denominator rules. Keep the original branches/worktrees until remote push is verified. Push outcomes are recorded by the integration residual tracker after the dedicated Git tool attempts.
