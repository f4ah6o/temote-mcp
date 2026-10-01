import test from "node:test";
import assert from "node:assert/strict";
import worker from "../src/index.js";
import { MODERN_PROTOCOL_VERSION, PUBLIC_TOOLS } from "../src/protocol.js";
import { FABRIC_APP_URI } from "../src/fabric-app/index.js";
import { FabricController, refreshDelay, retainView } from "../app/controller.js";

function fixture() {
  const calls = [];
  const now = Date.now();
  const registry = ["host-a", "host-b"].map((host_id) => ({
    host_id, instance_id: host_id, generation: 1, expires_at: now + 60_000,
    connected_at: now - 1000, last_seen: now, platform: "linux",
  }));
  const state = { offline: false, corrupt: false, tasksFail: false };
  const env = {
    CLIENT_TOKEN: "test-client", HOST_TOKEN: "private-host-token",
    HOST_TOKENS_JSON: JSON.stringify({ "host-a": "private-token-a", "host-b": "private-token-b", "offline": "private-token-c" }),
    GATEWAY_DEPLOYMENT: { id: "test-deployment" },
    GATEWAY_REGISTRY: { idFromName: (name) => name, get: () => ({ fetch: async () => new Response(JSON.stringify(registry)) }) },
    GATEWAY_SESSIONS: {
      idFromName: (name) => name,
      get: (key) => ({ async fetch(url, options) {
        const host = key.slice(5);
        if (new URL(url).pathname === "/status") return state.offline
          ? new Response(null, { status: 404 }) : new Response(JSON.stringify({ status: "registered" }));
        const rpc = JSON.parse(options.body).request;
        calls.push({ host, name: rpc.params.name, args: rpc.params.arguments });
        const sessions = [{ session_id: "shared", status: "active", permission_mode: "agent",
          cwd: "/private/path", last_error: "secret-error", workspace: { repository: host, branch: "main", cwd: "/private" },
          ...(state.corrupt ? { host_id: "outside-host" } : {}) }];
        if (rpc.params.name === "task_list" && state.tasksFail) return new Response(null, { status: 503 });
        const value = rpc.params.name === "session_list" ? sessions : {
          tasks: [{ backend: "codex", task_id: `${host}-task`, status: "running", revision: 2,
            last_updated_at: Math.floor(now / 1000), output: "secret-output", prompt: "secret-prompt" }],
          backends: { codex: { status: "ok", total: 1, skipped: 0 } }, total: 1, limit: 64, truncated: false,
        };
        return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result: { content: [{ type: "text", text: JSON.stringify(value) }] } }));
      } }),
    },
  };
  return { env, calls, state };
}

async function rpc(env, method, params = {}, { modern = false, token = "test-client" } = {}) {
  const headers = { "content-type": "application/json" };
  if (token) headers.authorization = `Bearer ${token}`;
  if (modern) {
    headers["mcp-protocol-version"] = MODERN_PROTOCOL_VERSION;
    headers["mcp-method"] = method;
    if (method === "tools/call") headers["mcp-name"] = params.name;
    params = { ...params, _meta: {
      "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
      "io.modelcontextprotocol/clientCapabilities": {},
    } };
  }
  const response = await worker.fetch(new Request("https://fabric.test/mcp", {
    method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  }), env);
  return { response, body: await response.json() };
}

const tool = (env, name, args = {}, options) => rpc(env, "tools/call", { name, arguments: args }, options);

test("Fabric resource and reads use MCP client authentication, not host tokens or dashboard auth", async () => {
  const { env, calls } = fixture();
  for (const token of [null, "wrong", "private-host-token"]) {
    for (const [method, params] of [["resources/read", { uri: FABRIC_APP_URI }], ["tools/call", { name: "fabric_overview", arguments: {} }]]) {
      const result = await rpc(env, method, params, { token });
      assert.equal(result.response.status, 401);
    }
  }
  assert.equal(calls.length, 0);
  const dash = await worker.fetch(new Request("https://fabric.test/dash/", { headers: { authorization: "Bearer test-client" } }), env);
  assert.equal(dash.status, 401);
});

test("legacy and modern discovery, resources, and tools expose a self-contained read-only app", async () => {
  const { env } = fixture();
  for (const modern of [false, true]) {
    const discovery = await rpc(env, modern ? "server/discover" : "initialize", {}, { modern });
    assert.deepEqual(discovery.body.result.capabilities.resources, { listChanged: false, subscribe: false });
    const listed = await rpc(env, "resources/list", {}, { modern });
    assert.equal(listed.body.result.resources[0].uri, FABRIC_APP_URI);
    const read = await rpc(env, "resources/read", { uri: FABRIC_APP_URI }, { modern });
    assert.equal(read.response.headers.get("cache-control"), "no-store");
    const resource = read.body.result.contents[0];
    assert.equal(resource.mimeType, "text/html;profile=mcp-app");
    assert.match(resource.text, /<!doctype html>/i);
    assert.match(resource.text, /Retained tasks/);
    assert.ok(Buffer.byteLength(resource.text) < 2 * 1024 * 1024);
    assert.doesNotMatch(resource.text, /<script[^>]+src=|<link[^>]+href=/);
    assert.deepEqual(resource._meta.ui.csp.connectDomains, []);
    assert.deepEqual(resource._meta["openai/ui"].availableDisplayModes, ["inline", "fullscreen"]);
    const list = await rpc(env, "tools/list", {}, { modern });
    const overview = list.body.result.tools.find((tool) => tool.name === "fabric_overview");
    assert.deepEqual(overview._meta["openai/ui"].entrypoints, [{ type: "global" }]);
    assert.equal(overview._meta.ui.resourceUri, FABRIC_APP_URI);
    const result = await tool(env, "fabric_overview", {}, { modern });
    assert.equal(result.body.result.structuredContent.kind, "fabric_overview");
    assert.equal(result.body.result.resultType, modern ? "complete" : undefined);
  }
  for (const args of [{ uri: "file:///etc/passwd" }, { uri: FABRIC_APP_URI, path: "/private" }]) {
    assert.equal((await rpc(env, "resources/read", args)).body.error.code, -32602);
  }
  assert.ok(PUBLIC_TOOLS.filter((t) => t.name.startsWith("fabric_")).every((t) => t.annotations.readOnlyHint));
});

test("Fabric arguments reject missing scope, extra keys and invalid identifiers before probing", async () => {
  const { env, calls } = fixture();
  for (const [name, args] of [
    ["fabric_overview", { host_id: "host-a" }], ["fabric_session_list", {}],
    ["fabric_session_list", { host_id: "!" }], ["fabric_session_list", { host_id: "../host" }],
    ["fabric_session_read", { host_id: "host-a" }],
    ["fabric_session_read", { host_id: "host-a", session_id: ".." }],
    ["fabric_session_read", { host_id: "host-a", session_id: "shared", limit: 9999 }],
  ]) assert.equal((await tool(env, name, args)).body.error.code, -32602);
  assert.equal(calls.length, 0);
});

test("configured inventory preserves partial failure and redacts credentials", async () => {
  const { env, calls } = fixture();
  const result = await tool(env, "fabric_overview");
  const view = result.body.result.structuredContent;
  assert.deepEqual(view.inventory.data.hosts.map((host) => host.host_id), ["host-a", "host-b", "offline"]);
  assert.equal(view.inventory.data.hosts.at(-1).availability, "offline");
  assert.equal(view.inventory.status, "stale");
  assert.equal(view.service.status, "confirmed");
  assert.doesNotMatch(JSON.stringify(result.body), /private-token|private-host-token|test-client/);
  assert.equal(calls.length, 0);
});

test("same session ID on two hosts stays explicitly scoped and only redacted bounded reads are dispatched", async () => {
  const { env, calls, state } = fixture();
  for (const host of ["host-a", "host-b"]) {
    const result = await tool(env, "fabric_session_read", { host_id: host, session_id: "shared" });
    const view = result.body.result.structuredContent;
    assert.equal(view.session.data.session.workspace.repository, host);
    assert.equal(view.tasks.data.backends[0].tasks[0].task_id, `${host}-task`);
    assert.equal(view.tasks.data.limit, 64);
    assert.equal(view.tasks.status, "stale"); // older host lacks other backend projections
    assert.doesNotMatch(JSON.stringify(result.body), /secret-|\/private/);
  }
  assert.ok(calls.every((call) => ["session_list", "task_list"].includes(call.name)));
  assert.ok(calls.filter((call) => call.name === "task_list").every((call) => call.args.limit === 64 && call.args.session_id === "shared"));
  state.tasksFail = true;
  const partial = (await tool(env, "fabric_session_read", { host_id: "host-a", session_id: "shared" })).body.result.structuredContent;
  assert.equal(partial.session.status, "confirmed");
  assert.equal(partial.tasks.status, "unavailable");
  const before = calls.length;
  const miss = (await tool(env, "fabric_session_read", { host_id: "outside", session_id: "shared" })).body.result.structuredContent;
  assert.equal(miss.session.error_code, "host_not_found");
  assert.equal(calls.length, before);
  state.corrupt = true;
  const corrupt = (await tool(env, "fabric_session_read", { host_id: "host-a", session_id: "shared" })).body.result.structuredContent;
  assert.equal(corrupt.session.status, "unavailable");
  assert.equal(corrupt.tasks, undefined);
});

const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
const envelope = (data, extra = {}) => ({ status: "confirmed", freshness: "live", data, ...extra });
function scopedResult(name, args) {
  const field = name === "fabric_session_list" ? "sessions" : "session";
  return { structuredContent: { kind: name, [field]: envelope(args) } };
}

test("controller uses initial result without a tool call and deduplicates each refresh", async () => {
  const pending = deferred();
  let calls = 0;
  const controller = new FabricController(() => { calls += 1; return pending.promise; }, () => {});
  controller.initial({ structuredContent: { kind: "fabric_overview", inventory: envelope({ hosts: [] }) } });
  assert.equal(calls, 0);
  const one = controller.refresh();
  const two = controller.refresh();
  assert.equal(one, two);
  await Promise.resolve();
  assert.equal(calls, 1);
  pending.resolve({ structuredContent: { kind: "fabric_overview", inventory: envelope({ hosts: [] }) } });
  await one;
  assert.equal(controller.state.busy, false);
});

test("old selection responses cannot overwrite a new selection", async () => {
  const old = deferred();
  const controller = new FabricController((params) => params.name === "fabric_session_read" && params.arguments.host_id === "host-a"
    ? old.promise : Promise.resolve(scopedResult(params.name, params.arguments)), () => {});
  const first = controller.select("host-a", "shared");
  await Promise.resolve();
  await controller.select("host-b", "shared");
  old.resolve(scopedResult("fabric_session_read", { host_id: "host-a", session_id: "shared" }));
  await first;
  assert.equal(controller.state.detail.session.data.host_id, "host-b");
});

test("failed reads retain only visibly stale data, session removal clears tasks, scope mismatches fail", async () => {
  const prior = { kind: "fabric_session_read", session: envelope({ host_id: "a", session_id: "s" }), tasks: envelope({ backends: [] }) };
  const next = { kind: prior.kind, session: envelope({ host_id: "a", session_id: "s" }, { status: "unavailable", error_code: "host_offline" }) };
  assert.equal(retainView(prior, next).tasks.freshness, "stale");
  assert.equal(retainView(prior, next).session.retained, true);
  assert.equal(retainView(prior, { ...next, session: { ...next.session, error_code: "session_not_found" } }).tasks, undefined);
  const controller = new FabricController(() => Promise.resolve(scopedResult("fabric_session_list", { host_id: "other" })), () => {});
  await assert.rejects(controller.request("fabric_session_list", { host_id: "host-a" }), /fabric_scope_mismatch/);
  assert.equal(refreshDelay(false), 5000);
  assert.equal(refreshDelay(true), 30000);
});
