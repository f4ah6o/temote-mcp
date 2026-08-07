import assert from "node:assert/strict";
import test from "node:test";

import worker, { GatewaySession } from "../src/index.js";
import {
  PUBLIC_TOOLS,
  negotiateProtocolVersion,
  sessionIdFromRpc,
  validateSessionId,
} from "../src/protocol.js";

class MemoryStorage {
  constructor() {
    this.values = new Map();
  }

  async get(key) {
    return this.values.get(key);
  }

  async put(key, value) {
    if (typeof key === "object" && value === undefined) {
      for (const [entryKey, entryValue] of Object.entries(key)) this.values.set(entryKey, entryValue);
      return;
    }
    this.values.set(key, value);
  }

  async delete(key) {
    this.values.delete(key);
  }
}

function noOpRegistry() {
  const stub = { fetch: async () => new Response(null, { status: 204 }) };
  return {
    idFromName: (name) => name,
    get: () => stub,
  };
}

function post(path, body) {
  return new Request(`https://session.internal/${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
}

async function body(response) {
  return response.json();
}

test("public gateway exposes the same fifteen public tools", () => {
  assert.equal(PUBLIC_TOOLS.length, 15);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "without_sandbox"), false);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "session_list"), true);
  assert.equal(PUBLIC_TOOLS.every((tool) => tool.inputSchema.additionalProperties === false), true);
});

test("session IDs are safe Durable Object routing keys", () => {
  for (const value of ["mac", "windows-wsl2", "project_1.dev"]) assert.equal(validateSessionId(value), true);
  for (const value of ["", ".", "..", "../escape", "contains spaces", "x".repeat(65)]) {
    assert.equal(validateSessionId(value), false, value);
  }
});

test("tools/call routing reads only params.arguments.session_id", () => {
  assert.equal(
    sessionIdFromRpc({ params: { arguments: { session_id: "mac" } } }),
    "mac",
  );
  assert.equal(sessionIdFromRpc({ session_id: "wrong-place" }), null);
});

test("protocol negotiation accepts known versions and falls back", () => {
  assert.equal(
    negotiateProtocolVersion({ params: { protocolVersion: "2025-03-26" } }),
    "2025-03-26",
  );
  assert.equal(negotiateProtocolVersion({ params: { protocolVersion: "future" } }), "2025-06-18");
});

test("reconnect increments generation and rejects the old host", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  const first = await body(await session.fetch(post("connect", {
    session_id: "mac",
    instance_id: "instance-a",
    platform: "macos",
  })));
  const second = await body(await session.fetch(post("connect", {
    session_id: "mac",
    instance_id: "instance-b",
    platform: "macos",
  })));

  assert.equal(first.generation, 1);
  assert.equal(second.generation, 2);
  const stale = await session.fetch(post("poll", {
    session_id: "mac",
    instance_id: "instance-a",
    generation: 1,
  }));
  assert.equal(stale.status, 409);
  assert.equal((await stale.json()).error, "stale_generation");
});

test("dispatch, poll, and respond complete one routed RPC", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  const connected = await body(await session.fetch(post("connect", {
    session_id: "windows-wsl2",
    instance_id: "instance-w",
    platform: "wsl2",
  })));

  const dispatched = session.fetch(post("dispatch", {
    request: {
      jsonrpc: "2.0",
      id: 7,
      method: "tools/call",
      params: { name: "session_info", arguments: { session_id: "windows-wsl2" } },
    },
  }));
  const poll = await session.fetch(post("poll", {
    session_id: "windows-wsl2",
    instance_id: "instance-w",
    generation: connected.generation,
  }));
  const envelope = await poll.json();
  assert.equal(envelope.request.id, 7);

  const uploaded = await session.fetch(post("respond", {
    session_id: "windows-wsl2",
    instance_id: "instance-w",
    generation: connected.generation,
    request_id: envelope.request_id,
    response: { jsonrpc: "2.0", id: 7, result: { content: [] } },
  }));
  assert.equal(uploaded.status, 204);
  assert.deepEqual(await (await dispatched).json(), {
    jsonrpc: "2.0",
    id: 7,
    result: { content: [] },
  });
});


test("the single MCP endpoint publishes the gateway tool list", async () => {
  const request = new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
  });
  const response = await worker.fetch(request, {
    CLIENT_TOKEN: "client-token",
    GATEWAY_VERSION: "test",
  });

  assert.equal(response.status, 200);
  const rpc = await response.json();
  assert.equal(rpc.result.tools.length, 15);
  assert.equal(rpc.result.tools.some((tool) => tool.name === "without_sandbox"), false);
});

test("the MCP endpoint selects a Session Durable Object only by session_id", async () => {
  const selected = [];
  const sessionStub = {
    fetch: async (_url, init) => {
      const request = JSON.parse(init.body).request;
      return new Response(JSON.stringify({
        jsonrpc: "2.0",
        id: request.id,
        result: { content: [{ type: "text", text: "routed" }] },
      }), { headers: { "content-type": "application/json" } });
    },
  };
  const request = new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 2,
      method: "tools/call",
      params: { name: "session_info", arguments: { session_id: "mac-main" } },
    }),
  });
  const response = await worker.fetch(request, {
    CLIENT_TOKEN: "client-token",
    GATEWAY_SESSIONS: {
      idFromName: (name) => {
        selected.push(name);
        return name;
      },
      get: () => sessionStub,
    },
  });

  assert.deepEqual(selected, ["mac-main"]);
  assert.equal((await response.json()).result.content[0].text, "routed");
});
