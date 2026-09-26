// Fabric context boundary.
//
// Context projection is intentionally separate from host/session routing.
// FBR1 exposes only binding readiness; deterministic/cloud resolution is added
// by the O3C resolver packets without moving execution authority into Fabric.

export function contextPlaneBindings(env) {
  return {
    observationDatabase: Boolean(env?.OBSERVATION_DB),
  };
}
