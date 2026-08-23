import assert from "node:assert/strict";
import test from "node:test";

import worker, { GatewaySession, accessEmailAllowed, readBoundedBytes } from "../src/index.js";
import {
  MODERN_PROTOCOL_VERSION,
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

test("public gateway exposes the same twenty-five public tools", () => {
  assert.equal(PUBLIC_TOOLS.length, 25);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "without_sandbox"), false);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "session_list"), true);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "kintone_cli_status"), true);
  assert.equal(PUBLIC_TOOLS.some((tool) => tool.name === "kintone_cli_run"), true);
  assert.equal(PUBLIC_TOOLS.every((tool) => tool.inputSchema.additionalProperties === false), true);
});

test("Access email allowlist is fail-closed and case-insensitive", () => {
  for (const configured of [undefined, null, "", "   ", ",,,", " , "]) {
    assert.equal(accessEmailAllowed(configured, "user@example.com"), false);
  }
  for (const email of ["user@example.com", "USER@example.com", " user@example.com "]) {
    assert.equal(
      accessEmailAllowed("other@example.com, User@Example.com", email),
      true,
      email,
    );
  }
  for (const email of ["", "other2@example.com", null, undefined]) {
    assert.equal(accessEmailAllowed("user@example.com", email), false, String(email));
  }
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

test("dispatch capacity is bounded and excess work fails without disturbing pending calls", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  await session.fetch(post("connect", {
    session_id: "bounded",
    instance_id: "instance-cap",
    platform: "linux",
  }));

  const pending = [];
  for (let index = 0; index < 64; index += 1) {
    pending.push(session.fetch(post("dispatch", {
      request: {
        jsonrpc: "2.0",
        id: index,
        method: "tools/call",
        params: { name: "session_info", arguments: { session_id: "bounded" } },
      },
    })));
  }
  for (let spin = 0; spin < 20 && session.pending.size < 64; spin += 1) {
    await Promise.resolve();
  }
  assert.equal(session.pending.size, 64);
  assert.equal(session.queue.length, 64);

  const excess = await session.fetch(post("dispatch", {
    request: {
      jsonrpc: "2.0",
      id: "excess",
      method: "tools/call",
      params: { name: "session_info", arguments: { session_id: "bounded" } },
    },
  }));
  assert.equal(excess.status, 503);
  assert.equal((await excess.json()).error, "gateway_busy");
  assert.equal(session.pending.size, 64);
  assert.equal(session.queue.length, 64);

  await session.fetch(post("connect", {
    session_id: "bounded",
    instance_id: "instance-replacement",
    platform: "linux",
  }));
  const replaced = await Promise.all(pending);
  assert.equal(replaced.every((response) => response.status === 502), true);
  assert.equal(session.pending.size, 0);
  assert.equal(session.queue.length, 0);
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


test("bounded stream reader matches the length model across chunking", async () => {
  const limit = 64;
  for (const length of [0, 1, 62, 63, 64, 65, 66, 96]) {
    for (const chunkSize of [1, 7, 31, 64, 128]) {
      const source = new Uint8Array(length).fill(0x61);
      const stream = new ReadableStream({
        start(controller) {
          for (let offset = 0; offset < source.length; offset += chunkSize) {
            controller.enqueue(source.slice(offset, Math.min(offset + chunkSize, source.length)));
          }
          controller.close();
        },
      });
      const message = { headers: new Headers(), body: stream };
      if (length <= limit) {
        const actual = await readBoundedBytes(message, limit, "generated body");
        assert.equal(actual.byteLength, length, `length=${length} chunk=${chunkSize}`);
      } else {
        await assert.rejects(
          readBoundedBytes(message, limit, "generated body"),
          /generated body is too large/,
          `length=${length} chunk=${chunkSize}`,
        );
      }
    }
  }
});

test("chunked MCP request bodies cannot bypass the body limit", async () => {
  const oversized = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "tools/list",
    padding: "x".repeat(8 * 1024 * 1024),
  });
  const encoder = new TextEncoder();
  const bytes = encoder.encode(oversized);
  const stream = new ReadableStream({
    start(controller) {
      const chunk = 64 * 1024;
      for (let offset = 0; offset < bytes.length; offset += chunk) {
        controller.enqueue(bytes.slice(offset, Math.min(offset + chunk, bytes.length)));
      }
      controller.close();
    },
  });
  const request = new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
    },
    body: stream,
    duplex: "half",
  });
  assert.equal(request.headers.has("content-length"), false);
  const response = await worker.fetch(request, { CLIENT_TOKEN: "client-token" });
  assert.equal(response.status, 400);
  const rpc = await response.json();
  assert.match(rpc.error.message, /request body is too large/);
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
  assert.equal(rpc.result.tools.length, 25);
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


test("modern server/discover advertises the 2026 protocol", async () => {
  const request = new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
      "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
      "mcp-method": "server/discover",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: "discover-1",
      method: "server/discover",
      params: {
        _meta: {
          "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
          "io.modelcontextprotocol/clientCapabilities": {},
        },
      },
    }),
  });
  const response = await worker.fetch(request, {
    CLIENT_TOKEN: "client-token",
    GATEWAY_VERSION: "test",
  });

  assert.equal(response.status, 200);
  const result = (await response.json()).result;
  assert.deepEqual(result.supportedVersions, [MODERN_PROTOCOL_VERSION]);
  assert.equal(result.resultType, "complete");
  assert.equal(result.ttlMs, 0);
  assert.equal(result.cacheScope, "private");
});

test("modern tools/list requires Mcp-Method and returns cacheable result fields", async () => {
  const rpc = {
    jsonrpc: "2.0",
    id: 11,
    method: "tools/list",
    params: {
      _meta: {
        "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
      },
    },
  };
  const missingHeader = await worker.fetch(new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
      "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
    },
    body: JSON.stringify(rpc),
  }), { CLIENT_TOKEN: "client-token" });
  assert.equal(missingHeader.status, 400);
  assert.equal((await missingHeader.json()).error.code, -32020);

  const response = await worker.fetch(new Request("https://gateway.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer client-token",
      "content-type": "application/json",
      "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
      "mcp-method": "tools/list",
    },
    body: JSON.stringify(rpc),
  }), { CLIENT_TOKEN: "client-token", GATEWAY_VERSION: "test" });
  assert.equal(response.status, 200);
  const result = (await response.json()).result;
  assert.equal(result.resultType, "complete");
  assert.equal(result.ttlMs, 0);
  assert.equal(result.cacheScope, "private");
  assert.equal(Array.isArray(result.tools), true);
});

test("modern routed tool responses are normalized to the 2026 result shape", async () => {
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
      "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
      "mcp-method": "tools/call",
      "mcp-name": "session_info",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 12,
      method: "tools/call",
      params: {
        name: "session_info",
        arguments: { session_id: "mac-main" },
        _meta: {
          "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
          "io.modelcontextprotocol/clientCapabilities": {},
        },
      },
    }),
  });
  const response = await worker.fetch(request, {
    CLIENT_TOKEN: "client-token",
    GATEWAY_VERSION: "test",
    GATEWAY_SESSIONS: {
      idFromName: (name) => name,
      get: () => sessionStub,
    },
  });

  assert.equal(response.status, 200);
  const result = (await response.json()).result;
  assert.equal(result.resultType, "complete");
  assert.equal(result.content[0].text, "routed");
  assert.equal(result._meta["io.modelcontextprotocol/serverInfo"].name, "temote-mcp-gateway");
});
