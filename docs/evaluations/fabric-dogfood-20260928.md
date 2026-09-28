# Fabric metadata dogfood, 2026-09-28

Scope: metadata automation, live local dogfood, and authenticated Cloudflare
metadata verification. Cloudflare authorization was completed after the initial
blocked assessment below. A dedicated Access-protected hostname is now deployed.
The user subsequently confirmed Managed OAuth login as an allowlisted user.
A subsequent plugin verification connected the host agent and completed a
read-only delegated task through Fabric; see the plugin-verification section.
Persistent host startup was configured afterward, as recorded below.

## Identity and observations

The checkout started clean on `deploy-fabric`, HEAD
`94a72e7772e15a94026ae483eec0e68046017099`, with no upstream. No commit or push
was performed. Exact candidate changes and owner-local observations are retained
under ignored `dogfood/runs/fabric-20260928/`.

The baseline binary and sibling Linux sandbox helper were built before source
changes and preserved in `baseline-bin/`. Both baseline and candidate doctor
reported zero failures. Codex app-server 0.147.0 advertised the selected
`gpt-5.6-luna` / `max` profile; the locally installed backend did not advertise
GPT-6 models.

The successful live `delegation-lifecycle` revision 1 runs used the same existing
normal session and 200 polls at a one-second interval:

| Observation | Baseline | Candidate |
| --- | --- | --- |
| Binary SHA-256 | `1c7c94c634ab6349e4961297138754033381099de92eb2724e13d31753005143` | `a100a1dd8f2fd1bf08de031e344666e157333d28d31c89d930f47bde888e7505` |
| Assertions | all 3 pass | all 3 pass |
| Result-read calls | 1 | 1 |
| Polls | 35 | 28 |
| Duplicate starts / ambiguous terminal states | 0 / 0 | 0 / 0 |

`local-runtime-comparison.json` targets `read_terminal_result`: `qualified`, `unchanged`.
`full-loop-final-comparison.json` adds blocked deployment and unrun authenticated
remote tools gates: `blocked`, `unchanged`. Poll variation is visible but was
not selected as an improvement target. An initial interrupted run and a run
blocked at the default 20 polls are retained separately; neither is a pass.
The capped retry was still running in its immutable run snapshot. A later
`final-task-states-after-candidate.json` read-only listing confirms all five
tasks are terminal: three completed and two interrupted after the earlier
owners closed. The first passing candidate run is retained separately; the
final run uses the preserved binary from the completed regression build.

## Metadata loop

Runtime stdio `tools/list` observation found 25 local tools. The candidate changed
only `poll_job` and `stop_job` descriptions, removing references to retired
command tools. The Rust public managed-session projection adds lifecycle and
host-routing metadata, generating 30 Fabric tools without changing the existing
structural contract fingerprint:
`ef6818b96867949403ec51ed29b92f10a9256d6172c9601215ac090607fdc78e`.

Fabric now imports the generated metadata directly. Two `just generate-tools`
runs produced identical artifact bytes. The full metadata SHA-256 was
`f260e9ed9b2e192009e8af3feedb87e0c0119da8abad5d09158f8b0167d41a9f`.
Temporarily changing a generated description caused `just check-generated` to
fail with `gateway tool metadata is stale`; after restoring exact bytes, it
passed. Generator fixtures cover add/remove/rename, prose updates, malformed
source, stable key ordering, and retention of schema constraints.

## Verification

- Dogfood scenario validation and 13 protocol unit tests: pass.
- All six fixture scenarios: pass except release qualification, correctly blocked
  without independent CI/action gates.
- Live baseline and candidate, candidate build and both doctors: pass.
- Generated-state checks (4 tests), repeated generation and deliberate stale
  rejection: pass.
- Node 22.23.3 gateway tests (101 before the runtime fix), Node 24.21.0
  final gateway tests (102 after the fix), and SQLite D1 tests (10): pass.
- Wrangler 4.142.0 pinned install and deploy dry-run: pass; final bundle 125.49 KiB,
  gzip 24.37 KiB. This does not validate remote resource readiness.
- `cargo test --locked -- --test-threads=1`, format, Clippy with warnings denied,
  no-default-features all-targets check, and diff whitespace check: pass.
- The initial parallel Rust suite failed two existing lifecycle timeout tests;
  both isolated reruns and the final complete serial suite passed. No lifecycle
  implementation was changed. Ignored host/provider/release tests were not run.

## Local Worker runtime correction

Starting the bundled Worker in workerd exposed an invalid numeric named export
of `OBSERVATION_SCHEMA_VERSION`. A bundle dry-run alone had not detected it.
The constant remains in its internal observation module; removing only its
Worker-entrypoint export made the runtime start. A focused regression check
keeps the current named helpers and Durable Object exports callable.
Local `/healthz` and authenticated `/mcp` `tools/list` both returned HTTP 200;
the structural fingerprint matched and the returned list deeply equaled all
30 generated tools, including descriptions and schemas.
The local-only dummy credential was not published, and the Worker process was
stopped after the smoke check. This is local runtime evidence only.

## Initial Cloudflare boundary

The selected target is `temote.obr-grp.com`. Public DNS returned ENODATA,
and authenticated record ownership remains unverified because the DNS API is
not authorized. Read-only probes found no Worker route/custom-domain connection
for this service, no configured Worker secrets, and no `temote-observation` D1
database. The checked-in configuration
remains a generic template with Access/owner placeholders and a sentinel D1 ID.
DNS and zone Access API probes returned HTTP 403, code 10000; the existing
Wrangler authorization lacks DNS Read and Access app/policy Read permissions.
Access Write is also needed if protection must be created or changed; the
zone-level application state could not be verified. Existing authorization
already supports Worker deployment/routes and D1 writes.
No remote resources, routes, secrets, migrations or deployment were changed.
The existing deployment ID `85795af5-6fd9-46e3-8deb-5cc87ee41b33` and version ID
`0eaf413d-4c42-47b9-9983-796b473cfbfe` predate this change and are not candidate
deployment evidence.

Before resuming, configure Access protection and host service-token policy for
the selected hostname, provide authorization for DNS/Access and the Worker
connection, provision D1 and apply its additive migrations, set owner/Access
vars and the host-token secret, and run the documented deployment checks.
Authenticated remote `tools/list`, candidate deployment ID, endpoint smoke and
remote runtime-log verification remain unrun.

Final gateway and dry-run results are retained as tool-output summaries rather
than raw logs; the local smoke artifact is also a bounded summary. Raw Rust
check logs, immutable live runs/comparisons, and the exact candidate patch with
untracked artifacts are retained locally.


## Cloudflare deployment after plugin authorization

The authenticated Cloudflare plugin verified target DNS ownership: no existing
record used `temote.obr-grp.com`, and no other Worker owned that hostname. A new
self-hosted Access application protects the whole hostname, reusing the existing
owner-only human policy without modifying it. Managed OAuth is enabled using
the existing client-registration convention. A separate, dedicated Service Auth
policy and service token are configured for host connections.

D1 `temote-observation` was provisioned in APAC with ID
`83e896c8-393e-41b8-b2a7-f686a8a108fc`. The two checked-in additive migrations
were applied through the authenticated Cloudflare API and recorded in
`d1_migrations`. Wrangler's separate OAuth credential returned code 7403 for
D1 queries; this did not block the plugin API. Existing Durable Object namespace
IDs were preserved, and no existing DNS record or application was deleted.
The selected observation owner namespace is `temote-primary-owner`.

The non-secret deployment configuration is retained locally as
`dogfood/runs/fabric-20260928/wrangler.remote.toml`; the checked-in configuration
remains generic. Host and Access credentials are outside the repository with
owner-only permissions; their values are not included in these records.

After validating the real configuration with a dry-run, the deployment command
from `gateway/` was:

```sh
npm run deploy -- --config ../dogfood/runs/fabric-20260928/wrangler.remote.toml --domain temote.obr-grp.com --keep-vars
```

The uploaded deployment was `0fdbfcbd-6957-4220-b566-c5a5e9ed9570`, version
`5020cd8f-5733-44b0-8cb2-366b9a2c9e1e`. Secret cleanup produced final deployment
`a4383218-46e8-4281-8eac-79caf0802d8a`, version
`131bd4ce-5b13-4423-b90b-9c5f605b33ac`, at 100%. Both versions have identical
script ETag `59e315ebd19a1826578a62d02615f286f844835de7626e1dae7e8ce351ca49ed`.

Authenticated `https://temote.obr-grp.com/healthz` returned HTTP 200 and the
expected fingerprint. Unauthenticated health returned 401. Authenticated
`/mcp` `tools/list` returned HTTP 200 and exactly the 30 generated metadata
entries, including every description and input schema. That metadata smoke
used a dedicated Access service token plus the existing `CLIENT_TOKEN` origin
capability temporarily; the temporary secret was removed immediately afterward.
Final Worker bindings contain no `CLIENT_TOKEN`, and the final health remains
200 while a service token alone receives 401 from `/mcp`. Production user MCP
authentication retains the email/sub/issuer/audience/signature checks.

Both public OAuth discovery endpoints also returned HTTP 200 JSON with the
expected authorization server. This is deployed metadata/discovery proof,
not an interactive end-user OAuth proof.
Service-token JWTs lack the user identity required by `/mcp`; end users must
complete Managed OAuth. No host agent is connected yet. Bounded tail observation
recorded two successful Worker events with zero exceptions; raw request headers
were discarded and only sanitized tail summaries retained. The default Python
HTTP user agent initially received an edge 403; the explicit smoke user agent
succeeded without any change to Access or security settings.

`cloudflare-deployment.json`, `remote-smoke.json`, `remote-after-cleanup.json`,
and the new `full-loop-cloudflare-comparison.json` retain the deployment
evidence. The earlier blocked comparison remains immutable. The new comparison
qualifies the local/runtime/deployed-metadata loop as unchanged; it does not
claim release qualification or a completed user OAuth flow.


## Subsequent user confirmation

The user confirmed that Managed OAuth login succeeded as an allowlisted user
on the deployed endpoint. This is user-reported verification; the agent did
not independently replay the login or inspect authentication tokens. It closes
the previously outstanding human login check without changing the historical
metadata-smoke or comparison evidence. The confirmation does not establish a
connected host agent or delegated task execution through Fabric.

The owner-local `managed-oauth-user-confirmation.json` records this observation
without identity or credential values. Existing immutable artifacts retain
their original `not_run` state for the agent's own OAuth test.


## Temote plugin verification after handoff

The selected `temote` plugin exposed all 30 generated tool names in the current
client catalogue. Its first `host_list` call succeeded and returned an empty
list. This distinguished a callable authenticated MCP endpoint from the earlier
conversation where plugin tools were unavailable.

Read-only local inspection found the existing supervisor and the active normal
`fabric-dogfood-20260928` session in this checkout. The supervisor already
advertised the named root `src`; neither it nor any session was restarted.
The preserved candidate binary started a detached host-level gateway agent
(PID `4097290`) with the existing owner-only credential file, passing secrets
only through its environment. It connected to `https://temote.obr-grp.com` as
`ms-01-alpha`, generation 1, protocol compatible, with session availability
`ready`. The agent remains running; no boot-time service was configured.

Through the selected plugin, explicit host routing succeeded for `host_list`,
`session_list`, `session_info`, `context_resolve`, and `codex_status`.
The session scope was `src/temote-mcp-fabric`, permission mode `agent`, with
`yolo: false`. Context showed no active or attention tasks. Codex app-server
0.147.0 advertised `gpt-5.6-luna` / `max`, matching the earlier live profile.

A single read-only status task was accepted with operation ID
`f64cab9f-abfd-4c0d-9d01-f47ac4e0ce57` and task ID
`7622b9e8-6b70-5e77-aec3-c109201b8a75`. Three `codex_task_get` polls observed
running, not-modified, then completed at revision 4 with no reconciliation
required. The terminal evidence reference was read directly in one
`evidence_read` call: 2,371 bytes, complete and untruncated. It reported branch
`deploy-fabric`, HEAD `94a72e7772e15a94026ae483eec0e68046017099`, 13 modified
tracked files and 3 untracked files, matching independent local inspection.
A final `host_info` confirmed the same generation and readiness.

This proves authenticated plugin tool calls, host routing, task execution to a
terminal state, and bounded evidence retrieval through the deployed Fabric.
The client catalogue's tool-name set matches the generated set; a separate raw
user-authenticated `tools/list` response was not captured, so this does not
replace the earlier full metadata-equality smoke. No user authentication token
was inspected or replayed, and no authorization check was changed.

The owner-local `fabric-plugin-verification.json` retains a bounded non-secret
summary. Earlier immutable run artifacts remain unchanged. Only this evaluation
was edited after the delegated read-only check; `git diff --check` passed.
No commit or push was performed, and no code or deployment changed in this
continuation. The earlier full-suite results were not rerun for this record-only
update.


## Persistent host startup

At the user's request, boot-time startup was configured with systemd user
services outside the repository. The existing user already had `Linger=yes`.
The persistent `~/.config/systemd/user/temote-session-supervisor-20260809.service`
was enabled using its absolute file path. The active transient unit with the
same name remains in control until the user manager restarts; its process
(PID `3259657`) and all existing sessions were left running. The persistent
boot definition uses the existing installed `~/.cargo/bin/temote-mcp supervisor`
command, the named root `src=/home/hirohito-fujita/src`, and the existing PATH.
No internal one-shot upgrade restore plan was copied into the startup command.

The new enabled `temote-fabric-agent.service` depends on that supervisor,
uses `Restart=always`, a five-second retry delay, and SIGINT for graceful
shutdown. The verified candidate binary and sibling sandbox helper were copied
to `~/.local/lib/temote-mcp/fabric/<candidate-sha256>/` so startup does not depend
on this checkout's ignored run directory. Secrets remain in
`~/.config/temote-mcp/fabric-agent.env` with mode `0600`; supervisor configuration
is in a separate `0600` environment file. Neither file is tracked in Git.

Both unit definitions passed `systemd-analyze --user verify`. The previous
manual gateway agent was stopped with SIGINT and replaced by the running
systemd service. Plugin `host_info` confirmed `ms-01-alpha` ready and compatible
at generation 2. A controlled SIGINT then exercised automatic restart:
`NRestarts=1`, followed by a ready, compatible generation 3. Plugin session
snapshots before the switch and after the restart contained the same 68
sessions, with unchanged status and supervisor PID. The bounded service-journal
check found no error records. Environment-file permissions and both boot
symlinks were checked.

Inspect the agent with `systemctl --user status temote-fabric-agent.service`
and follow its logs with `journalctl --user -u temote-fabric-agent.service -f`.
The machine was not rebooted for this verification. Automatic startup does not
restore the previous in-memory sessions after a machine reboot; a new session
must be started through the normal lifecycle tools. The current supervisor's
transient status is expected until the next user-manager start, when the
persistent definition is selected. No commit or push was performed.
