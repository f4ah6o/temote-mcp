import test from "node:test";
import assert from "node:assert/strict";
import { evaluateCfPreflight } from "../scripts/cf-deployment-preflight.mjs";

const databaseId = "12345678-1234-1234-1234-123456789012";
const baseline = { databaseId, namespaces: { GATEWAY_SESSIONS: "session-id", GATEWAY_REGISTRY: "registry-id" } };
const inventory = (owner, renamed) => [
  { id: "session-id", script: owner, class: renamed ? "FabricSession" : "GatewaySession", use_sqlite: true },
  { id: "registry-id", script: owner, class: renamed ? "FabricRegistry" : "GatewayRegistry", use_sqlite: true },
];
test("migration preflight requires the retained IDs and confirmed predecessors", () => {
  for (const stage of ["legacy", "rename", "prepare", "transfer", "fabric"]) {
    const owner = stage === "fabric" ? "temote-fabric" : "temote-mcp-gateway";
    const current = inventory(owner, !["legacy", "rename"].includes(stage));
    const profile = { migrationStage: stage, databaseId };
    const result = evaluateCfPreflight(profile, baseline, current);
    assert.equal(result.remote_deployment, "not_run");
    assert.equal(result.namespaces[0].namespace_id, "session-id");
    for (const invalid of [[], [...current, current[0]], current.map(item => ({ ...item, id: `${item.id}-new` })),
      current.map(item => ({ ...item, script: "unrelated" })), current.map(item => ({ ...item, use_sqlite: false }))]) {
      assert.throws(() => evaluateCfPreflight(profile, baseline, invalid));
    }
    assert.throws(() => evaluateCfPreflight({ ...profile, databaseId: "different" }, baseline, current));
  }
  assert.throws(() => evaluateCfPreflight({ migrationStage: "fabric", databaseId }, baseline, inventory("temote-mcp-gateway", true)));
  assert.throws(() => evaluateCfPreflight({ migrationStage: "prepare", databaseId }, baseline, inventory("temote-mcp-gateway", false)));
  assert.throws(() => evaluateCfPreflight({ migrationStage: "legacy", databaseId }, baseline, inventory("temote-mcp-gateway", true)));
  assert.throws(() => evaluateCfPreflight({ migrationStage: "prepare", databaseId }, baseline, inventory("temote-fabric", true)));
});
