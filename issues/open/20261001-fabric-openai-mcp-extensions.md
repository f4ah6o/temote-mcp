# Fabric: standard MCP core + OpenAI MCP Extensions adapter

Status: open
Model: unknown
Created: 2026-10-01
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Extend the existing Fabric MCP protocol adapter with optional OpenAI extension metadata and bounded user surfaces.

## 背景

The detailed design, decisions, and historical evidence remain in 「既存設計・履歴」 below. This 2026-10-05 normalization records the current work boundary without claiming implementation or test completion.

## 問題

The read-only app exists, while optional extension metadata, mentions, and interaction UI remain unimplemented.

## 目標

Extend the existing Fabric MCP protocol adapter with optional OpenAI extension metadata and bounded user surfaces.

## 対象外

Do not expand this packet into unrelated backend execution, broad host access, or changes to the repository safety invariants. Existing completed slices and their evidence remain historical facts.

### Preserved scope boundary: 13. Non-goals

This issue does not by itself authorize:

- replacing Fabric with an OpenAI-only service
- removing standard MCP support
- removing the existing web dashboard
- deleting legacy protocol support without its normal migration decision
- changing Task/Execution authority
- moving approval authority into ChatGPT
- replacing the Devin ACP execution backend
- renaming `gateway/` to `fabric/` ahead of the existing Fabric migration plan
- destructive Durable Object/D1 migration

## 提案する方針

Follow the preserved detailed contract and split remaining independent phases into the linked child packets where listed. Keep accepted side effects idempotent, scoped, and reconcilable. Use the current source and docs as the implementation baseline.

Add optional OpenAI extension capability and metadata handling to the current Fabric protocol adapter (`gateway/src/protocol.js` and its contract tests). PR #90 already merged the initial read-only MCP App. Retain standard MCP and legacy clients, authenticate all resource and interaction paths, and version contract fingerprints deliberately. SDK replacement is not part of this workstream; any future replacement would require separate parity evidence and a new decision.

## 受け入れ条件

Complete source criteria from “12. Acceptance criteria” (unchecked items remain unverified):

This issue is complete when:

- [ ] Fabric exposes the same provider-neutral standard MCP functionality as before
- [ ] OpenAI-specific capabilities are optional and capability-negotiated
- [ ] ChatGPT/Codex can use at least one native OpenAI extension surface backed by Fabric
- [ ] a non-OpenAI MCP client can use Fabric without understanding OpenAI extensions
- [ ] Devin caller interoperability is documented and live-tested where the current Devin client supports the required MCP transport/auth flow
- [ ] existing Devin ACP backend behavior is preserved
- [ ] no OpenAI extension becomes execution authority
- [ ] current auth/routing/fail-closed guarantees remain intact
- [ ] dashboard/resource surfaces preserve existing bounded/sanitized disclosure policy
- [ ] protocol/contract parity evidence is recorded before deleting existing custom protocol code
- [ ] CI is green

## テスト計画

- Run focused unit and integration tests for the behaviors and boundaries specified in the preserved design.
- Run `cargo fmt --all -- --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo check --no-default-features --all-targets`, and `git diff --check`; run `(cd fabric && npm test)` for shared protocol or Fabric changes. Record host-only and external gates as NOT RUN until actually executed.

### Source test details: 11. Tests

Add or extend tests covering:

### Contract

- [ ] generated routed tool contract unchanged unless intentionally updated
- [ ] fingerprint deterministic
- [ ] extension metadata deterministic where included
- [ ] unsupported capability path

### Protocol

- [ ] modern `server/discover`
- [ ] legacy `initialize`
- [ ] `tools/list`
- [ ] `tools/call`
- [ ] OpenAI-capable client
- [ ] standard-only client
- [ ] unknown extension fields
- [ ] malformed extension requests fail clearly

### Routing/security

- [ ] Host selection remains fail-closed
- [ ] duplicate session IDs remain ambiguous without explicit Host
- [ ] caller cannot select another owner
- [ ] named-root path stays logical/root-relative
- [ ] OpenAI UI metadata cannot bypass Host authorization/approval
- [ ] extension resources do not disclose secrets or absolute Host paths

### Client compatibility

- [ ] ChatGPT/Codex extension-capable path
- [ ] standard MCP client path
- [ ] Devin MCP caller path where supported by the current Devin client
- [ ] existing Devin ACP execution backend remains unaffected

## リスク

- Preserve authenticated routing, owner isolation, bounded disclosure, and durable-state migration; reject unsafe egress or ambiguous ownership.

## 変更履歴

Assess user-visible, operational, compatibility, and migration effects during implementation and add a `CHANGES.md` entry when applicable; this issue-only preparation does not edit the changelog.

## 注記

- 2026-10-05: Normalized the issue. This is a preparation record; unchecked criteria and external gates remain incomplete.

## 2026-10-05 実行パケット

- [`fabric-extension-metadata-interactions`](../done/20261005-fabric-extension-metadata-interactions.md)

These are planned packets, not completed implementation. The parent remains open until applicable children and acceptance evidence are complete.

## 既存設計・履歴

> Historical Status: active design umbrella — initial read-only adapter landed via PR #90; later phases remain open
Repository: `f4ah6o/temote-mcp`
Related:
- `issues/open/20260926-temote-fabric-product-boundary.md`
- `issues/open/20260924-temote-development-harness-restructure.md`
- `issues/open/20260925-observation-context-memory-plane.md`
- `issues/done/20260926-cloud-observation-knowledge-plane.md`
> Historical Created: 2026-10-01 (Asia/Tokyo)

## 0. Current implementation boundary (2026-10-02)

PR #90 is already merged. It adds an optional read-only Fabric MCP App while retaining the custom standard protocol adapter; see `docs/fabric-dashboard.md`. This PR now delivers the design umbrella, not a second runtime implementation branch. Do not interpret the phase checklist as a report that no implementation exists. Mentions, settings, and interaction UI remain future work.

Platform support must be tracked per feature and client version. The upstream Web table refers to ChatGPT Work and excludes classic ChatGPT; composer mentions and form elicitation have platform limitations. Repository fixtures and Chromium harnesses do not prove live ChatGPT, Devin, or OpenCode compatibility.

For every extension, record the exact advertisement/negotiation seam and standard fallback. Discovery-only calls must not persist a caller capability globally or leak it across authenticated callers. Metadata changes must either preserve the existing fingerprint inputs with a separately versioned extension contract, or deliberately version and regenerate the public contract. UI resource/tools/call authorization uses the same owner/scope boundary; entrypoint visibility is never approval authority.

## 1. Goal

Adopt [openai/mcp-extensions](https://github.com/openai/mcp-extensions) at the Temote Fabric MCP boundary without making Fabric OpenAI-specific.

The desired product model is:

```text
caller / head
  |
  +-- ChatGPT / Codex
  |      |
  |      +-- standard MCP
  |      +-- OpenAI MCP Extensions when advertised/supported
  |
  +-- Devin / OpenCode / other MCP clients
         |
         +-- standard MCP
                |
                v
         +------------------+
         |  Temote Fabric   |
         | provider-neutral |
         | core             |
         +--------+---------+
                  |
             Fabric Link
                  |
                  v
             Temote Host
                  |
          execution backends
       Codex / OpenCode / Devin
```

Fabric remains the shared connectivity, routing, observation, context, and memory plane. OpenAI MCP Extensions are an optional presentation/capability layer on top of the standard MCP contract.

## 2. Why

Fabric currently implements its MCP protocol surface directly under `gateway/`, including modern `server/discover`, legacy `initialize`, tool metadata, protocol-version negotiation, and result adaptation.

At the same time, OpenAI now publishes MCP extensions and TypeScript/Python SDK support for capabilities such as:

- MCP App entrypoints
- composer mentions
- richer form elicitation
- settings
- file/resource integrations
- OpenAI-specific UI/resource metadata

Reference:

- https://github.com/openai/mcp-extensions
- https://github.com/openai/mcp-extensions/blob/main/docs/spec.md

These capabilities can make Fabric feel native in ChatGPT/Codex, especially for host/session/repository discovery, dashboard access, and interaction handling.

They must not become required Fabric semantics because other MCP clients should continue to use the same Fabric endpoint through standard MCP.

## 3. Design rule

> Fabric Core is provider-neutral. OpenAI MCP Extensions adapt Fabric capabilities to OpenAI hosts; they do not define Fabric authority or execution semantics.

In particular:

- standard MCP remains the canonical interoperable remote surface
- OpenAI-specific extensions are capability-negotiated and optional
- unsupported OpenAI extensions must not prevent normal MCP tools from working
- Fabric routing/authentication/authority rules remain unchanged
- Host remains authoritative for live execution, workspace, approval, and backend state
- Fabric remains non-authoritative for live execution state
- extension support must not create a second independent task/session model

## 4. Target architecture

```text
fabric/
  core/
    access/
    registry/
    routing/
    link/
    observation/
    context/
    memory/

  mcp/
    standard/
      discovery
      tools
      resources
      compatibility

    openai/
      extensions
      app entrypoints
      mentions
      elicitation
      settings
      resource metadata
```

The current implementation may remain physically under `gateway/` while the Fabric migration is still in progress. This issue does not require an immediate directory rename.

## 5. Standard MCP contract

The existing public Fabric tool contract remains available to clients that do not implement OpenAI extensions.

Examples include:

```text
host_list
host_info
session_list
session_start
session_info
context_resolve
context_status
...
```

The exact existing generated routed-tool contract remains authoritative unless deliberately versioned.

The following existing safeguards must remain intact:

- generated routed tool metadata
- public contract fingerprint
- modern MCP protocol compatibility
- legacy MCP compatibility while still supported
- fail-closed host/session routing
- Cloudflare Access / caller authentication
- host credentials and Fabric Link authentication
- no absolute named-root path disclosure
- no automatic replay of ambiguous mutating operations

## 6. OpenAI extension layer

### 6.1 MCP App / sidebar entrypoint

Provide a Fabric MCP App entrypoint suitable for ChatGPT/Codex.

Initial scope:

- Fabric overview
- configured/discoverable Hosts
- sessions
- recent task state
- repository context freshness
- unresolved interaction summaries

The existing `/dash/` web dashboard remains valid. The MCP App may reuse the same bounded Fabric APIs/data model rather than introducing another backend.

Conceptually:

```text
/dash/             -> browser dashboard
MCP App entrypoint -> ChatGPT/Codex-native dashboard

both
  -> same Fabric core projections
```

### 6.2 Composer mentions

Investigate mention/search support for selecting Fabric resources from the composer.

Useful resource classes:

- repository
- Host
- session
- task, if stable and appropriately bounded

Mention results must expose stable logical identities rather than host filesystem paths.

### 6.3 Elicitation / interaction UI

OpenAI form elicitation may be used to render supported interaction requests in ChatGPT/Codex.

However, the underlying interaction state must remain provider-neutral.

Do not model an approval as "an OpenAI form". Instead:

```text
Fabric/Host interaction state
  |
  +-- OpenAI host -> OpenAI elicitation UI
  |
  +-- standard MCP client -> standard tool/control path
```

This preserves compatibility with non-OpenAI callers.

### 6.4 Settings

Evaluate OpenAI settings extensions only for preferences that are genuinely caller/UI configuration.

Do not move authoritative Host policy, sandbox policy, approval policy, credentials, or execution settings into a ChatGPT-specific settings store.

### 6.5 Resources and UI metadata

Use OpenAI-specific resource/UI metadata only as an enhancement.

A client that ignores these fields must still be able to use the ordinary MCP tools successfully.

## 7. Official SDK adoption

`@openai/mcp-extensions` is designed to extend MCP servers built with the official `@modelcontextprotocol/sdk`.

The current Fabric Worker implements substantial MCP protocol handling directly.

Therefore, do **not** replace `gateway/src/protocol.js` blindly.

First create a compatibility spike that determines whether the official MCP SDK can preserve the current Fabric contract and Cloudflare Worker deployment model.

The spike must compare at least:

- `server/discover`
- legacy `initialize`
- supported protocol versions
- `tools/list`
- `tools/call`
- existing annotations/input schemas
- modern response metadata
- public contract fingerprint inputs
- error behavior
- Streamable HTTP behavior
- stateless/session behavior
- Cloudflare Worker runtime compatibility
- existing authentication boundary

Possible outcomes:

1. official MCP SDK can own the complete protocol adapter
2. official SDK can own only selected standard paths
3. current protocol adapter remains, while extension metadata/methods are integrated separately

Choose based on measured parity, not package preference.

## 8. Non-OpenAI clients

A core acceptance requirement is that OpenAI extension support does not reduce interoperability.

At minimum, verify a non-OpenAI MCP client can still:

1. connect/authenticate to Fabric
2. list tools
3. discover Hosts/sessions
4. call a read-only routed tool
5. call a representative delegated task path where the test environment permits
6. resolve repository/session context

Devin should be treated as a separate caller role from the existing Devin execution backend.

These are distinct paths:

```text
Devin as MCP caller
  -> Fabric
  -> Temote Host

Temote Host
  -> Devin ACP backend
```

Supporting one does not imply the other, and the existing Devin ACP backend must not be removed as part of this work.

Where Devin's current MCP client supports the Fabric standard contract, it should require no OpenAI-specific extension behavior.

## 9. Capability negotiation

OpenAI-specific behavior must be conditional on negotiated client/host capabilities.

Required behavior:

- supported OpenAI capability -> extension feature may be exposed
- unsupported OpenAI capability -> ordinary MCP continues
- unknown extension fields -> must not break standard clients
- extension failure -> must not silently change task/execution authority
- extension UI unavailable -> preserve a standard tool-based path where the operation itself is supported

Do not require every MCP caller to understand `openai/*` extension keys.

## 10. Implementation phases

### Phase A — compatibility spike

- [ ] add official MCP SDK and OpenAI MCP Extensions in an isolated experiment or adapter
- [ ] record current protocol behavior as fixtures
- [ ] compare generated/public tool contract before and after
- [ ] verify Cloudflare Worker compatibility
- [ ] document which existing custom protocol code can safely be retired, if any
- [ ] make no user-visible contract change yet

### Phase B — extension capability advertisement

- [ ] add OpenAI extension capability advertisement without changing core tools
- [ ] verify unsupported/non-OpenAI clients continue to work
- [ ] add protocol tests for capability-present and capability-absent callers
- [ ] preserve current contract fingerprint semantics, or deliberately version them if extension metadata must be covered

### Phase C — Fabric MCP App dashboard

- [ ] expose a global/sidebar MCP App entrypoint
- [ ] reuse bounded Fabric dashboard/context projections
- [ ] show authority/freshness distinctions
- [ ] do not expose credentials, absolute paths, raw sensitive evidence, or unrestricted logs

### Phase D — resource mentions

- [ ] repository mention/search
- [ ] Host mention/search
- [ ] session mention/search
- [ ] stable identity and authorization checks
- [ ] bounded results

### Phase E — interaction UI

- [ ] map supported provider-neutral interaction requests to OpenAI elicitation
- [ ] keep standard MCP fallback/control flow
- [ ] verify no approval bypass
- [ ] verify stale interaction handling

### Phase F — protocol adapter decision

After the above parity evidence exists:

- [ ] decide whether to keep or replace current custom MCP protocol handling
- [ ] if replacing, migrate incrementally
- [ ] keep legacy compatibility for the documented compatibility period
- [ ] update generated contract/fingerprint logic deliberately
- [ ] retain rollback path

## 11. Tests

Add or extend tests covering:

### Contract

- [ ] generated routed tool contract unchanged unless intentionally updated
- [ ] fingerprint deterministic
- [ ] extension metadata deterministic where included
- [ ] unsupported capability path

### Protocol

- [ ] modern `server/discover`
- [ ] legacy `initialize`
- [ ] `tools/list`
- [ ] `tools/call`
- [ ] OpenAI-capable client
- [ ] standard-only client
- [ ] unknown extension fields
- [ ] malformed extension requests fail clearly

### Routing/security

- [ ] Host selection remains fail-closed
- [ ] duplicate session IDs remain ambiguous without explicit Host
- [ ] caller cannot select another owner
- [ ] named-root path stays logical/root-relative
- [ ] OpenAI UI metadata cannot bypass Host authorization/approval
- [ ] extension resources do not disclose secrets or absolute Host paths

### Client compatibility

- [ ] ChatGPT/Codex extension-capable path
- [ ] standard MCP client path
- [ ] Devin MCP caller path where supported by the current Devin client
- [ ] existing Devin ACP execution backend remains unaffected

## 12. Acceptance criteria

This issue is complete when:

- [ ] Fabric exposes the same provider-neutral standard MCP functionality as before
- [ ] OpenAI-specific capabilities are optional and capability-negotiated
- [ ] ChatGPT/Codex can use at least one native OpenAI extension surface backed by Fabric
- [ ] a non-OpenAI MCP client can use Fabric without understanding OpenAI extensions
- [ ] Devin caller interoperability is documented and live-tested where the current Devin client supports the required MCP transport/auth flow
- [ ] existing Devin ACP backend behavior is preserved
- [ ] no OpenAI extension becomes execution authority
- [ ] current auth/routing/fail-closed guarantees remain intact
- [ ] dashboard/resource surfaces preserve existing bounded/sanitized disclosure policy
- [ ] protocol/contract parity evidence is recorded before deleting existing custom protocol code
- [ ] CI is green

## 13. Non-goals

This issue does not by itself authorize:

- replacing Fabric with an OpenAI-only service
- removing standard MCP support
- removing the existing web dashboard
- deleting legacy protocol support without its normal migration decision
- changing Task/Execution authority
- moving approval authority into ChatGPT
- replacing the Devin ACP execution backend
- renaming `gateway/` to `fabric/` ahead of the existing Fabric migration plan
- destructive Durable Object/D1 migration

## 14. Principle

> Standard MCP is the interoperability contract. OpenAI MCP Extensions are the native OpenAI experience layered on top.

Or in Temote terms:

> Execute on the Host. Connect through Fabric. Adapt the experience to the caller without changing the work model.
