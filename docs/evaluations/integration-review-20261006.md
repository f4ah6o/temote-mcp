# Review result

Target: integration branch, code commits through `b381d19` and accompanying documentation/issues
Base: `02d8285`
Verdict: approve-with-nits — reviewed implementation and available live acceptance; external gates remain explicit

## Summary

Three review passes examined correctness, safety/error boundaries, and maintainability/tests/integration. Cross-file callers, relevant issue acceptance, backend projections, and actual Cloudflare inventories were read alongside changes. The review includes the final continuation claims, delegated VCS receipts, root policy and actual OpenCode registration correction. Code commits are `eb8ef7f`, `091b8c4`, `2ed578b` and `b381d19`; Fabric commit is `fc2c65a`.

## Findings resolved during integration

| Pass / dimension | Concrete finding | Resolution |
| --- | --- | --- |
| Correctness, API compatibility | Unchanged task probes and independent interaction summaries could churn task revisions or stale their verification. | Backend semantic reconciliation preserves unchanged records; pending summaries own their cursor. Tests separately assert initial newly observed native usage and repeated unchanged probes. |
| Correctness, lifecycle | Reconnected session history was rejected if the current lifecycle state had advanced. | Events accept retained historical transitions only under current authenticated Host generation and exact session owner; monotonic projections reject regression/replay. |
| Safety, concurrency | Subscription replacement after an awaited owner read could deliver an old outbox entry under a new instance. | Delivery re-reads the active subscription instance and retires only the obsolete entry. |
| Safety, error handling | Extension Host JSON was allocated before bounding. | Streaming bounded JSON parser is used before decoding. |
| Safety, authority | A discovered root preparation session ID could accept generic task text or controls. | Common admission reserves receipt-owned preparation sessions; typed starts match full owner, UUID, exact server-owned task/model/effort both before approval and immediately before backend startup. |
| Safety, lifecycle | Private OpenCode checks bypassed common approval/observation and lacked parent generation authority. | Fixed helpers use orchestrated Codex, private-origin fingerprints, parent TaskId/workspace/generation/runtime fences, and matching retained child cleanup. |
| Safety, concurrency | A previously started OpenCode bridge could lend a workspace after Change binding. | Bridge rejects Change-bound workspace before approval and again at pre-start admission. |
| Error handling, retries | A missing initial snapshot task receipt after a saved reservation could be treated as never dispatched. | A durable dispatch attempt and fail-closed lost-receipt recovery are durable and tested; no new snapshot may be started when prior acceptance is unknown. |
| Correctness, execution boundary | A post-start snapshot could be mistaken for the initial mutating task state, and the create/allocation sequence required an unknown future executor TaskId. | Typed ready-allocation creation and initial snapshot/start receipts are implemented with exact allocation, VCS and backend acceptance receipts. |
| Tests, integration | Private event fixtures used non-private temporary parent directories; revision assertions described the prior semantics. | Fixtures now use owner-only parents. Summary cursor and native usage tests retain their distinct semantic assertions. |
| Compatibility, delivery | Deploying a legacy profile after rename/transfer could create fresh state or revert target ownership. | Preflight rejects backwards stages; actual migration preserved both namespace IDs and D1 database. Authority redeployments omit the public domain. |

## Tests reviewed

Meaningful owner/scope/replay properties, backend native-report fixtures, socket lifecycle tests, deployment-stage identity checks, webhook replacement/monotonicity regressions, real workerd D1 transactions, actual browser lifecycle tests, and environment mutation survivors. Four selected environment mutants were all caught after a green library-only baseline. The initial mutation attempt's compile failure is not a mutation PASS.

## Residual risks

Full Rust (125 library + 1,050 binary + ordinary integration tests), Clippy all targets/features with warnings denied, no-default check, and installed OpenCode private registration passed. Supervisor upgrade/lifecycle E2Es and three repeated macOS restart/drain runs also passed. Linux/process/provider/physical-host/registered-client gates are independent. Chromium 1228 was actually used for available browser acceptance; pinned 1243 download was unavailable. Existing Fabric Link runs the old in-process Host code until runtime-injected restart can be authorized through 1Password MCP. No release version was manually advanced.

No additional defect was found in the reviewed secret-binding inheritance, Access denial, canonical/legacy namespace aliases, bounded wait deadline, strict/lenient report separation, raw-result retention bound, or friction export authorization boundaries. This statement covers the reviewed code paths and available evidence; it does not claim external provider acceptance.

## Final passes

Correctness/API compatibility: strict continuation object keys, atomic source
claims including crash-before-successor-write, source control fencing, native
report validity, exact receipt replay, legacy defaults and intentional optional
schema additions were checked. Safety/error handling: new normal starts fail
closed outside named roots; legacy restart retains verified scope; friction
observation binds the full session instance and uses bounded no-follow marker
reads; Accepted VCS/delivery operations reconcile rather than redispatch.
Concurrency/lifecycle: a nonblocking cross-process bound-task guard closes the
probe/resume-versus-snapshot race; writer authority stays held through receipt
import; OpenCode parent authority is registered before prompt submission;
pinned provisioning claims survive their marker/phase crash point.
Maintainability/tests/integration: common report profiles, canonical environment
lookup, physical Fabric rename, source authority inheritance, namespace/D1
continuity and current documentation/issue links were checked. Real OpenCode
V2 registration corrected the config/status shape and catalog readiness.

Final live dogfood found an additional major defect: MCP directly dispatched
inside its frontend process while local task CLI dispatched inside the
supervisor. Continuing a local terminal task from MCP failed with
`CODEX_CONTINUATION_RUNTIME_OWNED`. A shared orchestration function alone does
not share the backend runtime lease. The repair must route modern frontends
to the same authoritative supervisor while preserving full-instance fences,
public yolo denial, typed requests, uncertainty and distinct wire limits.
Commit `2ed578b` repaired runtime and evidence ownership through typed private
relay. Both actual frontend directions, native continuation/replay, fully
escaped 1 MiB conflict handling and scoped evidence reads passed. The new
owner-process, negotiation/error, full-instance and chunk-cap regressions
also passed. The confirmed major defect is resolved.

### nit

Location: `Cargo.toml` compatible binary targets.
Both canonical and legacy binaries intentionally use `src/main.rs`; Cargo emits
a duplicate-target manifest warning. It does not prevent format, tests or
Clippy. A future dedicated compatibility entry file can remove that warning
while preserving one implementation. No compiler warning was suppressed to
hide this manifest warning.


## Repair review passes

Correctness/API: task and evidence calls negotiate one explicit supervisor
capability; old supervisors retain only the successful legacy-mode fallback.
Actual local/MCP control, source/successor lineage and UUID replay confirm one
runtime owner. Public schemas and generated fingerprints are unchanged.
Safety/error handling: only typed backend operations and opaque evidence IDs
cross the private boundary, with independent size bounds, full instance and
public yolo checks. Failed negotiation, missing owner evidence, unknown
responses and unsupported protocol versions never probe a second owner.
Concurrency/lifecycle: backend leases remain inside one supervisor; evidence
binds process/start/scope/mode/grants, so replacement instances cannot inherit
it. Production restart was not forced. Maintainability/tests/integration:
ordinary control limits remain 64 KiB; the large request frame is isolated;
all new regressions, native canary and central gates passed. No unresolved
blocker, major or minor defect remains in these reviewed repair paths.

## Managed-source selection review

A later live canary found a correctness defect in automatic preparation model
selection: choosing the lexicographically smallest advertised name could select
a hidden auxiliary model. The repaired status projection preserves native
`hidden`, `isDefault` and `defaultReasoningEffort` metadata. Preparation excludes
hidden models, requires an unambiguous visible default when advertised, and
retains catalog order for legacy inventories. Advertised effort validation and
durable receipt pinning remain intact.

Pass 1 (correctness/API): optional status metadata is additive; explicit user
model selection is unchanged; an unusable or ambiguous default fails closed.
Pass 2 (safety/errors/lifecycle): accepted receipts are not rewritten, no second
dispatch or network authorization is introduced, and blocked native preparation
cannot activate a missing workspace marker. Pass 3 (tests/integration): native
projection/legacy fixtures and a generated catalog reference property cover
hidden/default ordering, empty model/effort boundaries and unadvertised effort.
A fresh actual canary selected `gpt-6.1-sol` / `low`, returned a valid native failed report and created only an isolated bare store/failed marker. The physical source fetch remains blocked by DNS/network failure and rejected child approval;
this external result is not acceptance PASS. No unresolved code finding remains
in this reviewed repair; the Cargo compatibility-target nit remains.

Final scoped projection/selection mutation run: green baseline, 5/5 caught, zero missed/unviable/timeouts. Final issue and branch-local link guards passed. No source change followed the verified `b381d19` repair.
