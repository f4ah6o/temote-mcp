# Fabric: support MCP Events for Temote job/session state changes

Status: open
Repository: `f4ah6o/temote-mcp`
Related:
- `issues/open/20261001-fabric-openai-mcp-extensions.md`
- `issues/open/20260927-bounded-wait-for-delegated-tasks.md`
- `issues/open/20260926-temote-fabric-product-boundary.md`
Created: 2026-10-05 (Asia/Tokyo)

## 1. Goal

Support MCP Events at the Temote Fabric MCP boundary so MCP clients such as ChatGPT can subscribe to state changes instead of repeatedly polling `poll_job`, `job_list`, or `session_info` for terminal transitions.

Primary reference:

- https://developers.openai.com/plugins/build/mcp-events

OpenAI's current ChatGPT integration requires MCP 2.0 / protocol version `2026-07-28`, webhook delivery, durable subscription state, and outbound HTTPS access to callback URLs.

The repository already implements the modern `2026-07-28` protocol path and `server/discover`. This issue therefore extends the existing modern MCP path rather than introducing another protocol mode.

## 2. Product boundary

MCP Events is a delivery/subscription adapter over existing Temote/Fabric state. It must not create another independent job/session state machine.

```text
Temote Host / execution backend
        |
        | authoritative job/session transitions
        v
Fabric projection / routing
        |
        +--> existing read/control tools
        |
        +--> MCP Events adapter
               |
               +--> durable subscriptions
               +--> filter matching
               +--> signed webhook delivery
```

The Host remains authoritative for live execution state. Fabric may observe/project transitions and deliver them, but an event delivery is not itself the source of truth.

Do not make Fabric generally OpenAI-specific. Keep the event catalog and internal event envelope provider-neutral; isolate ChatGPT-specific webhook requirements in the MCP Events adapter.

## 3. Protocol surface

### 3.1 Discovery

For MCP paths that can actually provide durable subscriptions and outbound webhook delivery, advertise:

```json
{
  "capabilities": {
    "tools": {},
    "events": {}
  }
}
```

Do not advertise `events` on a path that cannot honor the complete lifecycle.

Legacy MCP behavior remains unchanged.

### 3.2 Methods

Implement on the same authenticated MCP endpoint:

- `events/list`
- `events/subscribe`
- `events/unsubscribe`

`events/list` must return stable event names, descriptions, supported delivery modes, `inputSchema`, and `payloadSchema`.

If the catalog becomes paginated, return `nextCursor` and accept `cursor` on subsequent list requests.

## 4. Initial event catalog

Start with the state changes that remove active polling from normal Temote orchestration.

### 4.1 `job.state.changed`

Suggested subscription arguments:

- `session_id`: required
- `job_id`: optional

Suggested payload:

- `session_id`
- `job_id`
- `previous_state`
- `state`
- `timestamp`
- optional bounded terminal result/error summary
- optional locator needed by existing read tools

The event must cover terminal states as well as meaningful intermediate transitions, but high-frequency internal progress should not become an unbounded event stream.

### 4.2 `session.state.changed`

Suggested subscription arguments:

- `session_id`: required

Suggested payload:

- `session_id`
- `previous_state`
- `state`
- `timestamp`

### 4.3 Payload rule

Keep event payloads compact. Large logs, patches, artifacts, transcripts, and full results remain available through read tools.

User-authored text is data only. Event payloads must never inject model instructions into user-authored fields.

## 5. Subscription lifecycle

`events/subscribe` must:

1. authenticate the caller using the existing MCP/Fabric identity boundary;
2. authorize the event and requested resource/filter;
3. validate the event name and arguments against the catalog schema;
4. support webhook delivery only in the first implementation;
5. require a `whsec_` signing secret whose base64 value decodes to 24-64 bytes;
6. validate and verify the callback endpoint before activation;
7. persist the subscription and expiration;
8. return a deterministic subscription ID and `refreshBefore`.

Subscription identity must be derived deterministically from:

- authenticated principal;
- callback URL;
- event name;
- canonical JSON form of arguments.

Equivalent argument objects with different key order must not create duplicate subscriptions.

Repeated subscribe requests for the same identity refresh/update the existing subscription.

Support `ttlMs` semantics:

- omitted: server default lifetime;
- finite value: grant no more than the requested duration, except for a documented minimum if needed to prevent pathological refresh;
- `null`: request a non-expiring subscription;
- return `refreshBefore: null` only if a non-expiring subscription is actually granted.

On refresh with a replacement secret, replace the stored key and support a short bounded rotation window where old and new Standard Webhooks signatures are both accepted/sent as required.

`events/unsubscribe` must be authorized and idempotent.

## 6. Durable subscription storage

Subscription state must survive Fabric/worker/process restart for the lifetime granted by the server.

Persist at least:

- subscription ID;
- authenticated principal/owner;
- event name;
- canonical filter arguments;
- callback URL;
- active signing secret;
- prior signing secret + rotation expiry when applicable;
- verification state / verification cache reference;
- expiration / refresh time;
- cursor where replay is supported;
- delivery retry metadata required for bounded retries.

Do not use process memory as the durable source of truth.

The exact backend should follow the Fabric persistence direction rather than introducing an isolated one-off store solely for MCP Events.

## 7. Callback verification and SSRF boundary

Before application events are delivered, send a signed verification request containing a fresh, single-use, short-lived challenge:

```json
{
  "type": "verification",
  "challenge": "<random single-use value>"
}
```

Requirements:

- unique `webhook-id`;
- `webhook-timestamp`;
- Standard Webhooks `webhook-signature`;
- `X-MCP-Subscription-Id`;
- require a 2xx response;
- require the response body to echo the challenge;
- compare the returned challenge in constant time.

On verification failure, return JSON-RPC error `-32015` (`CallbackEndpointError`) with a categorized reason such as `challenge_failed` or `timeout`.

Callback networking must be fail-closed:

- HTTPS only;
- resolve and validate destination addresses at connection time;
- block loopback, private, link-local, local-network, metadata-service, and otherwise non-public destinations;
- connect to the validated address while preserving the original hostname for TLS verification;
- never follow redirects;
- repeat address validation for verification and every event delivery.

Successful verification may be cached by authenticated principal + callback URL only for a bounded period.

## 8. Webhook delivery

Deliver exactly one application event per request.

Event envelope:

```json
{
  "eventId": "evt_...",
  "name": "job.state.changed",
  "timestamp": "2026-10-05T00:00:00Z",
  "data": {},
  "cursor": null
}
```

Required headers:

- `Content-Type: application/json`
- `webhook-id`: same value as `eventId`
- `webhook-timestamp`: Unix seconds for the signing time
- `webhook-signature`: Standard Webhooks HMAC signature
- `X-MCP-Subscription-Id`: active subscription ID

Rules:

- serialize the body once, sign those exact bytes, and send those same bytes;
- complete request body <= 256 KiB (262,144 bytes);
- preserve a stable unique `eventId` across retries;
- generate a fresh signing timestamp/signature for every retry;
- retry transient failures with exponential backoff and bounded attempts;
- do not retry HTTP 410 or 413;
- tolerate out-of-order deliveries;
- downstream mutating tools must remain idempotent so duplicate event-driven execution does not duplicate effects.

## 9. Filtering and authorization

Filters are enforced before delivery.

At minimum:

- a `job_id` subscription must not receive another job's transitions;
- a `session_id` subscription must not receive another session's transitions;
- subscriptions may expose only events/resources the authenticated account is allowed to discover;
- authorization must be rechecked during the subscription lifetime;
- revoked account/resource access stops future delivery.

A subscription must never become a durable authority grant that survives revoked Fabric/Host access.

## 10. Replay and cursors

The first implementation may use non-replayable Temote state-change events and return `cursor: null`.

If replay is later added:

- cursor persistence becomes part of the durable subscription state;
- refresh/resume must not skip events still awaiting delivery;
- return `truncated: true` when requested history is no longer available.

Do not invent replay semantics only to satisfy the protocol shape.

## 11. Explicit non-goals for v1

ChatGPT currently does not require these delivery forms, so leave them out of the first implementation:

- polling delivery;
- streaming delivery;
- draft MCP Events `gap` control notification;
- draft MCP Events `terminated` control notification;
- embedding full logs/artifacts/results in event payloads;
- changing the legacy MCP protocol surface.

## 12. Implementation seams

Prefer explicit components with narrow responsibilities:

1. event catalog/schema definitions;
2. durable subscription repository;
3. authorization + filter matcher;
4. callback verifier;
5. SSRF-safe outbound HTTP transport;
6. Standard Webhooks signer;
7. delivery/retry queue;
8. Temote/Fabric transition producers;
9. MCP method handlers;
10. contract/E2E test fixtures.

The event producer should consume existing authoritative state transitions or projections rather than adding polling loops solely to synthesize events.

## 13. Acceptance criteria

### Protocol

- [ ] Supported modern MCP discovery advertises `capabilities.events = {}`.
- [ ] Unsupported/non-durable paths do not advertise Events.
- [ ] `events/list` returns schema-valid initial Temote event definitions.
- [ ] `events/subscribe` and `events/unsubscribe` are available on the same authenticated endpoint.
- [ ] Legacy MCP contract remains unchanged.

### Lifecycle

- [ ] Subscribe validates auth, event name, filter schema, secret format, callback URL, and callback challenge.
- [ ] Repeating an equivalent subscription is idempotent.
- [ ] Canonical JSON prevents key-order duplicates.
- [ ] Finite subscriptions return `refreshBefore`.
- [ ] Refresh after process/worker restart updates the existing subscription rather than creating another one.
- [ ] Secret replacement supports bounded rotation.
- [ ] Unsubscribe is authorized and idempotent.

### Security

- [ ] HTTPS is required.
- [ ] Private/loopback/link-local/metadata/local callback destinations are rejected.
- [ ] Redirects are rejected.
- [ ] DNS/address validation is performed for verification and delivery connections.
- [ ] Verification challenge comparison is constant-time.
- [ ] Revoked access stops future delivery.

### Delivery

- [ ] Matching job/session transitions produce schema-valid signed events.
- [ ] Non-matching transitions are not delivered.
- [ ] Each request contains one event and is <= 256 KiB.
- [ ] Event IDs remain stable across retries.
- [ ] Retry signatures/timestamps are fresh.
- [ ] HTTP 410 and 413 are not retried.
- [ ] Duplicate/out-of-order deliveries do not create duplicate Temote mutations.

### Tests

- [ ] Unit tests cover event schemas, canonical subscription identity, secret validation, filtering, expiry, retry classification, and payload size.
- [ ] Security tests cover callback SSRF cases, redirect rejection, challenge mismatch, timeout, and secret rotation.
- [ ] Restart test proves durable subscription refresh and delivery across process/worker restart.
- [ ] Existing modern/legacy MCP tests remain green.
- [ ] Live ChatGPT Work E2E confirms: discover -> list -> subscribe -> callback verification -> matching event -> ChatGPT task reaction -> unsubscribe.
- [ ] Live E2E also verifies a non-matching event is not delivered.
- [ ] Event-triggered actions are checked for feedback loops.

## 14. Upstream compatibility note

Treat OpenAI's MCP Events page as the current ChatGPT interoperability target, but keep the implementation isolated enough that the draft protocol can evolve without coupling Temote's core state model to OpenAI-specific behavior.

When upstream semantics change, update the contract tests and compatibility notes before changing durable subscription data or event identities.

Reference checked 2026-10-05:
https://developers.openai.com/plugins/build/mcp-events
