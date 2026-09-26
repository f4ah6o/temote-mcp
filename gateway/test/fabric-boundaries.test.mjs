import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

import {
  accessEmailAllowed,
  normalizeAccessTeamDomain,
  validateAccessJwtShape,
} from "../src/access.js";
import {
  agentRoute,
  agentRouteKey,
  compareSessionRoute,
  withoutHostRoutingArgument,
} from "../src/routing.js";
import {
  OBSERVATION_SCHEMA_VERSION,
  observationPlaneBindings,
} from "../src/observation/index.js";
import { contextPlaneBindings } from "../src/context/index.js";

test("Fabric access helpers are independently importable", () => {
  assert.equal(accessEmailAllowed("USER@example.com", "user@example.com"), true);
  assert.equal(normalizeAccessTeamDomain("team.cloudflareaccess.com"), "https://team.cloudflareaccess.com");
  assert.throws(() => validateAccessJwtShape("not-a-jwt"), /invalid JWT/);
});

test("Fabric routing helpers keep route normalization out of the Worker entrypoint", () => {
  assert.deepEqual(agentRoute({ host_id: "mac-main" }), { host_id: "mac-main" });
  assert.equal(agentRouteKey({ session_id: "temote" }), "session:temote");
  const routed = withoutHostRoutingArgument({
    params: { arguments: { host_id: "mac-main", session_id: "temote" } },
  });
  assert.deepEqual(routed.params.arguments, { session_id: "temote" });
  assert.equal(compareSessionRoute({ host_id: "a", session_id: "z" }, { host_id: "b", session_id: "a" }) < 0, true);
});

test("Fabric observation and context boundaries do not depend on routing Durable Objects", () => {
  assert.equal(OBSERVATION_SCHEMA_VERSION, 1);
  assert.deepEqual(observationPlaneBindings({}), { database: false, memoryQueue: false });
  assert.deepEqual(
    observationPlaneBindings({ OBSERVATION_DB: {}, MEMORY_QUEUE: {} }),
    { database: true, memoryQueue: true },
  );
  assert.deepEqual(contextPlaneBindings({ OBSERVATION_DB: {} }), { observationDatabase: true });

  for (const relative of ["../src/observation/index.js", "../src/context/index.js"]) {
    const source = fs.readFileSync(new URL(relative, import.meta.url), "utf8");
    assert.doesNotMatch(source, /GatewaySession|GatewayRegistry|GATEWAY_SESSIONS|GATEWAY_REGISTRY/);
  }
});


test("routing runtime owns host/session dispatch and Durable Objects", () => {
  assert.equal(typeof routingRuntime.handleHostApi, "function");
  assert.equal(typeof routingRuntime.listGatewaySessions, "function");
  assert.equal(typeof routingRuntime.GatewaySession, "function");
  assert.equal(typeof routingRuntime.GatewayRegistry, "function");
});
