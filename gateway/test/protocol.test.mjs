import assert from "node:assert/strict";
import test from "node:test";

import worker, { GatewayRegistry, GatewaySession, accessEmailAllowed, gatewaySessionBodyLimit, hostApiBodyLimit, nextGatewayGeneration, pruneExpiredRegistrySessions, readBoundedBytes, shouldReplaceRegistrySession } from "../src/index.js";
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

async function waitFor(predicate, message) {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.fail(message);
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

test("gateway generation increment is safe and fail-closed", () => {
  const cases = [
    [undefined, 1],
    [null, 1],
    [0, 1],
    [1, 2],
    [42, 43],
    [Number.MAX_SAFE_INTEGER - 1, Number.MAX_SAFE_INTEGER],
    [Number.MAX_SAFE_INTEGER, null],
    [-1, null],
    [1.5, null],
    ["1", null],
    [Number.NaN, null],
    [Number.POSITIVE_INFINITY, null],
  ];
  for (const [previous, expected] of cases) {
    assert.equal(nextGatewayGeneration(previous), expected, `previous=${String(previous)}`);
  }
});

test("generation exhaustion does not replace active gateway state", async () => {
  const storage = new MemoryStorage();
  await storage.put("generation", Number.MAX_SAFE_INTEGER);
  await storage.put("host", {
    session_id: "existing",
    instance_id: "old",
    generation: Number.MAX_SAFE_INTEGER,
    expires_at: Date.now() + 60_000,
  });
  const session = new GatewaySession(
    { storage },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  const response = await session.fetch(post("connect", {
    session_id: "existing",
    instance_id: "new",
    platform: "linux",
  }));
  assert.equal(response.status, 503);
  assert.equal((await response.json()).error, "generation_unavailable");
  assert.equal((await storage.get("host")).instance_id, "old");
  assert.equal(await storage.get("generation"), Number.MAX_SAFE_INTEGER);
});

test("registry expiry pruning matches the active-lease model", () => {
  const now = 10_000;
  for (let count = 0; count <= 64; count += 1) {
    for (let expiredEvery = 1; expiredEvery <= 7; expiredEvery += 1) {
      const sessions = {};
      const expected = [];
      for (let index = 0; index < count; index += 1) {
        const expired = index % expiredEvery === 0;
        const expires_at = expired ? now - index - 1 : now + index + 1;
        sessions[`session-${index}`] = { session_id: `session-${index}`, expires_at };
        if (!expired) expected.push(`session-${index}`);
      }
      const changed = pruneExpiredRegistrySessions(sessions, now);
      assert.equal(changed, expected.length !== count);
      assert.deepEqual(Object.keys(sessions), expected);
    }
  }
});

test("registry replacement is monotonic by generation then lease expiry", () => {
  for (let existingGeneration = 1; existingGeneration <= 4; existingGeneration += 1) {
    for (let incomingGeneration = 1; incomingGeneration <= 4; incomingGeneration += 1) {
      for (const existingExpiry of [100, 200, 300]) {
        for (const incomingExpiry of [100, 200, 300]) {
          const actual = shouldReplaceRegistrySession(
            { generation: existingGeneration, expires_at: existingExpiry },
            { generation: incomingGeneration, expires_at: incomingExpiry },
          );
          const expected = incomingGeneration > existingGeneration
            || (incomingGeneration === existingGeneration && incomingExpiry >= existingExpiry);
          assert.equal(
            actual,
            expected,
            `existing=${existingGeneration}/${existingExpiry} incoming=${incomingGeneration}/${incomingExpiry}`,
          );
        }
      }
    }
  }
});

test("stale registry upserts cannot roll back generation or lease expiry", async () => {
  const now = 10_000;
  const storage = new MemoryStorage();
  const registry = new GatewayRegistry({ storage }, {}, { now: () => now });
  const upsert = (generation, expires_at) => registry.fetch(post("upsert", {
    session_id: "monotonic",
    generation,
    expires_at,
  }));

  assert.equal((await upsert(2, now + 200)).status, 204);
  assert.equal((await upsert(1, now + 1000)).status, 204);
  assert.deepEqual(await storage.get("sessions"), {
    monotonic: { session_id: "monotonic", generation: 2, expires_at: now + 200 },
  });

  assert.equal((await upsert(2, now + 100)).status, 204);
  assert.equal((await storage.get("sessions")).monotonic.expires_at, now + 200);
  assert.equal((await upsert(2, now + 300)).status, 204);
  assert.equal((await storage.get("sessions")).monotonic.expires_at, now + 300);
  assert.equal((await upsert(3, now + 50)).status, 204);
  assert.equal((await storage.get("sessions")).monotonic.generation, 3);
});

test("registry capacity prunes expired leases and preserves existing refreshes", async () => {
  const now = 50_000;
  const storage = new MemoryStorage();
  const registry = new GatewayRegistry(
    { storage },
    {},
    { maxSessions: 3, now: () => now },
  );
  await storage.put("sessions", {
    stale: { session_id: "stale", generation: 1, expires_at: now - 1 },
    a: { session_id: "a", generation: 1, expires_at: now + 100 },
    b: { session_id: "b", generation: 1, expires_at: now + 100 },
  });

  const add = (session_id, generation = 1) => registry.fetch(post("upsert", {
    session_id,
    generation,
    expires_at: now + 100,
  }));
  assert.equal((await add("c")).status, 204);
  assert.deepEqual(Object.keys(await storage.get("sessions")).sort(), ["a", "b", "c"]);

  const full = await add("d");
  assert.equal(full.status, 503);
  assert.equal((await full.json()).error, "registry_full");
  assert.deepEqual(Object.keys(await storage.get("sessions")).sort(), ["a", "b", "c"]);

  assert.equal((await add("b", 2)).status, 204);
  assert.equal((await storage.get("sessions")).b.generation, 2);

  const invalid = await registry.fetch(post("upsert", { session_id: "z", generation: 1 }));
  assert.equal(invalid.status, 400);
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

test("queued generation filtering never delivers work to a replacement host", () => {
  for (let count = 0; count <= 64; count += 1) {
    for (let modulus = 2; modulus <= 7; modulus += 1) {
      const session = new GatewaySession(
        { storage: new MemoryStorage() },
        { GATEWAY_REGISTRY: noOpRegistry() },
      );
      const failed = [];
      const expectedCurrent = [];
      for (let index = 0; index < count; index += 1) {
        const generation = index % modulus === 0 ? 1 : 2;
        const request_id = `request-${index}`;
        session.queue.push({ request_id, request: { id: index }, generation });
        session.pending.set(request_id, {
          generation,
          timer: undefined,
          resolve: (result) => failed.push([request_id, result]),
        });
        if (generation === 2) expectedCurrent.push(index);
      }

      const delivered = [];
      while (true) {
        const envelope = session.takeQueuedRequest(2);
        if (!envelope) break;
        delivered.push(envelope.request.id);
      }
      assert.deepEqual(delivered, expectedCurrent, `count=${count} modulus=${modulus}`);
      assert.equal(session.queue.length, 0);
      assert.deepEqual(
        [...session.pending.entries()]
          .filter(([, pending]) => pending.generation === 2)
          .map(([requestId]) => Number(requestId.slice("request-".length))),
        expectedCurrent,
        `current pending count=${count} modulus=${modulus}`,
      );
      assert.equal(
        failed.every(([, result]) => result.status === 502 && result.error === "host_replaced"),
        true,
      );
      assert.equal(failed.length, count - expectedCurrent.length);
    }
  }
});

test("a waiting replacement poll ignores stale queued work and receives only its generation", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  const staleFailures = [];
  let delivered = null;
  session.waitingPoll = {
    generation: 2,
    instance_id: "new-host",
    timer: undefined,
    resolve: (response) => { delivered = response; },
  };
  session.pending.set("old", {
    generation: 1,
    timer: undefined,
    resolve: (result) => staleFailures.push(result),
  });
  session.queue.push({ request_id: "old", request: { id: 1 }, generation: 1 });
  session.flushWaitingPoll();
  assert.equal(delivered, null);
  assert.equal(session.waitingPoll?.generation, 2);
  assert.deepEqual(staleFailures, [{ status: 502, error: "host_replaced" }]);

  session.pending.set("new", { generation: 2, timer: undefined, resolve: () => {} });
  session.queue.push({ request_id: "new", request: { id: 2 }, generation: 2 });
  session.flushWaitingPoll();
  assert.equal(session.waitingPoll, null);
  assert.equal((await delivered.json()).request.id, 2);
});

test("request ID collisions never overwrite pending RPC state", async () => {
  const ids = ["same", "same", "second"];
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
    { requestId: () => ids.shift() },
  );
  await session.fetch(post("connect", {
    session_id: "collision-safe",
    instance_id: "instance-a",
    platform: "linux",
  }));

  const dispatch = (id) => session.fetch(post("dispatch", {
    request: {
      jsonrpc: "2.0",
      id,
      method: "tools/call",
      params: { name: "session_info", arguments: { session_id: "collision-safe" } },
    },
  }));
  const first = dispatch(1);
  await waitFor(() => session.pending.size === 1, "first collision request was not registered");
  const second = dispatch(2);
  await waitFor(() => session.pending.size === 2, "second collision request was not registered");
  assert.deepEqual([...session.pending.keys()], ["same", "second"]);
  assert.deepEqual(session.queue.map((entry) => entry.request_id), ["same", "second"]);

  await session.fetch(post("connect", {
    session_id: "collision-safe",
    instance_id: "instance-b",
    platform: "linux",
  }));
  const settled = await Promise.all([first, second]);
  assert.equal(settled.every((response) => response.status === 502), true);
});

test("request ID collision exhaustion fails without mutating existing pending state", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
    { requestId: () => "same" },
  );
  await session.fetch(post("connect", {
    session_id: "collision-full",
    instance_id: "instance-a",
    platform: "linux",
  }));
  const makeRequest = (id) => post("dispatch", {
    request: {
      jsonrpc: "2.0",
      id,
      method: "tools/call",
      params: { name: "session_info", arguments: { session_id: "collision-full" } },
    },
  });
  const first = session.fetch(makeRequest(1));
  await waitFor(() => session.pending.size === 1, "collision fixture did not register first request");
  assert.deepEqual([...session.pending.keys()], ["same"]);

  const rejected = await session.fetch(makeRequest(2));
  assert.equal(rejected.status, 503);
  assert.equal((await rejected.json()).error, "request_id_unavailable");
  assert.deepEqual([...session.pending.keys()], ["same"]);
  assert.deepEqual(session.queue.map((entry) => entry.request_id), ["same"]);

  await session.fetch(post("connect", {
    session_id: "collision-full",
    instance_id: "instance-b",
    platform: "linux",
  }));
  assert.equal((await first).status, 502);
});

test("timed-out queued requests are removed before a host can execute them", async () => {
  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
    { rpcTimeoutMs: 5 },
  );
  const connected = await body(await session.fetch(post("connect", {
    session_id: "timeout-safe",
    instance_id: "instance-timeout",
    platform: "linux",
  })));

  const timedOut = await session.fetch(post("dispatch", {
    request: {
      jsonrpc: "2.0",
      id: 99,
      method: "tools/call",
      params: { name: "git_push", arguments: { session_id: "timeout-safe" } },
    },
  }));
  assert.equal(timedOut.status, 504);
  assert.equal((await timedOut.json()).error, "host_request_timeout");
  assert.equal(session.pending.size, 0);
  assert.equal(session.queue.length, 0);
  assert.equal(connected.generation, 1);
});

test("queued request cancellation preserves every unrelated request across positions", () => {
  for (let count = 1; count <= 64; count += 1) {
    const positions = new Set([0, Math.floor(count / 2), count - 1]);
    for (const position of positions) {
      const session = new GatewaySession(
        { storage: new MemoryStorage() },
        { GATEWAY_REGISTRY: noOpRegistry() },
      );
      session.queue = Array.from({ length: count }, (_, index) => ({
        request_id: `request-${index}`,
        request: { id: index },
      }));
      session.removeQueuedRequest(`request-${position}`);
      assert.equal(session.queue.length, count - 1, `count=${count} position=${position}`);
      assert.equal(
        session.queue.some((entry) => entry.request_id === `request-${position}`),
        false,
        `count=${count} position=${position}`,
      );
      assert.deepEqual(
        session.queue.map((entry) => entry.request.id),
        Array.from({ length: count }, (_, index) => index).filter((index) => index !== position),
        `count=${count} position=${position}`,
      );
    }
  }
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


test("host respond budget covers maximum get_image payload without widening other host APIs", async () => {
  const rawImageBytes = 32 * 1024 * 1024;
  const maximumBase64Bytes = 4 * Math.ceil(rawImageBytes / 3);
  assert.equal(hostApiBodyLimit("connect"), 8 * 1024 * 1024);
  assert.equal(hostApiBodyLimit("poll"), 8 * 1024 * 1024);
  assert.equal(hostApiBodyLimit("disconnect"), 8 * 1024 * 1024);
  assert.equal(hostApiBodyLimit("respond"), 52 * 1024 * 1024);
  const maximumRequestBytes = 8 * 1024 * 1024;
  const conservativeEnvelopeBytes = 64 * 1024;
  assert.ok(
    maximumBase64Bytes + maximumRequestBytes + conservativeEnvelopeBytes
      < hostApiBodyLimit("respond"),
  );
});

test("Durable Object body budgets preserve dispatch envelopes and host responses", async () => {
  assert.equal(gatewaySessionBodyLimit("connect"), 8 * 1024 * 1024);
  assert.equal(gatewaySessionBodyLimit("poll"), 8 * 1024 * 1024);
  assert.equal(gatewaySessionBodyLimit("disconnect"), 8 * 1024 * 1024);
  assert.equal(gatewaySessionBodyLimit("dispatch"), 8 * 1024 * 1024 + 64 * 1024);
  assert.equal(gatewaySessionBodyLimit("respond"), 52 * 1024 * 1024);

  const session = new GatewaySession(
    { storage: new MemoryStorage() },
    { GATEWAY_REGISTRY: noOpRegistry() },
  );
  const dispatchPadding = "x".repeat(8 * 1024 * 1024);
  const dispatch = await session.fetch(post("dispatch", {
    request: { jsonrpc: "2.0", id: 1, method: "tools/call", padding: dispatchPadding },
  }));
  assert.equal(dispatch.status, 503);
  assert.equal((await dispatch.json()).error, "host_offline");

  await session.fetch(post("connect", {
    session_id: "budgeted",
    instance_id: "budget-instance",
    platform: "linux",
  }));
  const respondPadding = "x".repeat(8 * 1024 * 1024);
  const respond = await session.fetch(post("respond", {
    session_id: "budgeted",
    instance_id: "budget-instance",
    generation: 1,
    request_id: "missing-request",
    response: { jsonrpc: "2.0", id: 1, result: { padding: respondPadding } },
  }));
  assert.equal(respond.status, 409);
  assert.equal((await respond.json()).error, "stale_request");
});

test("host respond accepts payloads above the generic body limit while connect rejects them", async () => {
  const padding = "x".repeat(8 * 1024 * 1024);
  const hostApi = {
    idFromName: (name) => name,
    get: () => ({ fetch: async () => new Response(null, { status: 204 }) }),
  };
  const init = (action) => new Request(`https://gateway.example.test/v1/hosts/${action}`, {
    method: "POST",
    headers: {
      authorization: "Bearer host-token",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      session_id: "mac-main",
      instance_id: "instance-main",
      generation: 1,
      request_id: "request-1",
      response: { jsonrpc: "2.0", id: 1, result: { content: [] } },
      padding,
    }),
  });

  const env = { HOST_TOKEN: "host-token", GATEWAY_SESSIONS: hostApi };
  const responded = await worker.fetch(init("respond"), env);
  assert.equal(responded.status, 204);

  const connected = await worker.fetch(init("connect"), env);
  assert.equal(connected.status, 400);
  assert.match((await connected.json()).detail, /request body is too large/);
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
