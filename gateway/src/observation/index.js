// Fabric observation boundary.
//
// FBR1 establishes the module boundary. C0 adds the cloud schema/contracts only;
// durable D1 ingestion, bindings, replication cursors, and Queue delivery remain
// C1+ work. Routing Durable Objects must not own that state.

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
