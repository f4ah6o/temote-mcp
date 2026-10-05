# Temote integration evaluation — 2026-10-06 JST

Branch: `codex/20261005-complete-issues-fabric`; implementation base: `02d8285`; issue-preparation commit: `5ffa9fc`. Implementation, central validation and final frontend acceptance are complete for the available repository gates. Final Rust repair commits: `2ed578b` (frontend runtime/evidence routing) and `b381d19` (visible catalog defaults). External operational and dependency gates remain listed separately. Commands below were executed against the reviewed working tree that produced those commits.

## Temote dogfood

Actual Codex tasks ran through the installed Temote stdio server in the normal `dogfood-20261005` session. Selected plugin Host and session reads passed throughout deployment. The existing Link reports runtime `2026.9.24`, generation 5, and degraded session listing because supervisor-owned `mbt` metadata is absent. No metadata was fabricated or removed. Host binaries were installed; the production supervisor handoff failed closed on the `mbt` restart-context blocker described below.

## Executed Cloudflare migration

Deployment used `cf 1.0.0-beta.12` with reviewed ignored non-secret profiles and prebuilt bundles. Existing secret bindings stayed at the source authority through provider inheritance; no secret values were read or copied. The canonical Worker forwards original requests once over an account-local Service binding to that authority.

| Transition | Worker | Deployed version | Result |
| --- | --- | --- | --- |
| Compatible source candidate | temote-mcp-gateway | `0f4fe47e-e0d3-4411-8f58-fd71ac2942ca` | PASS |
| Class rename | temote-mcp-gateway | `bc747ef0-e275-4bca-9523-937b60494890` | PASS |
| Transfer preparation, no public route | temote-fabric | `327ad61d-830b-40d6-b197-5229ff494645` | PASS |
| Namespace transfer | temote-mcp-gateway | `da1d59fe-a85e-4d02-86e5-c2be7da9513c` | PASS |
| Public domain activation | temote-fabric | `a9083f39-d581-44e0-9372-16e0d059e313` | PASS |
| Authority refresh after contract generation/directory rename | temote-mcp-gateway | `288033da-d8aa-464d-a638-5a239d395c1f` | PASS |
| Canonical target refresh from `fabric/` | temote-fabric | `0ddb07b6-345b-4d5b-8175-ae74d66cf4ce` | PASS |
| Final continuation contract, private authority | temote-mcp-gateway | `6c3f74e5-20a8-4dee-91b7-3fb8576c41b3` | PASS |
| Final continuation contract, public target | temote-fabric | `701d3c28-38b3-44d9-a14c-2f4d62fba5bd` | PASS |

All migration stages and subsequent refreshes retained registry namespace `48b4886dbdd04638b4bf8ba3b12d4683` and session namespace `ce5cf7214e7c4ec387b137bf16123348`. Final owner is `temote-fabric`; classes are `FabricRegistry` and `FabricSession`. Authenticated selected-plugin Host/session reads passed after activation; unauthenticated `https://temote.f12o.com/healthz` returned 401 through Access. These prove available ingress and routing, not new Host feature acceptance.

D1 database `3f2252ac-24fe-4356-b0d5-533d00172e6c` was retained. Migrations 0003, 0004 and 0005 were actually applied; `PRAGMA foreign_key_check` returned no violations and pending event count was zero. Pre-migration Time Travel bookmark: `00000004-00000000-000050fb-d27d9a7e4f3ee2e9ca2f9876a074d2d6`. No row contents were read. Rollback after transfer must retain target-owned namespaces; do not deploy a pre-rename configuration. Source authority redeployments omit the public domain.

The physical source directory was renamed from `gateway/` to `fabric/` after those runtime checks. Current CI, developer commands, contract generation and current documentation point to `fabric/`; historical evaluation records retain the commands they actually executed.

## Earlier integration validation

- All-target Rust check passed before the final typed Change start addition.
- Contract generator: 10/10 passed, authoritative managed-source and bounded-wait metadata regenerated.
- Full JavaScript suite after directory rename: 456/456, zero skipped, with actual installed Chromium 1228 selected only for positive browser tests. Pinned Chromium 1243 download was unavailable. Negative missing-browser lifecycle tests remained intact.
- Full Rust run before the final additions: library 125 passed; binary 1007 passed, four failed, one ignored. The four fixture defects were corrected; this is not recorded as an overall PASS. Events Host retest: 3/3 passed.
- Host-level fixture gates passed in that full Rust run: Codex restart drain, managed Codex replacement cleanup, ACP socket completion/report, and OpenCode completion/report.
- Dogfood protocol validation passed; Python protocol tests: 18/18 passed.
- Scoped environment mutation run initially stopped at an unmutated compile failure from concurrently incomplete Change edits; no mutation verdict was claimed from it. The library-only rerun passed its unmutated baseline and caught all 4 selected mutants (`names` empty/default and `lookup` None/default); no survivors.

## External gates

1Password MCP authentication timed out three times. No Developer Environment secrets were obtained by a fallback. Fabric Link restart with fresh runtime injection, dedicated Events sender deployment, actual callback delivery and registered ChatGPT Events are NOT RUN. The existing Link remains connected and uses its old in-process Host implementation.

Physical multi-host, Linux bubblewrap/userns, Devin provider, direct UI accepted-prompt hooks, GitHub/gh-stack delivery, jj/LFS/submodule/hooks and unobserved provider-specific acceptance are NOT RUN unless a later entry records execution. Unsupported capabilities remain explicit and never become execution, verification or delivery PASS. The legacy one-shot CLI remains available pending server parity acceptance.

## Final contract deployment

Both final profiles passed prebuilt dry run and local namespace preflight.
The source authority intentionally has no public target; the target deploy
reported the actual custom domain `temote.f12o.com`. Authenticated selected
plugin HostList and SessionInfo passed after the final deployment, and Access
still returned 401 to the unauthenticated health request. Namespace IDs and
D1 remained unchanged. The final full Fabric run passed 456/456 with zero
skipped tests (58.72 seconds).

## Host acceptance gates

Before the final frontend repair, full Rust tests: library 125 PASS, binary 1,033 PASS (two intentional
Host/provider tests ignored), all ordinary integration tests PASS. Clippy with
all targets/all features and `-D warnings` PASS; no-default-features all-target
check PASS; generated contract check 10/10 PASS. Later post-registration checks and final Host install identifiers are recorded below.

Ignored supervisor upgrade E2E: 3/3 PASS (86.46 seconds), including compatible
same-PID handoff, incompatible-generation rejection and explicit-force blocked
session behavior. Ignored session lifecycle E2E: 1/1 PASS (3.40 seconds).
Installed OpenCode 2.0.11 private registration gate PASS (0.54 seconds), after
fixing the private ancestors, modern V2 config/status envelope and waiting for
both connection and authenticated catalog observation. The initial failing
runs are not acceptance PASS. This test uses isolated state and no provider
turn; managed workspace build/test acceptance remains NOT RUN.

## Host installation and production handoff

`cargo install --path . --locked --force` passed. The canonical and compatible
executables and Linux helper were installed from source without advancing the
baseline package version `2026.8.0`; this is not an allocated CalVer release.
The previous installed executable and helper were backed up in a private
directory before replacement.

Candidate preflight reported two planned sessions and one blocked session:
`mbt` cannot be restored because captured `LANG` / `PATH` restart context is
unavailable or changed. The upgrade command rejected that blocker before
handoff; no forced stop or metadata repair was performed. Production handoff
is BLOCKED / NOT RUN, and supervisor PID 25452 retains runtime `2026.9.24`.
The separate existing Fabric Link also retains old in-process code; restarting
it needs authorized 1Password MCP runtime injection that is unavailable.

## Final frontend canary

An isolated private HOME and socket namespace, with the repository and a private provisioning directory as
explicit named roots, prevent test lifecycle operations from changing production
sessions. The installed Codex 0.160.0 native executable was used with the
existing runtime `CODEX_HOME` path; no credential file was read or copied.
The initial configured multicall `vp` symlink was unsuitable after executable
canonicalization and returned retryable startup failure. Those attempts are
not acceptance PASS.

Local task `50453977-46d6-50c0-9c27-666a090746e4` completed. MCP rediscovery
returned its retained terminal view with reconciliation deferred. A new MCP
continuation failed `CODEX_CONTINUATION_RUNTIME_OWNED`: the live runtime was
owned by the local supervisor. Repair task
`73e486f5-99cc-59ed-89da-06d848d741aa` was accepted through the same real Temote
stdio server to repair modern frontend routing. Executed repair acceptance is recorded below.

The local owner then reconciled the canary: native structured report was
`valid`, summary `LOCAL_FRONTEND_CANARY`, and scoped evidence was 2,032 bytes.
Execution was completed while verification remained `not_run` and delivery
`not_started`. This proves one installed Codex 0.160.0 native report turn, not
legacy/provider parity or cross-frontend control acceptance.

## Final supervisor routing repair and acceptance

Repair commit `2ed578b` routes modern MCP status/start/get/control/list and
explicit evidence reads to the same supervisor that owns local CLI runtimes.
Only successful negotiation with an explicitly older supervisor permits the
compatible direct MCP path. Negotiation/relay failures never start work at a
second owner. Private typed packets preserve full session instances, public
yolo denial, the caller's observation actor and distinct ordinary/local
64 KiB versus public 1 MiB input bounds. JSON escaping is accounted for.
Evidence records now bind the full instance and enforce the absolute 64 KiB
chunk cap. Configured multicall executable paths retain their invocation name
after resolved-target validation.

New relay regressions: 11/11 PASS, including a separate evidence-owning
process. At repair commit `2ed578b`, full Rust run: library 125 PASS, binary 1,047 PASS, two
intentional Host/provider tests ignored; all ordinary integrations PASS.
All-target/all-feature Clippy with warnings denied, fmt check, no-default
all-target check and diff check PASS. The full run includes all ten generated
contract snapshots; no public fingerprint change was introduced by this
private transport repair. Fabric's existing 456/456 final run remains valid.
The delegated editor's sandbox-denied Cargo attempt is NOT RUN; these central
Host checks are the actual validation.

Actual two-frontend Codex 0.160.0 canary PASS: local→MCP rediscovery/control,
MCP→local control, exact UUID replay without another turn, and a fully escaped
1 MiB changed request reaching the owner and returning OPERATION_CONFLICT.
Task `c4e2d3fa-bb65-51a0-9dff-f345b501eff9` continued as independent task
`24c0632b-2354-5888-90ff-6f7fecac5797` on the same thread in a fresh turn.
Both retained native reports were valid. The successor retained independent
verification `not_run` and delivery `not_started`; exact continuation replay
reused its task/turn, and source controls were fenced. Its 3,464-byte evidence
was read from the supervisor through MCP. All isolated sessions and this
supervisor were stopped gracefully after the canary. Production sessions
were unaffected.

## Managed-source canary and model selection

An actual isolated `session start --source f4ah6o/temote-mcp --vcs auto`
accepted operation `e367577e-f7a5-4390-a9f7-4e2cb59849cb` and retained task
`ce9313e6-0ec2-51d0-a7d2-87ce0f6ceecc`. The task completed with a valid native
report whose status was BLOCKED: required HTTPS fetch network approval was
rejected. jj 0.37.0 was available, but no repository, workspace or allocation
marker was created. Provisioning returned `workspace_readiness_invalid` and
never activated. This is BLOCKED / NOT COMPLETED, not acceptance PASS. Physical
fetch/replay, second allocation, ready environment and cache acceptance remain
NOT RUN.

The canary exposed an automatic-selection defect: lexicographic selection
could choose hidden auxiliary `codex-auto-review`. Codex's locally generated
ModelListResponse schema supplies `hidden`, `isDefault` and
`defaultReasoningEffort`. Status now preserves those optional values; preparation
excludes hidden entries, chooses one visible default and an advertised effort,
and rejects ambiguous defaults. Legacy inventories preserve catalog order.
The choice remains pinned in the durable receipt. No child sandbox/network
approval policy was changed.

After the supervisor relay repair, ignored supervisor upgrade E2Es were rerun:
3/3 PASS in 103.69 seconds, including compatible handoff, incompatible-generation
rejection and explicit-force behavior. This isolated fixture result does not
remove the production `mbt` blocker.

A fresh isolated post-repair operation
`085b4e81-4426-4554-96a6-34f524da9862` selected the advertised normal default
`gpt-6.1-sol` with its default `low` effort. Retained task
`07c7be3a-ebf2-500f-9423-c4d083eec7f7` completed with
`report_source: native_structured_output`, `report_status: valid`, and a failed
report. It initialized an isolated bare store and wrote a **failed** marker;
GitHub DNS/network fetch remained unavailable and the child approval was
rejected. No workspace or ready marker was created and provisioning never
activated. Model-selection acceptance passed; managed source acceptance remains
BLOCKED / NOT COMPLETED. All owned canary supervisors/sessions were stopped.
An intermediate retry in the original isolated store stayed pending behind its
existing accepted ensure claim; it was stopped without deleting that claim.
A first fresh-supervisor setup attempt omitted its named-root directory and
failed before any task; that setup attempt is not acceptance PASS.

The first full post-selection Rust run passed 125 library and 1,050 binary
tests (two intentional ignored gates), then all seven Activity CLI integration
fixtures timed out before connection. A no-change isolated Activity CLI rerun
passed; this initial overall run is not recorded as PASS. Final full rerun and
post-selection checks are recorded below only after execution.

## Final post-selection repository gates

Against code commit `b381d19`, the no-change full Rust rerun passed:
125 library tests, 1,050 binary tests with two intentional ignored gates, and
all ordinary integrations including all seven Activity CLI fixtures. The full
run took 57.18 seconds and included all ten generated contract snapshots.
All-target/all-feature Clippy with `-D warnings` PASS (44.35 seconds),
no-default-features all-target check PASS (18.83 seconds), fmt check PASS,
and diff check PASS. Public contract fingerprints were unchanged; this optional
backend status metadata does not require another Fabric redeployment.
Current selected-plugin HostList and SessionInfo reads passed again; they still
correctly show the old production Host runtime and active normal dogfood session.

## Final source installation

`cargo install --path . --locked --force` passed again after `b381d19`
(64 seconds), installing source-baseline `2026.8.0` without a release bump.
Installed SHA-256:

| Executable | SHA-256 |
| --- | --- |
| `temote` | `b3d6ba4c9b236581f908c5d8dda1a327094bc36bd727b5c09469c6e9d71638c0` |
| `temote-mcp` | `491697906353b18a6155acbf443fa8081ab8efab2980b51909948370565d5165` |
| `temote-linux-sandbox` | `5ac2728adfaaceaafb62f55d24bb44437f1c36c461d8f1a1fd6b903e6a46c6ec` |

Production supervisor and Link remain at their prior runtime because the
recorded restart-context/1Password gates remain unresolved; installing binaries
alone is not runtime handoff acceptance. The old private backups are retained.

## Final scoped mutation and document gates

The model projection/selection run tested five mutants against a green
unmutated baseline: 5 caught, 0 missed, 0 unviable and 0 timeouts (8 minutes).
These cover both removed status projections and hidden/default/advertised-effort
comparison changes. The generated catalog property includes empty model/effort
boundaries and unadvertised effort values. The first copy-target attempt was
interrupted before baseline; it has no mutation verdict. The completed run used
an isolated source copy and existing build cache, without in-place source edits.

All 55 normalized current issues passed the issue validator. The final branch
Markdown link check covered 96 files and 348 links with zero missing local
targets; an older archived relative link was corrected. Changelog structure,
duplicates and dates passed. No secret-shaped deployment profile or runtime
credential file is included in the branch.
