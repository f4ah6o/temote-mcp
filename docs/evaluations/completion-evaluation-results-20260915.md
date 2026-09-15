# Completion evaluation results (2026-09-15, Asia/Tokyo)

Status: execution in progress. Task definitions are frozen in [`completion-evaluation-manifest-20260915.md`](completion-evaluation-manifest-20260915.md).

## Runtime qualification

| Check | Result | Bounded evidence |
| --- | --- | --- |
| Host installations | pass, unchanged | Temote MCP 2026.9.7; Vite+ 0.3.1; Vite+-managed Codex 0.147.0; OpenCode 1.17.10 |
| Isolated Codex provenance | pass | official npm `@openai/codex@0.153.4`; exact CLI version; npm integrity in manifest |
| Isolated OpenCode provenance | pass | official npm `opencode-ai@1.18.30`; exact CLI version; npm integrity in manifest |
| Codex app-server model list | pass | `gpt-5.6-luna` advertises `max`; `gpt-5.6-sol` advertises `high` |
| Direct auth/model smoke | pass | Luna/max returned `OK` in 4.378 s; Sol/high returned `OK` in 4.463 s |
| Isolated state/auth boundary | pass | evaluation data under `/tmp/temote-completion-eval-20260915`; copied auth/account inputs mode 0600; source auth inode/size/mtime/mode unchanged where checked; no contents printed |

## OpenCode 1.18.30 acceptance

The deterministic adapter suite passed 73/73. Read-only diagnostics through `TEMOTE_OPENCODE_BIN` reported exact version 1.18.30, 96 bounded model identifiers, no truncation, and the requested model present.

Current one-shot provider calls did not complete:

| Requested model | Adapter/provider result | Workspace |
| --- | --- | --- |
| `opencode-go/deepseek-v4-flash` | bounded `process_nonzero_exit`; provider requires explicit regional opt-in | unchanged |
| `opencode/gpt-5.6-luna`, `opencode/gpt-5.6-sol`, `opencode-go/mimo-v2.5`, `opencode-go/mimo-v2.5-pro` | provider reports no payment method | unchanged |
| `opencode/mimo-v2.5-free` | provider reports model disabled | unchanged |

These attempted runs remain failed current live attempts and are not converted into successful acceptance. Earlier 2026-09-12 evidence already records successful 1.18.30 one-shot executions and normalized report delivery. Persistent server/attach/automatic-resume behavior was not attempted and is outside the accepted issue scope.

## Linux Vite+ `local_agent_run`

An isolated rebuilt Temote process used a normal `agent` session and selected the actual host `~/.vite-plus/bin/codex` launcher (`vp` 0.3.1, Codex CLI 0.147.0). One evaluation binary pair failed before model execution because the JavaScript launcher reported a missing `@openai/codex-linux-x64` optional package, even though the matching package and regular 262 MB vendor executable existed. A separately built pair from source with the same launcher/sandbox code passed a Luna/max model smoke in the same evaluation environment (`VITE_READ_ONLY_OK`, exit 0, 12.822 s). The binary artifact differential remains unexplained; no source fix or launcher-closure regression is claimed from the first failure.

The verified pair then reached a distinct host limitation in `workspace_write`. Codex could not start even `pwd` or create an allowed workspace file: nested unified-exec creation returned `Operation not permitted`. The same installed Codex and exact named permission profile work outside Temote's outer local-agent sandbox. The host enforces `kernel.apparmor_restrict_unprivileged_userns=1` and a `bwrap-userns-restrict` profile; a nested user-namespace probe fails under the outer bwrap. Disabling the inner sandbox would also remove the imported-auth deny and is not a safe acceptance substitute.

The absent `.git` and sibling writes are non-diagnostic because the allowed write failed too. Linux acceptance remains pending on a host or outer sandbox backend that supports nested user namespaces while retaining the Codex named-profile auth deny. A representative read-only canary read and successful bounded workspace write still need to pass there. The host launcher/runtime/global configuration and authentication file were unchanged. Linux evidence cannot replace the required macOS live Vite+ acceptance, which also remains pending.

## Real Codex app-server

The first real call identified an initialize `userAgent` mismatch and failed closed before `model/list`. The narrowly reviewed driver fix `344e6b6` accepts the verified real 0.153.4 grammar without relaxing the exact version pin. The driver is separate from every evaluated task's source base.

With that driver, a normal isolated `agent` session and attached approval console completed `codex_status` against official Codex 0.153.4 in 25.672 s. It reported `compatible=true`, Luna with `max`, and Sol with `high`. The console approved only the explicit status operation. A second no-change Luna/max task was approved and started as `running`; one typed `steer` was separately approved, advanced the generation, and produced `APP_SERVER_TYPED_CONTROL_OK`. `codex_task_get` returned `completed`, revision 6, `reconciliation_required=false`, and the retained completed state remained available after that MCP client exited. Scoped evidence identified CLI 0.153.4 and the exact final message; usage was unavailable. The workspace remained empty.

Concurrent-client qualification exposed a separate defect during T01-C: a second MCP process tried to reconcile a task still owned by the first MCP process. The secondary read returned `unknown`; the original runtime was later interrupted, and a fresh read reported terminal `interrupted`. The on-disk task store is shared while the live runtime registry is process-local, so concurrent clients can compete to reconstruct one running task. This is a failed attempted task outcome and is assigned as a product defect; it is not hidden by the successful single-client smoke.

## Available live connection evidence

- Current private files exist at the platform config location: `public.env` mode 0600 and Tunnel token mode 0600. Only configuration key names and file metadata were inspected; values were not printed.
- `public.env` contains the Managed OAuth/Access/public URL/named-root keys. No Cloudflare account API credential variable was present in the evaluation process.
- A redacted GET to the configured public URL's `/healthz` returned in 0.330 s with an OAuth error document and resource metadata, establishing external reachability and authentication enforcement but not an authenticated MCP session.
- Read-only `session_list` probes through the two configured Temote connectors did not return within approximately 73 seconds and were stopped. They are recorded as probe timeouts; no session/tool equivalence claim is made.
- No deployment, tunnel, production runtime, session, or provider configuration was changed.
- An isolated official `wrangler` 4.131.2 (`sha512-jmkGE7monbPKyYQr1FPQN+SARVhddqw2fhXOmTKCw4lroqlFGSS6rit/RTvPi/qzNLKrXxkS8DhWXasJnStplg==`) reused a private copy of the standard Wrangler config. `wrangler whoami` exited 0 and reported a logged-in account and permission table. Account API authentication is therefore available; no account identifier, email, token, deployment, or configuration value is recorded here.

## Three-arm task ledger

Runtime failures after an arm starts remain in the attempted denominator. A missing common prerequisite is recorded as unattempted. Observability or permission differences are labeled rather than normalized away.

| Task | Order | Common source base | Arm | State | Retry | Wall time | Requested | Observed | Usage | Checks / result |
| --- | --- | --- | --- | --- | ---: | --- | --- | --- | --- | --- |
| T01 | ABC | `0ee1db7` | A | completed | 1 | agent total unavailable; reported command time about 64 s | Sol/high | runtime telemetry unavailable | unknown | focused new + existing HTTP tests, retention E2E 9, fmt, clippy, no-default-features check, diff check pass; no unresolved items |
| T01 | ABC | `0ee1db7` | B | invalid/non-comparable; clean-context retry running | 1 | first attempt 508.365 s | Luna/max | runtime event telemetry unavailable | 4,374,920 input (4,238,336 cached), 19,858 output, 10,186 reasoning output tokens | first patch/check report completed with Unix-socket test limitation, then inspected accepted answer commit `232b5f1` through shared refs; retained as contaminated attempt and not selectable |
| T01 | ABC | `0ee1db7` | C | invalid/non-comparable; clean-context retry pending | 1 | first attempt 177.978 s before client loss; reconciliation 13.757 s | Luna/max | app-server returned requested model/effort; observed model provenance otherwise unavailable | unknown | first task produced no changes, inspected `232b5f1`, and was interrupted after a competing monitor client; retained as contaminated failed attempt and not selectable |
| T02 | BCA | `0ee1db7` | A/B/C | unattempted | 0 | — | per arm | — | — | frozen; prerequisite available but arms not started |
| T03 | CAB | `0ee1db7` | A/B/C | unattempted | 0 | — | per arm | — | — | frozen |
| T04 | ABC | `0ee1db7` | A/B/C | unattempted | 0 | — | per arm | — | — | frozen |
| T05 | BCA | `0ee1db7` | A/B/C | unattempted | 0 | — | per arm | — | — | frozen; S03 is present at base |
| T06 | CAB | `523435b` | A/B/C | unattempted | 0 | — | per arm | — | — | common prerequisite recorded before any arm; accepted S05 |
| T07 | ABC | pending S12 | A/B/C | unattempted | 0 | — | per arm | — | — | common prerequisite unavailable |
| T08 | BCA | pending accepted upgrade | A/B/C | unattempted | 0 | — | per arm | — | — | common prerequisite unavailable |
| T09 | CAB | pending S15 | A/B/C | unattempted | 0 | — | per arm | — | — | common prerequisite unavailable |
| T10 | ABC | same pending S15 as T09 | A/B/C | unattempted | 0 | — | per arm | — | — | common prerequisite unavailable |

T01-A's first invocation stopped at the account usage limit before changing the clean worktree. The same child task and worktree resumed after account availability returned; retry 1 is retained in the ledger.

T01-B completed through the Temote-owned delegation adapter. Its report correctly left observed model/effort unset because event telemetry did not expose them; the bounded event log did expose token counters, which are recorded as backend-specific usage without a cost inference. The arm remains an attempted completion with a runtime-test limitation rather than being excluded from comparison.

The initial T01 B/C worktrees were linked Git worktrees whose shared object database exposed post-base answer refs. Both Luna attempts found and inspected `232b5f1` despite the frozen prompt. Later retries use independent clones from a bundle containing only the common base's reachable history: no alternates, no answer refs, and `232b5f1` is not a readable object. The contaminated attempts and their runtime failures remain in the ledger; fresh-context retries do not erase them.
