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

An isolated rebuilt Temote process used a normal `agent` session and selected the actual host `~/.vite-plus/bin/codex` launcher (`vp` 0.3.1, Codex CLI 0.147.0). The first read-only request reached the Vite+/Node launcher but failed before model execution: Codex's JavaScript launcher could not resolve its `@openai/codex-linux-x64` optional package inside the local-agent sandbox. The exact package and 262 MB vendor executable existed on the host and were ordinary, matching-version files. A second run after verifying both freshly built Temote binaries reproduced the same bounded failure in 0.507 s.

The current result is a failed attempted acceptance, not a successful launcher closure. Workspace-write, sibling isolation, and protected metadata live checks wait for an accepted generic fix. The host launcher/runtime/global configuration and authentication file were not changed. Linux evidence cannot replace the required macOS live Vite+ acceptance, which remains pending.

## Real Codex app-server

A normal isolated `agent` session and attached local approval console approved only `codex_status` for the canonical evaluation workspace. Official Codex 0.153.4 returned a real initialize `userAgent` beginning with the fixed client product and exact version (`temote-mcp/0.153.4 ...`), while the adapter accepted only the stale `codex_cli_rs/0.153.4` prefix. The call therefore failed closed as `CODEX_APP_SERVER_INCOMPATIBLE` before `model/list`.

Manual protocol qualification with the same pinned runtime confirmed the exact version and required Luna/max and Sol/high model entries. Temote task start, reconciliation, no-change completion, and typed control remain unattempted until the compatibility parser is fixed and reviewed. No approval was auto-granted and no child command/file mutation was approved.

## Available live connection evidence

- Current private files exist at the platform config location: `public.env` mode 0600 and Tunnel token mode 0600. Only configuration key names and file metadata were inspected; values were not printed.
- `public.env` contains the Managed OAuth/Access/public URL/named-root keys. No Cloudflare account API credential variable was present in the evaluation process.
- A redacted GET to the configured public URL's `/healthz` returned in 0.330 s with an OAuth error document and resource metadata, establishing external reachability and authentication enforcement but not an authenticated MCP session.
- Read-only `session_list` probes through the two configured Temote connectors did not return within approximately 73 seconds and were stopped. They are recorded as probe timeouts; no session/tool equivalence claim is made.
- No deployment, tunnel, production runtime, session, or provider configuration was changed.

## Three-arm task ledger

Runtime failures after an arm starts remain in the attempted denominator. A missing common prerequisite is recorded as unattempted. Observability or permission differences are labeled rather than normalized away.

| Task | Order | Common source base | Arm | State | Retry | Wall time | Requested | Observed | Usage | Checks / result |
| --- | --- | --- | --- | --- | ---: | --- | --- | --- | --- | --- |
| T01 | ABC | `0ee1db7` | A | completed | 1 | agent total unavailable; reported command time about 64 s | Sol/high | runtime telemetry unavailable | unknown | focused new + existing HTTP tests, retention E2E 9, fmt, clippy, no-default-features check, diff check pass; no unresolved items |
| T01 | ABC | `0ee1db7` | B | completed with unresolved runtime-test limitation | 0 | 508.365 s | Luna/max | runtime event telemetry unavailable | 4,374,920 input (4,238,336 cached), 19,858 output, 10,186 reasoning output tokens | source changes complete; fmt, diff, clippy, cargo checks, targeted non-runtime tests, and gateway tests pass; sandbox denied Unix-socket creation for runtime HTTP/fallback tests |
| T01 | ABC | `0ee1db7` | C | unattempted | 0 | — | Luna/max | — | — | app-server driver fix `344e6b6` accepted after the frozen task source base; live rerun pending with source base unchanged |
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
