// Fabric context boundary.
//
// Context projection is intentionally separate from host/session routing and
// never makes Fabric authoritative for live execution state.

import { resolveCloudContext } from "./resolver.js";

export { resolveCloudContext };

export function contextPlaneBindings(env) {
  return {
    observationDatabase: Boolean(env?.OBSERVATION_DB),
  };
}
