# Salvaged completion evidence (2026-09-15 evaluation lane)

Distilled on 2026-09-22 from `codex/20260915-completion-evaluation` docs
(`docs/evaluations/completion-evaluation-manifest-20260915.md`,
`docs/evaluations/completion-evaluation-results-20260915.md`,
`docs/evaluations/completion-residual-validation-20260915.md`) and the
`codex/eval-t05-c-r2-incomplete` tree, per
`issues/done/20260916-completion-evidence-branch-salvage.md`. Each statement is
labeled with its original evidence date/commit and whether the build it tested
is superseded by current `main`. Non-secret evidence only; original values,
hosts' credential material, and raw logs were never imported and remain only in
the branch history and the original operator's `/tmp` artifacts.

The frozen manifest (task statements, common bases, arm/dispatch rules) remains
the authoritative spec if the three-arm evaluation is ever resumed; the results
ledger below records what was proven, blocked, or left unknown.

## Product defects found, fixed, and now covered on current main

| Evidence (2026-09-15) | Original fix | Current-main status | Owning issue / row |
| --- | --- | --- | --- |
| Two MCP processes competed to reconstruct one running Codex task (shared task store, process-local runtime registry); a secondary `get` returned `unknown` and a secondary interrupt could finalize the primary's live task | `1d6cd3e` (not merged as a commit) | Superseded by `main`: `CODEX_TASK_RUNTIME_OWNED` fencing, `reconciliation_deferred` reads; tests `task_get_defers_to_live_runtime_owner_and_resumes_after_release`, `concurrent_control_acceptance_is_idempotent_for_duplicate_operation`, `control_acceptance_rejects_terminal_tasks_without_recording_operation`, `cross_process_store_and_runtime_ownership_are_fenced` | `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md` (repo-local complete) |
| Typed `resume` reused a dead RPC client: after a deliberate private transport stop, resume returned `reconciliation_required` instead of an applied receipt | `ebb1821` (not merged as a commit) | Superseded by `main`: typed `resume` is reconciliation-only by contract (`docs/usage.md` — does not start a new turn or revive a terminated child process); tests `uncertain_thread_start_failure_is_not_blindly_replayed`, `pre_thread_startup_failure_can_retry_same_operation`, `task_get_reconciles_remote_completion_before_not_modified` | same parent |
| `codex_status` failed with `CODEX_APP_SERVER_INCOMPATIBLE` against the real 0.153.4 server because of a strict version pin | `344e6b6` (driver fix) | Superseded by design change: main is deliberately version-agnostic (`app_server_accepts_arbitrary_peer_version_and_rejects_oversized_protocol`, `app_server_version_is_best_effort_diagnostic_only`) | same parent |
| `Applied` resume receipt acknowledges reconciliation, not successful continuation or task completion | contract clarification | Preserved on `main` in `docs/usage.md` (typed resume semantics) | same parent |

## Live-only residuals (still open; not superseded)

| Evidence (2026-09-15, host `ms-01-alpha`) | Status | Owning row |
| --- | --- | --- |
| Linux `local_agent_run(agent=codex)` cannot run Codex inside Temote's outer sandbox on hosts enforcing `kernel.apparmor_restrict_unprivileged_userns=1` / `bwrap-userns-restrict` (nested unified-exec returns `Operation not permitted` even for `pwd`; the same installed Codex works outside the outer sandbox) | Still open; needs a host or outer-sandbox backend that supports nested user namespaces while retaining the Codex named-profile auth deny | `issues/open/20260908-live-acceptance-matrix.md` — Developer broker / installed runtime |
| macOS live Vite+ Codex `local_agent_run` acceptance was never run | Still open | same matrix row |
| Cloudflare read-only probes: `wrangler whoami` authenticated; `wrangler deployments status` showed one current `temote-mcp-gateway` deployment version but no proven route/custom-domain target; `wrangler secret list` was empty (`HOST_TOKENS_JSON` absent under that account/script); unauthenticated `/healthz` returned an OAuth error document (external reachability + auth enforcement) | Still open; authenticated public health not tested | `issues/open/20260908-live-acceptance-matrix.md` — Cloudflare Worker / Durable Object gateway |
| OpenCode 1.18.30 one-shot provider calls all blocked: `opencode-go/deepseek-v4-flash` needs explicit regional opt-in; `opencode/gpt-5.6-luna`, `opencode/gpt-5.6-sol`, `opencode-go/mimo-v2.5`, `opencode-go/mimo-v2.5-pro` reported no payment method; `opencode/mimo-v2.5-free` reported disabled. Deterministic adapter suite 73/73 and bounded diagnostics passed | Still open provider-entitlement limitation | `issues/open/20260908-live-acceptance-matrix.md` (provider entitlement) |
| Host ingress incident: an upgrade-process test stopped the original ingress; recovery blocked on a wedged supervisor control socket (listen backlog 58, `Ping`/list timeouts); 23 live session sockets were active while persisted lifecycle records all read `crashed`; after explicit approval the wedged supervisor and sessions were stopped rather than reconstructed from untrusted metadata | Still-relevant operational evidence; session-liveness-vs-persisted-record divergence is only partially covered by the bounded runtime-observation diagnostics added later | `issues/open/20260916-upgrade-process-group-friction.md` |
| A preflight process listing exposed an unrelated existing tunnel credential in transient tool output because it was present in the other process's command line; the value and process identity were deliberately not copied | Open operational follow-up: credential rotation was not performed | `issues/open/20260908-live-acceptance-matrix.md` |

## Evaluation dispositions preserved

- Adoption decision: **HOLD** (2026-09-15). Selectable reviewed candidates exist, but T07-C, T08-C, T08-A, and all T09/T10 arms were unattempted; T05-C-r2 stopped with an incomplete untested patch (`0054b969`, tree preserved on `codex/eval-t05-c-r2-incomplete`); T01-A and T06-B lacked independent review confirmation; usage/cost/model-identity stay unknown where not directly measured. Owning issue: `issues/doing/20260908-08-codex-delegation-dogfood-and-app-server.md` Phase D.
- S15/S16 candidate-only validation ledger (`completion-residual-validation-20260915.md` @ `8a6efff`): format/clippy/no-default/diff passed on the candidate; socket-bound tests were environment-blocked. **Superseded** — S05–S16 all landed on `main` and the current tree is what gates run; retained as historical record only.
- Two `#[cfg(test)]` isolation overrides (`resolve_codex_home`, `default_runtime_directory`) existed on old branches; **not imported** — `main` isolates `config::state_dir()` for tests and every test passes explicit tempdirs, so they are dead isolation there.
