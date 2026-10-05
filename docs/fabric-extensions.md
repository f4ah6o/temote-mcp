# Optional Fabric MCP extensions

Temote Fabric keeps its standard routed tool catalog and core fingerprint
unchanged. The optional extension contract is version 2, with a separate
`extensionContractFingerprint()`. Capability selection happens on each modern
request; there is no caller cache. A standard-only `tools/list` returns exactly
the core catalog.

## Composer mentions

A modern request that advertises the installed MCP Apps extension
`io.modelcontextprotocol/ui` can list and call `search_mentions`. The tool uses
`_meta["openai/extensions"]["mentions/search"]` and app visibility. The
OpenAI extension spec does not define a mention capability; the UI extension
is only a selector for exposing this optional app-visible tool. It does not
guarantee that a client will render composer mentions.

Search uses authenticated, bounded live Host and session projections.
Results contain only validated logical identities and safe repository labels.
Unavailable membership or liveness fails closed. The standard-only catalog
does not contain the mention tool.

## Host interaction forms

Modern requests declaring `openai/elicitation.form` can list and call
`fabric_interaction_read`. Its arguments name an explicit Host, session,
OpenCode task, and interaction. No other backend is currently supported by
this adapter. The Gateway reads a fresh `opencode_task_get` result from that
Host, parses a bounded task view, and rejects missing, deferred, unknown,
replaced, or stale interactions. It never answers from dashboard summaries.

With signing material available, a supported permission or single-choice
question returns the MCP 2026-07-28 multi round-trip
`resultType: "input_required"`, an `inputRequests.interaction`
`openai/elicitation/create` form, and an opaque `requestState`. The form
uses generic text and numbered question options. It does not copy Host
request text or choice labels. Clients need to inspect the ordinary
`opencode_task_get` view to understand the request before answering.
Permission choices are limited to `reject` and `once`; persistent approval
is not offered.

On retry, `requestState` and `inputResponses` go in the original
`tools/call` params. The state carries only the interaction ID, expected
revision, expiration, operation UUID, and hashes binding the principal,
method, tool name, and arguments. It is HMAC protected with a domain-separated
key derived from an explicitly injected `FABRIC_INTERACTION_SECRET` or that
Host's existing `HOST_TOKENS_JSON` credential. The Gateway never logs or
returns either secret. Rotation invalidates old state. State expires after
two minutes. The retry reads the Host again and resolves a numbered answer
against the fresh validated choice set before calling
`opencode_task_control(action="answer")` with the state's operation UUID.
The Host retains authority for interaction identity, owner, atomic answer,
receipt idempotence, and ordinary local approval policy. A declined or
cancelled form makes no control call.

If the key is unavailable, or a fresh interaction cannot be represented
without copying sensitive details, the tool returns a scoped standard
fallback reference to `opencode_task_get` and `opencode_task_control`.
Legacy and standard-only callers continue to use those existing Host tools.
The optional interaction tool is absent from their catalog.

The Gateway fixture tests exercise the real public HTTP dispatch, Host proxy,
MRTR response, retry, and control routes. Live ChatGPT/Codex client behavior
and deployment remain NOT RUN.
