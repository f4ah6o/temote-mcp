# Events E2: constrained Host sender and signed delivery

Status: done
Model: unknown
Created: 2026-10-05
Updated: 2026-10-06
Branch: codex/20261005-complete-issues-fabric

## 概要

Events E2: constrained Host sender and signed delivery. Parent: `issues/open/20261005-fabric-mcp-events.md`.

## 背景

Dependency: E1 durable subscriptions/outbox and authenticated Fabric Link. The parent retains full design and historical evidence. This packet tracks acceptance for the integrated implementation; executed checks and remaining live gates appear below.

## 問題

Worker egress cannot by itself prove validated-IP connection pinning, and offline Hosts must not lose queued events.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Fabric sends egress requests via Access-protected Tunnel to a dedicated Host HTTPS sender. Sender validates public DNS answer and pins IP for each connection with original-host TLS/SNI; HTTPS only, no redirects. Verify signed challenge, then send one <=256KiB event per request with stable eventId, fresh retry signature, filtered auth, and bounded backoff; hold outbox while Host offline until expiry.

## 受け入れ条件

- [x] Private/loopback/link-local/metadata targets and redirects fail.
- [x] DNS rebinding cannot switch pinned IP.
- [x] Challenge mismatch fails -32015.
- [x] 410/413 stop retries.
- [x] 2xx acknowledges one event.
- [x] Offline/reconnect preserves unexpired queued delivery without duplicate mutation.

## テスト計画

Mock DNS/connect and TLS-name tests, verification/rotation/retry fault injection, offline-reconnect E2E, gateway/Rust gates; live ChatGPT/Access gate NOT RUN until available.

## リスク

Host sender has transport-only authority and must never accept arbitrary destinations from an unauthenticated caller.

## 変更履歴

User-facing behavior, operational constraints and migration notes are recorded in [CHANGES.md](../../CHANGES.md) and the linked integration evaluation.

## 検証記録

- 2026-10-06: target/IP/TLS pinning, challenge rejection, response classifications, queue expiry/ack and reconnect fixtures; production sender/callback remains NOT RUN in parent PASS as repository fixtures/static contract review; host-specific gates remain explicitly separate.
- Scope: src/events_sender.rs; fabric/test
- Evidence: [integration evaluation](../../docs/evaluations/integration-20261006.md), [review](../../docs/evaluations/integration-review-20261006.md). Final Rust/Fabric gates passed; no external NOT RUN is promoted to PASS.

## 注記

- 2026-10-05: Split from `issues/open/20261005-fabric-mcp-events.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
- 2026-10-06: Implemented and repository acceptance verified; see dated validation evidence. External parent gates remain separate.
- 2026-10-06: Acceptance verified by the referenced repository fixtures and contract review; remaining live operational gates stay open in parent issues.
