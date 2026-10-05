# Fabric MCP Events

The modern Fabric MCP endpoint implements the [MCP Events webhook contract](https://developers.openai.com/plugins/build/mcp-events). The initial catalog is `job.state.changed` and `session.state.changed`. Filters require `host_id` and `session_id`; job events also accept `job_id`. The legacy MCP endpoint remains unchanged. The protocol cursor is always `null`; there is no replay claim.

Events are advertised only for client-token callers when the D1 tables and authenticated dedicated sender health check succeed. Cloudflare Access JWT callers are not offered durable Events yet: an allowlisted email alone cannot prove that a JWT subject still has access at delivery time. This fails closed until a durable Access revocation authority is integrated.

## Runtime wiring

Apply `fabric/migrations/0005_mcp_events.sql` to the existing `OBSERVATION_DB`. Configure these Worker bindings through the normal deployment secret mechanism:

- `EVENT_SENDER_URL`: fixed HTTPS URL ending in `/events/send`, through an Access-protected Cloudflare Tunnel.
- `EVENT_SENDER_ACCESS_CLIENT_ID` and `EVENT_SENDER_ACCESS_CLIENT_SECRET`: Access service-token secret bindings.
- `EVENT_SENDER_BEARER`: separate random bearer secret, at least 32 characters, shared with the dedicated Host sender.

Mount `events_sender::router(bearer)` on a dedicated Host HTTPS listener behind that Tunnel. The sender has only authenticated `/events/health` and `/events/send` routes. It accepts bounded verification or single-event delivery envelopes, validates every resolved address, pins the validated socket while retaining the callback hostname for TLS, disables proxies and redirects, and signs the exact bytes sent. Never mount it as a general request forwarder or expose its bearer through metadata, logs, or ordinary tool output.

The Worker `scheduled` handler sweeps the D1 outbox. The deployment scheduler must invoke it periodically. A transition capture also attempts a bounded immediate sweep. Sender or Host unavailability leaves entries pending until their finite expiration; actual callback failures receive at most eight attempts with exponential backoff. HTTP 410 and 413 terminate delivery. Outbox event IDs are stable across retries and the sender signs each attempt with a fresh timestamp. The default subscription is 24 hours; finite requests are capped at 7 days, and `ttlMs: null` receives a finite 24 hours. A replacement secret is dual signed for five minutes.

## Authoritative transitions

Fabric records observed `session_info`, `job_list`, and `poll_job` responses only when they come from the authenticated Host route. The Host Link also follows the supervisor's live lifecycle activity stream. On a completed start, stop, restart, or automatic restart, or an observed crash, it reads the authoritative session view and enqueues the matching transition. The existing `stop_job` mutation enqueues a stopped job transition after the owner aborts the job. Enqueued records are private, bounded to 256 per Host, and retained for at most 12 hours; they contain no Fabric credentials or transcript. A Host event can therefore expire before a subscription's default 24-hour lifetime if the Link stays offline. While connected, the Link adds its current generation and instance ID and sends them to `/v1/hosts/{host_id}/events/transition` with its normal federated Host credentials. Pending entries are retried after connectivity returns. A 409 preserves an entry unless the Host can prove its full session scope was replaced; retry never synthesizes a current transition.

The POST body is bounded:

```json
{
  "name": "job.state.changed",
  "session_id": "session-a",
  "job_id": "job-a",
  "state": "completed",
  "timestamp": "2026-10-05T00:00:00Z",
  "generation": 1,
  "host_instance_id": "host-instance",
  "session_started_at": 1700000000,
  "session_process_id": 1234,
  "session_restart_count": 0,
  "session_permission_mode": "agent",
  "session_cwd": "/canonical/workspace",
  "session_permitted_directories": ["/canonical/workspace"]
}
```

Use `session.state.changed` without `job_id` for session lifecycle changes, including stopped sessions. The gateway verifies the active Host generation and instance, then compares the full retained session process and canonical scope identity against live authoritative `session_info`. Missing or degraded authority returns 409 and never creates a terminal event. A replacement session with reused IDs cannot receive predecessor outbox entries. The Host must emit job completions from its actual job transition path; Fabric does not start a polling loop to synthesize them.

The legacy in-memory job table currently has no production start/completion writer, so Host-originated `running`, `completed`, and `failed` job transitions cannot be demonstrated from that path. Read-observed job transitions still use the authenticated Host responses above. A lifecycle activity-stream gap is reported and reattached without inventing missed transitions; a stale offline transition is rejected by the Worker's live instance check. The live ChatGPT Work callback and reaction test requires external credentials and is **NOT RUN** here. An Access principal revocation integration is also outstanding before Events can be advertised to Access JWT callers.
