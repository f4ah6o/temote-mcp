# C6: optional Fabric R2 Tier 2 content store

Status: deferred / closed until a concrete large-content requirement is accepted
Repository: `f4ah6o/temote-mcp`
Created: 2026-10-01 (Asia/Tokyo)
Source: `issues/done/20260926-cloud-observation-knowledge-plane.md`

## Why deferred

The required Fabric observation/context/memory path (C0-C5) is implemented and qualified without R2. R2 is an optional Tier 2 object store for content that should not be placed directly in D1; it is not a prerequisite for durable observation metadata, derived knowledge, context resolution, or offline-host continuity.

Keeping optional storage work in the active queue makes the completed core plane look unfinished. Reopen or replace this note with a polished implementation packet only when a specific cloud-eligible content class needs object storage.

## Preserved requirements

If activated, the implementation must define and test:

- [ ] R2 binding and deployment contract
- [ ] explicit allow-listed cloud-eligible content contract
- [ ] bounded upload/object-size policy
- [ ] D1 content references without making R2 the metadata database
- [ ] owner/repository/session scoping of object references
- [ ] retention and deletion behavior
- [ ] encryption/access behavior consistent with existing Fabric authorization
- [ ] no raw transcript-dump regression
- [ ] failure of R2 does not fabricate observation acknowledgement or current context
- [ ] local execution remains independent from R2 availability

## Reopen trigger

Create a new polished packet when at least one concrete feature requires storing a bounded content body that cannot fit the current D1/structured-observation contract. That packet must name the content class, producer, reader, retention, authorization scope, maximum size, and fallback behavior.
