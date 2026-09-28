import assert from "node:assert/strict";
import test from "node:test";
import worker from "../src/index.js";

function request(arguments_, authorized = true, name = "context_status") {
  return new Request("https://fabric.example/mcp", {
    method: "POST",
    headers: { "content-type": "application/json", ...(authorized ? { authorization: "Bearer test-client" } : {}) },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: arguments_ } }),
  });
}

function fixture() {
  let reads = 0;
  const db = {
    prepare() {
      reads += 1;
      return { bind() { return this; }, async all() { return { results: [] }; }, async first() { return null; } };
    },
    async batch() { return []; },
  };
  const forbiddenRouting = { idFromName() { assert.fail("cloud response must not require a routing DO"); } };
  return {
    env: { CLIENT_TOKEN: "test-client", OBSERVATION_OWNER_ID: "test-owner", OBSERVATION_DB: db,
      GATEWAY_REGISTRY: forbiddenRouting, GATEWAY_SESSIONS: forbiddenRouting },
    reads: () => reads,
  };
}

test("cloud repository context authenticates before reading replicas", async () => {
  const { env, reads } = fixture();
  const response = await worker.fetch(request({ repository: "github:test/repository" }, false), env);
  assert.equal(response.status, 401);
  assert.equal(reads(), 0);
});

test("cloud repository context bypasses host discovery with no online sessions", async () => {
  const { env, reads } = fixture();
  const response = await worker.fetch(request({ repository: "github:test/repository" }), env);
  const rpc = await response.json();
  assert.equal(rpc.error, undefined);
  const context = JSON.parse(rpc.result.content[0].text);
  assert.equal(context.scope.repository, "github:test/repository");
  assert.equal(context.authority.observations, "replicated_observed");
  assert.ok(reads() > 0);
});

test("invalid cloud scope fails closed before host routing", async () => {
  const { env, reads } = fixture();
  const rpc = await (await worker.fetch(request({ repository: "github:test/repository", owner: "another-owner" }), env)).json();
  assert.equal(rpc.error.code, -32602);
  assert.equal(reads(), 0);
});

test("unrelated evidence tools still require a session", async () => {
  const { env, reads } = fixture();
  const rpc = await (await worker.fetch(request({ repository: "github:test/repository" }, true, "evidence_read"), env)).json();
  assert.equal(rpc.error.code, -32602);
  assert.match(rpc.error.message, /session_id/);
  assert.equal(reads(), 0);
});
