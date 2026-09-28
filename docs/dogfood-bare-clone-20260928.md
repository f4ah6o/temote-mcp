# Named-root bare clone: implementation and live dogfood

Date: 2026-09-28 (Asia/Tokyo). This change is implemented and the individual
clone → worktree → normal session → jj steps were verified live. **Neither
production nor the isolated end-to-end comparison qualifies:** production has
an missing required admission capability and the isolated run has an interrupted
final verification task. No release, merge, push, package version bump, or
production supervisor replacement was performed.

## Implementation

`repository_clone_bare` requires `session_id`, UUID `operation_id`, named
`root`, `source`, root-relative `destination`, `model`, and `effort`. Sources
are credential-free HTTPS Git URLs or logical local paths in the selected root.
The initial implementation uses the existing local Codex delegation backend.
There is no sessionless exception and Temote does not execute Git clone.

The supervisor uses its configured canonical root and owned active session
snapshot, under its transition lock. The session must be normal and its cwd
must equal that root. Absolute paths, traversal, existing destination leaves
(including empty directories and dangling symlinks), and symlink parents are
refused. Client environment overrides cannot authorize an unknown root.
Admission is checked before delegation and again after durable acceptance,
before child startup. The agent rechecks its parent and claims the destination
with exclusive `mkdir` before its one `git clone --bare` invocation.

Exact accepted retries reuse the retained task and UUID even after the leaf
exists. A different UUID cannot overwrite that leaf. Interrupted or uncertain
operations are reconciled through retained tasks and bounded session-owned
evidence; replay does not blindly launch another clone. Existing permission,
network, credential, and evidence boundaries remain in force.

Live testing exposed two integration problems, which were fixed:

- Lifecycle admission now recognizes a valid standard bare-backed linked
  worktree and reserves its validated canonical Git common directory. Reciprocal
  pointers are checked; managed-worktree authority and Git mutation brokers
  still require their existing primary-checkout layout.
- Accepted Codex runtimes tolerate transient unknown owner observations up to
  three consecutive observations. Verified inactive/replaced owners stop
  immediately, and persistent unknown stops fail-closed. This is tested behavior;
  it has **not** been shown to cure the live immediate interruptions.

The harness also distinguishes final assistant evidence from prompt/commentary,
uses authenticated HTTP for the existing lifecycle API, and records its extra
session selection and recovery calls. Documentation, the Temote MCP Agent Skill,
CHANGES.md, activity contracts and gateway contracts were updated.

## Identities and preserved inputs

The previous task was complete and the checkout was initially clean. The parent
is `94a72e7772e15a94026ae483eec0e68046017099` (main, PR #77). The independent
jj change is `xqnstsqnonxvzuzktortonqlzswmwvxq`, bookmark
`codex/20260928-bare-clone`, description
`feat: delegate named-root bare repository setup`.

Repository HEAD, jj working-copy commit/diff, and executed binary identity are
separate. Exact tested diffs, jj snapshots and operations are preserved under
`dogfood/runs/bare-clone-20260928/`; `candidate-monitored-tested.diff` identifies
the compiled product, and `candidate-selected-tested.diff` additionally
identifies the later CLI-only jj recipe and transport selection. The final
report/diff snapshot is recorded separately. Later edits are harness and docs,
not Rust binary changes. Package version `2026.8.0` is the repository baseline,
not a release version.

| Artifact | SHA-256 |
| --- | --- |
| baseline `temote-mcp` | `6dd1b03fea671941ae1690f100bbca0236cb91c097d69fedee20dd586b5266d1` |
| baseline sibling `temote-linux-sandbox` | `a4ecd610346e3c60f8048e277641ecce58f0a1253c18f50b5ebcf40c9fee8463` |
| final candidate `temote-mcp` | `27d81dd14fc9e927c1058778c57fda185ce42a8279c5a4edef0c4f0d78dc59cd` |
| final candidate sibling `temote-linux-sandbox` | `e6a10d79fe643996b95cfdaf0ca714a09db7f446ec6b5bd8ddd14cb354f068e4` |

Both binary bundles retain their Linux sibling helper. Scenario
`repository-setup` revision 1 was added **before product edits**; fingerprint
`3de3ec404c361af3e1822f6cc28bf9f7d58f18024dbb12e4902cae3d02d7c5d9`.
Public candidate contract fingerprint:
`620494dd320bc163aeaf16d062c8b783f5040f309f651696ca91f75bd4506606`.

Doctor's jj check passes for both versions (jj 0.45.1), as do Linux helper,
bubblewrap and user namespace prerequisites. Two existing warnings concern
orphan metadata and absent Devin Cloud credentials. The previous doctor
improvement is a prerequisite and is not attributed to this feature.

## Production observation

The user's preparation session was inspected first: `repo-setup`, active,
canonical cwd `/home/hirohito-fujita/src`, permission `agent`, non-yolo,
ambient Git credentials disabled. It was reused for admission observation.
Its final PID `3259657`, start `1790562797`, and restart count 0 match the
initial inspection. No duplicate root preparation session was made, and no
user session was stopped or restarted.

`baseline-production-final.json` fails in 2 calls because the tool is absent.
`candidate-production-monitored.json` is blocked in 2 calls with
`REPOSITORY_CLONE_SUPERVISOR_UNAVAILABLE`. The installed 2026.9.23 supervisor
cannot authoritatively admit this new operation. The candidate refuses before
starting any agent; it does not silently replace that supervisor.

Read-only upgrade preflight found 60 planned sessions and 6 non-restorable
sessions. It also requires direct HTTP ingress replacement and client/plugin
reconciliation. Upgrade was not applied. These deployment blockers are retained
in `upgrade-preflight-provisional.json`; they are not test successes.

`comparison-production-final.json` compares equivalent high-effort Codex/stdio
profiles: **qualification blocked, improvement not_evaluated**.

## Isolated live observation

An owned fixture beneath the configured physical `src` directory was assigned
its own generic named root `fixture`, state/runtime directories and supervisor
socket namespace `df9d4073a42f`. This is a separate root, not another preparation
session for the user's `src` root. Owned fixture supervisors/sessions alone were
replaced when testing rebuilt binaries. Production remained untouched.

The logical source was `fixture/source`, seed HEAD
`e8736de2170a61f0c8d2a6cbc358d13fc20ba6b5`.
The client supplied no host absolute paths. Unique bare destinations use
`temote-dogfood-<run UUID>.git`, with `.wt/development` inside each bare directory.

Equivalent final baseline/candidate conditions were Codex `gpt-5.6-sol`, effort
`medium`, permission `agent`, named-root-local source, 200 bounded polls,
1-second interval, stdio delegation and authenticated HTTP lifecycle. The local
OAuth fixture used PKCE S256 and one explicit approval on its owned console.
Tokens stayed in memory/environment and were not recorded or passed as CLI
arguments. These local-source runs do not measure remote authentication or
network approval behavior.

Final paired observations:

| Run artifact | Calls | Result |
| --- | ---: | --- |
| `baseline-isolated-http-reconciled.json` | 2 | FAIL: absent tool; downstream steps NOT RUN |
| `candidate-isolated-http-reconciled.json` | 171 | FAIL: final read-only verification task immediately interrupted |

Candidate run UUID: `027938bc-00fb-4606-a7ce-909b7b68931b`.
Bare clone, exact replay after reconnect, worktree creation, normal session,
jj development and host admission assertions pass in that run. Clone was
accepted once and exact replay returned the same task ID. The final verification
failure leaves its `no_duplicate_clone` scenario assertion NOT RUN, even though
the separate gate verifies no duplicate destination.

| Logical operation | Candidate calls |
| --- | ---: |
| inspect preparation session | 1 |
| bare clone | 1 |
| clone terminal polling | 30 |
| read completed evidence | 3 |
| exact clone replay | 1 |
| worktree creation/polling | 40 |
| normal session start/info | 2 |
| select normal session and develop with jj/polling | 85 |
| admission probes and final verification start/poll | 8 |

Poll timing is retained, not selected as an improvement target. No call-count
reduction is claimed. Independent gates add 6 admission calls and 2 repository
inspection calls. Supplemental read-only recovery completed in **92 calls**, recorded separately
in `verification-recovery.json`, with a final `TEMOTE_SETUP_VERIFIED` report. It
never starts a new clone or reclassifies the 171-call failure. The measured
final flow therefore needed 171 calls plus 92 recovery calls; the independent
admission/repository gates add 8 calls. Earlier failed attempts and fixture
OAuth/setup/doctor steps are separate observations, not hidden in this total.

`repository-reconciled-independent-gate.json` passes all 12 checks: bare status,
source HEAD, exactly one linked worktree, canonical Git common directory,
expected file text, jj description and only the intended diff, no config-id,
one destination for this UUID, active normal scoped session, complete retained
task listing and observed exact replay. `independent-admission-monitored-gate.json`
passes client root spoofing, symlink parent, dangling leaf and credential URL
refusal with zero new delegated tasks. The scenario additionally refused lexical
escape, absolute destination, existing leaf with a fresh UUID and a non-root
worktree session; task listings remained unchanged.

`comparison-isolated-final.json`: **qualification blocked, improvement
not_evaluated**, despite these passing independent functional gates. Repeated
immediate Codex interruptions are still unproven in cause. Neither bounded
monitor retries nor fresh session transport selection are claimed as measured
reliability improvements.

## Failure and recovery history

All failed observations remain immutable; they were not replaced by later runs.
Earlier profiles used effort `high` and are not mixed into the final medium pair.

| Candidate artifact | Calls | Observation |
| --- | ---: | --- |
| `candidate-isolated.json` | 3 | immediate clone interruption, no destination; 3-call exact-ID reconciliation retained one task, no duplicate |
| `candidate-isolated-2.json` | 38 | clone completed; harness rejected Codex `final_answer` phase; parser fixed |
| `candidate-isolated-final.json` | 85 | clone/worktree completed; stdio lifecycle start unavailable by design; existing HTTP lifecycle used subsequently |
| `candidate-isolated-http.json` | 117 | bare-linked session admission rejected; product lifecycle identity fixed |
| `candidate-isolated-http-v2.json` | 120 | clone/worktree/session passed; first jj task interrupted without artifacts; terminal/no-effects reconciliation preceded a new jj-only operation |
| `jj-recovery.json` | 184 | supplemental jj task completed; original failure unchanged |
| `candidate-isolated-http-v3.json` | 409 | all development steps passed; final verifier failed because persistent jj repo config needed an external writable host directory |
| `candidate-isolated-http-final.json` | 89 | with CLI-only config, first jj task immediately interrupted |
| `candidate-isolated-http-selected.json` | 56 | with fresh session transport selection, worktree task immediately interrupted |
| `candidate-isolated-http-reconciled.json` | 171 | all development/admission steps passed; final verifier interrupted |

An exact clone retry may replay its original `running` acceptance receipt;
`codex_task_get` supplies actual terminal state. The receipt is not evidence
that an interrupted task is currently running. 256 later active socket probes
passed, but do not establish the original interruption's cause.

## jj scope and extra steps

jj 0.45.1 rejects colocation in a linked Git worktree. Its repo/workspace secure
configuration can also require host writes outside a normal worktree session.
The measured recipe uses four preparation commands for an independent Git
backing repository inside the worktree, explicit tracking of only the change
file, and the literal `--config 'snapshot.auto-track="none()"'` argument on
every jj invocation. It does not persist repo/workspace config or create
config-id files. Verification adds `--ignore-working-copy` and the same CLI
setting. See [the operational recipe](self-improvement-dogfood.md#bare-repository-setup).

`jj-cli-config-sandbox-proof.json` and `jj-cli-config-development-proof.json`
separately demonstrate initialization, tracking, description and diff with every
other host path read-only. The final live repository gate confirms that layout.
This adds preparation/tracking/config steps, plus the extra MCP normal-session
selection call. Changes are not automatically exported to the shared original
bare repository; no out-of-scope metadata grant was added.

## Verification

| Gate | Result and evidence |
| --- | --- |
| `cargo fmt --all -- --check` | PASS, `fmt-monitored.log` |
| `cargo test -- --test-threads=8` | PASS, `cargo-test-monitored.log`; ignored gates listed below |
| `cargo clippy --all-targets -- -D warnings` | PASS, `clippy-monitored.log` |
| `cargo check --no-default-features --all-targets` | PASS, `no-default-monitored.log`; existing feature-disabled dead-code warnings retained |
| gateway `npm test` | PASS, 101 tests, `gateway-final.log` |
| Python harness | PASS, 18 tests; seven scenarios validate |
| Linux sandbox acceptance | PASS, 18 normal checks plus explicit pinned-descriptor path-swap gate, `linux-sandbox-final.log` |
| ignored session process E2E | PASS, all 4, `session-process-e2e.log` |
| clone mutation/property tests | PASS after fixing survivors, 23 caught / 3 unviable in final iterate; initial missed results retained |
| lifecycle identity mutation scope | PASS, 2 caught |
| monitor observation mutation scope | PASS, 1 caught / 1 unviable |
| three-pass diff review | reviewed correctness, boundaries/errors, tests/integration; `review-three-pass.json` |
| `git diff --check` | PASS; final recorded snapshot/check preserved |
| live OpenCode provider E2E | NOT RUN; provider unchanged, this feature is Codex-only |
| real direct-HTTP upgrade/reconnect E2E | NOT RUN; production upgrade blocked and user sessions preserved |
| production admission/restore | BLOCKED; old supervisor capability / six non-restorable sessions |
| full isolated scenario reliability | BLOCKED; reproducible immediate interrupted task |

Mutation unviable cases are generated `Default` substitutions for types without
`Default`; their compiler failures were inspected rather than counted as kills.
Clone admission also has generated path/source properties and a 32-case durable
prepare → accepted task → exact retry → new-ID collision round-trip. An earlier
full-suite probe flake was retained, isolated rerun passed, and the final full
suite passed; it was not silently omitted.

The measured remaining constraints are Codex-only delegation, local-source live
coverage, no automatic export from jj's worktree-local backing store, no cleanup
of partial clone failure, unproven immediate interruptions and blocked production
rollout. Static symlink rejection and exclusive leaf claiming do not eliminate
same-user filesystem races between validation and a delegated agent action;
this API cannot pass a pinned directory descriptor through its typed task.
Ignored owner-local artifacts retain hashes and bounded non-secret observations;
child transcripts remain bounded, expiring session evidence.
