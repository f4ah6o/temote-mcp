import assert from "node:assert/strict";
import test from "node:test";
import worker, { GatewaySession } from "../src/index.js";

class MemoryStorage {
  values = new Map();
  async get(key) { return structuredClone(this.values.get(key)); }
  async put(key, value) {
    const entries = typeof key === "object" ? Object.entries(key) : [[key, value]];
    for (const [name, entry] of entries) this.values.set(name, structuredClone(entry));
  }
  async delete(key) { this.values.delete(key); }
}

const post = (action, body) => new Request(`https://internal/${action}`, {
  method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
});
const registry = (fetch = async () => new Response(null, { status: 204 })) => ({
  idFromName: (name) => name, get: () => ({ fetch }),
});
const identity = { session_id: "reliability", instance_id: "agent", generation: 1 };
const connect = (instance_id = "agent") => post("connect", {
  session_id: identity.session_id, instance_id, platform: "linux",
});
const dispatch = (id) => post("dispatch", {
  request: { jsonrpc: "2.0", id, method: "tools/call", params: {
    name: "session_info", arguments: { session_id: identity.session_id },
  } },
});
const response = (request_id, id, route = identity) => post("respond", {
  ...route, request_id, response: { jsonrpc: "2.0", id, result: { content: [] } },
});

async function until(predicate) {
  for (let count = 0; count < 100; count += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.fail("fixture did not reach the expected state");
}

test("qualified host discovery never probes another host", async () => {
  const checked = [];
  const hosts = ["stalled", "selected"].map((host_id) => ({ host_id, generation: 1, expires_at: Date.now() + 60_000 }));
  const env = {
    CLIENT_TOKEN: "fixture",
    GATEWAY_REGISTRY: registry(async () => new Response(JSON.stringify(hosts))),
    GATEWAY_SESSIONS: {
      idFromName: (name) => name,
      get: (name) => ({ fetch: async (url, init) => {
        checked.push([name, new URL(url).pathname]);
        if (name === "host:stalled") throw new Error("unrelated host is unavailable");
        if (new URL(url).pathname === "/status") return new Response(null, { status: 204 });
        const rpc = JSON.parse(init.body).request;
        return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result: {
          content: [{ type: "text", text: JSON.stringify([{ session_id: "work" }]) }],
        } }));
      } }),
    },
  };
  for (const name of ["host_info", "session_list"]) {
    const result = await worker.fetch(new Request("https://gateway/mcp", {
      method: "POST", headers: { authorization: "Bearer fixture", "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: name, method: "tools/call", params: { name, arguments: { host_id: "selected" } } }),
    }), env);
    assert.ok((await result.json()).result, name);
  }
  assert.deepEqual(checked, [["host:selected", "/status"], ["host:selected", "/status"], ["host:selected", "/dispatch"]]);
});

test("capacity heartbeat renews the lease without accepting queued work", async () => {
  const storage = new MemoryStorage();
  const session = new GatewaySession({ storage }, { GATEWAY_REGISTRY: registry() });
  const connected = await (await session.fetch(connect())).json();
  assert.equal(connected.concurrent_requests, 8);
  const pending = session.fetch(dispatch(1));
  await until(() => session.pending.size === 1);
  const before = await storage.get("host");
  const heartbeat = await session.fetch(post("poll", { ...identity, accept_requests: false }));
  assert.equal(heartbeat.status, 204);
  assert.equal(session.pending.size, 1);
  assert.equal(session.queue.length, 1);
  assert.ok((await storage.get("host")).expires_at >= before.expires_at);
  const invalid = await session.fetch(post("poll", { ...identity, accept_requests: "false" }));
  assert.equal(invalid.status, 400);
  assert.equal((await invalid.json()).error, "invalid_accept_requests");
  assert.equal(session.queue.length, 1);
  const envelope = await (await session.fetch(post("poll", identity))).json();
  await session.fetch(response(envelope.request_id, 1));
  assert.equal((await pending).status, 200);
});

test("a late response leaves the generation and unrelated requests usable", async () => {
  const session = new GatewaySession({ storage: new MemoryStorage() }, { GATEWAY_REGISTRY: registry() }, { rpcTimeoutMs: 10 });
  await session.fetch(connect());
  const first = session.fetch(dispatch(1));
  await until(() => session.pending.size === 1);
  const firstEnvelope = await (await session.fetch(post("poll", identity))).json();
  assert.equal((await first).status, 504);
  session.rpcTimeoutMs = 1000;
  const second = session.fetch(dispatch(2));
  await until(() => session.pending.size === 1);
  const late = await session.fetch(response(firstEnvelope.request_id, 1));
  assert.equal(late.status, 409);
  assert.equal((await late.json()).error, "stale_request");
  const status = await (await session.fetch(new Request("https://internal/status"))).json();
  assert.equal(status.generation, 1);
  assert.equal(status.lease, "active");
  const secondEnvelope = await (await session.fetch(post("poll", identity))).json();
  assert.equal(secondEnvelope.request.id, 2);
  await session.fetch(response(secondEnvelope.request_id, 2));
  assert.equal((await second).status, 200);
});

for (const registryStatus of [204, 503]) {
  test(`old lease renewal cannot affect a replacement after registry returns ${registryStatus}`, async () => {
    let calls = 0;
    let finishRenewal;
    const renewal = new Promise((resolve) => { finishRenewal = resolve; });
    const storage = new MemoryStorage();
    const session = new GatewaySession({ storage }, { GATEWAY_REGISTRY: registry(async (url) => {
      if (new URL(url).pathname === "/upsert" && ++calls === 2) return renewal;
      return new Response(null, { status: 204 });
    }) });
    await session.fetch(connect());
    const oldPoll = session.fetch(post("poll", identity));
    await until(() => calls === 2);
    assert.equal((await session.fetch(connect("replacement"))).status, 200);
    const pending = session.fetch(dispatch(3));
    await until(() => session.pending.size === 1);
    finishRenewal(new Response(registryStatus === 503 ? JSON.stringify({ error: "registry_unavailable" }) : null, { status: registryStatus }));
    const old = await oldPoll;
    assert.equal(old.status, registryStatus === 204 ? 409 : 503);
    assert.equal((await storage.get("host")).generation, 2);
    assert.equal(session.pending.size, 1);
    assert.equal(session.waitingPoll, null);
    const replacement = { ...identity, instance_id: "replacement", generation: 2 };
    const envelope = await (await session.fetch(post("poll", replacement))).json();
    await session.fetch(response(envelope.request_id, 3, replacement));
    assert.equal((await pending).status, 200);
  });
}
