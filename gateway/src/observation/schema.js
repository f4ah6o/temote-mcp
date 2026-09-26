// Temote Fabric cloud observation / knowledge contract (O3C C0).
// This file defines wire/storage-facing semantics only. It does not make Fabric
// authoritative for Task, Execution, workspace, approval, or backend state.

export const OBSERVATION_SCHEMA_VERSION = 1;

export const FABRIC_AUTHORITY = Object.freeze({
  hostLive: "authoritative/live",
  replicatedObservation: "replicated_observed",
  derivedKnowledge: "derived",
});

export const OWNER_REPOSITORY_IDENTITY_CONTRACT = Object.freeze({
  ownerField: "owner_id",
  repositoryField: "repository_key",
  preferredRepositoryKey: "forge:<owner>/<repository>",
  localPathIsIdentity: false,
  unresolvedRepositoryScope: "session/task only; never promote repository-wide current knowledge",
});

export const OBSERVATION_SYNC_CONTRACT = Object.freeze({
  schemaVersion: 1,
  method: "POST",
  pathTemplate: "/v1/hosts/<host_id>/observations/sync",
  requestRequired: Object.freeze([
    "schema_version",
    "session_id",
    "source_base_revision",
    "source_head_revision",
    "journal_degraded",
    "gap_count",
    "records",
  ]),
  recordRequired: Object.freeze(["source_revision", "observation"]),
  responseRequired: Object.freeze([
    "session_id",
    "acked_through_revision",
    "cloud_head_seq",
    "complete",
  ]),
  ackRule: "advance only through a contiguous D1-committed source revision range",
  retryRule: "same records are idempotent by source revision and observation id",
  queueRule: "queue failure never rolls back committed D1 observation ingest",
});

export const FABRIC_FRESHNESS_CONTRACT = Object.freeze({
  required: Object.freeze([
    "latest_cloud_seq",
    "worker_last_cloud_seq",
    "worker_lag",
    "worker_last_success_at",
    "source_head_revision",
    "source_acked_revision",
    "source_gap_count",
    "cloud_observation_stale",
    "knowledge_stale",
    "partial",
  ]),
  gapRule: "known source gaps force partial/degraded provenance",
  minimumRevisionRule: "missing requested minimum revision is stale/partial, never silently current",
});

export function canonicalRepositoryKey({ forge, owner, repository, fingerprint } = {}) {
  if (forge && owner && repository) {
    return `${String(forge).toLowerCase()}:${owner}/${repository}`;
  }
  if (fingerprint) {
    return `repo:${fingerprint}`;
  }
  return null;
}
