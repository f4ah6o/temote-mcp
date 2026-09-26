// Fabric observation boundary.
//
// FBR1 establishes the module boundary. C1 keeps observation replication here;
// routing Durable Objects remain execution/routing infrastructure and do not own
// replicated observation state. Queue delivery remains a later phase.

export {
  FABRIC_AUTHORITY,
  FABRIC_FRESHNESS_CONTRACT,
  OBSERVATION_SCHEMA_VERSION,
  OBSERVATION_SYNC_CONTRACT,
  OWNER_REPOSITORY_IDENTITY_CONTRACT,
  canonicalRepositoryKey,
} from "./schema.js";

export function observationPlaneBindings(env) {
  return {
    database: Boolean(env?.OBSERVATION_DB),
    memoryQueue: Boolean(env?.MEMORY_QUEUE),
  };
}

export {
  handleObservationSync,
  MAX_OBSERVATION_SYNC_BODY_BYTES,
  MAX_OBSERVATION_SYNC_RECORDS,
  observationSyncHostId,
  validateObservationSyncRequest,
} from "./ingest.js";
