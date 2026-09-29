# Fabric dashboard

The read-only dashboard lives at `/dash/` on the existing Temote Fabric Worker and hostname. `/dash` redirects after authentication. Cloudflare Access protects the whole hostname; the Worker independently verifies the assertion signature, issuer, audience, expiry, subject, and allowed email. Client and host bearer tokens cannot open the dashboard. Existing MCP client-token compatibility is separate.

The dashboard uses configured host membership from `HOST_TOKENS_JSON`. Token values are never displayed. An offline entry means Fabric has no confirmed live route, and unknown means discovery could not establish availability. Neither describes a machine shutdown or a disconnection time. Connection history without evidence remains unknown. “Last synchronized” describes observation replication, rather than a live backend check.

Session and task panels use existing bounded Host reads. Task state is a retained record; a reachable Host does not establish that its backend state was just reconciled. Pending interaction summaries are produced on the Host independently of dashboard access and task lists. They expire after 30 seconds. A missing summary from an older Host is unsupported, and an expired or failed observation is unavailable. Approval details, question text, choices, prompts, output, environment values, and raw evidence are excluded.

Context uses the live resolver when available. Without a usable live or offline resolver the component is unavailable. Timeline contains allow-listed metadata from replicated observations, scoped to the configured owner, selected Host, and session. A replica is never used to infer current execution state.

Foreground refresh is five seconds and background refresh is thirty seconds. Manual refresh is available. Selection is stored in the URL query/hash, without task/session storage in localStorage. Partial failures remain visible per component. Offline or unknown Hosts make previous live information stale.

## Deployment and verification

Follow [the existing gateway deployment procedure](gateway.md#deploy). Keep the same Worker, Durable Object bindings, hostname, and Access application. Include `gateway/assets/` and the authenticated Static Assets binding from the candidate configuration. Worker-first routing protects asset aliases as well as the documented paths; HTML and unknown-path fallback are disabled. `workers_dev` stays disabled. The checked-in Access and D1 placeholders are templates and cannot establish remote readiness.

Build and check the candidate Host binary before an approved Host update. Installing a binary alone does not update an already running supervisor or runtime owner. Do not restart production Hosts or active tasks without the existing operator approval and lifecycle procedure.

After local tests and a dry-run, obtain approval for the candidate deployment against the existing target. Use an allowed human browser through Access to open `/dash/`. Verify unauthenticated rejection at the edge separately from Worker-side JWT rejection. Confirm hosts, sessions, tasks, pending summaries, context, timeline, stale/offline presentation, the unchanged MCP fingerprint, health semantics, and continuing observation sync. Record the exact candidate HEAD, Worker version, and Host binary identity. Fixture, local workerd, and dry-run checks do not replace this deployed gate.

If rollback is required, restore the previous Worker version/assets and approved Host binary through the existing procedure. Keep Access protection, DNS, observation data, and unrelated routes intact. Summary metadata is additive; older Hosts yield unsupported summaries. No data migration or release version bump is part of dashboard installation.

The [acceptance matrix and gate record](evaluations/fabric-dashboard-20260929.md) tracks implementation separately from deployed acceptance, CI, and independent review.
