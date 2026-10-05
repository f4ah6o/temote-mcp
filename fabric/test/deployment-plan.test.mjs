import test from "node:test";
import assert from "node:assert/strict";
import { deploymentPlan, namespaceExports } from "../deployment-plan.mjs";
import frontdoor from "../src/deployment/frontdoor.js";

test("migration stages preserve one namespace owner and never delete storage", () => {
  const exports = { durableObject: value => value };
  for (const stage of ["legacy", "rename", "prepare", "transfer", "fabric"]) {
    const plan = deploymentPlan({ migrationStage: stage });
    const definitions = namespaceExports(plan, exports);
    assert.ok(Object.values(definitions).every(value => value.state !== "deleted"));
    assert.equal(plan.namespaceOwner, ["transfer", "fabric"].includes(stage) ? plan.target : plan.source);
    if (stage === "rename") assert.deepEqual(definitions.GatewaySession, { state: "renamed", renamedTo: "FabricSession" });
    if (stage === "prepare") assert.deepEqual(definitions.FabricSession, { state: "expecting-transfer", storage: "sqlite", transferFrom: plan.source });
    if (stage === "transfer") assert.deepEqual(definitions.FabricSession, { state: "transferred", transferredTo: plan.target });
  }
  for (const profile of [{ sourceWorker: "same", targetWorker: "same" }, { migrationStage: "deleted" }, { migrationStage: "prepare", domain: "public.example" }, { migrationStage: "fabric", workerName: "wrong" }]) {
    assert.throws(() => deploymentPlan(profile));
  }
});

test("frontdoor forwards the original streaming request once to the retained authority", async () => {
  const request = new Request("https://fabric.example/mcp", { method: "POST", headers: { authorization: "Bearer fixture", "cf-access-jwt-assertion": "fixture-assertion" }, body: "bounded-original-request" });
  let calls = 0;
  const result = await frontdoor.fetch(request, { FABRIC_AUTHORITY: { async fetch(value) {
    calls += 1;
    assert.equal(value, request);
    return new Response(null, { status: 401 });
  } } });
  assert.equal(calls, 1);
  assert.equal(result.status, 401);
  const unavailable = await frontdoor.fetch(request, { FABRIC_AUTHORITY: { fetch() { throw new Error("secret-provider-error"); } } });
  assert.equal(unavailable.status, 503);
  assert.doesNotMatch(await unavailable.text(), /secret-provider/);
});
