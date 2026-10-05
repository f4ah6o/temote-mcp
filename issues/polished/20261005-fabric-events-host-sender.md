# Events E2: constrained Host sender and signed delivery

Status: polished
Model: unknown
Created: 2026-10-05
Updated: 2026-10-05
Branch: codex/20261005-complete-issues-fabric

## 概要

Events E2: constrained Host sender and signed delivery. Parent: `issues/open/20261005-fabric-mcp-events.md`.

## 背景

Dependency: E1 durable subscriptions/outbox and authenticated Fabric Link. The parent retains full design and historical evidence. This packet records unimplemented work and does not claim tests have run.

## 問題

Worker egress cannot by itself prove validated-IP connection pinning, and offline Hosts must not lose queued events.

## 目標

Deliver the bounded behavior in this packet while preserving the parent contract.

## 対象外

Other parent phases, unsupported provider integrations, and changes to Temote's security model are outside this packet.

## 提案する方針

Fabric sends egress requests via Access-protected Tunnel to a dedicated Host HTTPS sender. Sender validates public DNS answer and pins IP for each connection with original-host TLS/SNI; HTTPS only, no redirects. Verify signed challenge, then send one <=256KiB event per request with stable eventId, fresh retry signature, filtered auth, and bounded backoff; hold outbox while Host offline until expiry.

## 受け入れ条件

- [ ] Private/loopback/link-local/metadata targets and redirects fail.
- [ ] DNS rebinding cannot switch pinned IP.
- [ ] Challenge mismatch fails -32015.
- [ ] 410/413 stop retries.
- [ ] 2xx acknowledges one event.
- [ ] Offline/reconnect preserves unexpired queued delivery without duplicate mutation.

## テスト計画

Mock DNS/connect and TLS-name tests, verification/rotation/retry fault injection, offline-reconnect E2E, gateway/Rust gates; live ChatGPT/Access gate NOT RUN until available.

## リスク

Host sender has transport-only authority and must never accept arbitrary destinations from an unauthenticated caller.

## 変更履歴

Assess user-facing, operational and migration effects during implementation; add a `CHANGES.md` entry when applicable. No implementation or changelog change is claimed here.

## 注記

- 2026-10-05: Split from `issues/open/20261005-fabric-mcp-events.md` as a decision-complete implementation packet. Parent remains open until its child work is complete.
