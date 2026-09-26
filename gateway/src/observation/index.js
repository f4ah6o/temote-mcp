// Fabric observation boundary.
//
// FBR1 only establishes the module boundary. Durable observation ingestion,
// D1 schema, replication cursors, and Queue delivery are implemented by the
// O3C C0+ packets; routing Durable Objects must not own that state.

export const OBSERVATION_SCHEMA_VERSION = 1;

export function observationPlaneBindings(env) {
  return {
    database: Boolean(env?.OBSERVATION_DB),
    memoryQueue: Boolean(env?.MEMORY_QUEUE),
  };
}
