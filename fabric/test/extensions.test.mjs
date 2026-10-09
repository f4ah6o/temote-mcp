import assert from "node:assert/strict";
import test from "node:test";
import worker from "../src/index.js";
import { EXTENSION_CONTRACT_VERSION, extensionContractFingerprint, handleInteractionCall } from "../src/extensions/index.js";
import { MODERN_PROTOCOL_VERSION, PUBLIC_TOOLS, publicContractFingerprint } from "../src/protocol.js";

const task = "11111111-1111-4111-8111-111111111111";
const interaction = "22222222-2222-4222-8222-222222222222";
const args = { host_id: "host-a", session_id: "session-a", task_id: task, interaction_id: interaction };
const formCaps = { extensions: { "openai/elicitation": { form: {} } } };
function fixture() {
  const state = { revision: 4, status: "waiting_approval", calls: [], kind: "permission",
    deferred: false, missing: false, controlResult: "answered" };
  const env = {
    CLIENT_TOKEN: "client-token", HOST_TOKENS_JSON: JSON.stringify({ "host-a": "host-token" }),
    GATEWAY_DEPLOYMENT: { id: "test" },
    GATEWAY_SESSIONS: { idFromName: (s) => s, get: () => ({ fetch: async (_url, init) => {
      const rpc = JSON.parse(init.body).request;
      state.calls.push(rpc);
      let value;
      if (rpc.params.name === "opencode_task_get") {
        value = { task_id: task, status: state.status, revision: state.revision,
          reconciliation_deferred: state.deferred,
          pending_interactions: state.missing ? [] : [{ interaction_id: interaction,
            kind: state.kind, detail: state.kind === "permission" ? {} : { questions: [{
              custom: false, options: [{ label: "private choice A" }, { label: "private choice B" }],
            }] } }] };
      } else if (rpc.params.name === "opencode_task_control") {
        value = { task_id: task, interaction: { interaction_id: interaction,
          result: state.controlResult, applied: state.controlResult === "answered" } };
      } else throw new Error("unexpected host call");
      return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id,
        result: { content: [{ type: "text", text: JSON.stringify(value) }] } }));
    } }) },
  };
  return { state, env };
}
async function call(env, method, params = {}, caps = formCaps, modern = true, token = "client-token") {
  const headers = { "content-type": "application/json", authorization: `Bearer ${token}` };
  if (modern) {
    headers["mcp-protocol-version"] = MODERN_PROTOCOL_VERSION;
    headers["mcp-method"] = method;
    if (method === "tools/call") headers["mcp-name"] = params.name;
    params = { ...params, _meta: { "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
      "io.modelcontextprotocol/clientCapabilities": caps } };
  }
  const response = await worker.fetch(new Request("https://fabric.test/mcp", { method: "POST",
    headers, body: JSON.stringify({ jsonrpc: "2.0", id: crypto.randomUUID(), method, params }) }), env);
  return { status: response.status, body: await response.json() };
}
const read = (env, extra = {}, caps = formCaps, modern = true, token) => call(env, "tools/call",
  { name: "fabric_interaction_read", arguments: args, ...extra }, caps, modern, token);
const retry = (env, state, answer = "once", extra = {}) => read(env, {
  requestState: state, inputResponses: { interaction: { action: "accept", content: { answer } } }, ...extra,
});

test("optional catalog is request scoped and core contract stays exact", async () => {
  const { env } = fixture();
  const core = await publicContractFingerprint();
  const standard = await call(env, "tools/list", {}, {});
  assert.deepEqual(standard.body.result.tools.map((t) => t.name), PUBLIC_TOOLS.map((t) => t.name));
  const ui = await call(env, "tools/list", {}, { extensions: { "io.modelcontextprotocol/ui": {} } });
  assert.ok(ui.body.result.tools.some((t) => t.name === "search_mentions"));
  assert.ok(!ui.body.result.tools.some((t) => t.name === "fabric_interaction_read"));
  const forms = await call(env, "tools/list");
  assert.ok(forms.body.result.tools.some((t) => t.name === "fabric_interaction_read"));
  assert.ok(!forms.body.result.tools.some((t) => t.name === "search_mentions"));
  assert.equal((await call(env, "tools/call", { name: "search_mentions", arguments: { query: "" } }, {})).body.error.message,
    "unknown tool: search_mentions");
  assert.equal((await call(env, "tools/call", { name: "search_mentions", arguments: { query: "" } },
    { extensions: { "io.modelcontextprotocol/ui": {} } })).body.error.message, "mention_source_unavailable");
  assert.equal(EXTENSION_CONTRACT_VERSION, 2);
  assert.equal((await extensionContractFingerprint()).length, 64);
  assert.equal(await publicContractFingerprint(), core);
});

test("public HTTP MRTR reads Host and answers through its idempotent control receipt", async () => {
  const { env, state } = fixture();
  const first = await read(env);
  assert.equal(first.body.result.resultType, "input_required");
  assert.equal(first.body.result.inputRequests.interaction.method, "openai/elicitation/create");
  assert.equal(state.calls[0].params.name, "opencode_task_get");
  assert.doesNotMatch(JSON.stringify(first.body), /private choice|host-token|\/Users\/|requestState.*operation_id/);
  const done = await retry(env, first.body.result.requestState);
  assert.equal(JSON.parse(done.body.result.content[0].text).mode, "applied");
  assert.equal(state.calls.at(-1).params.name, "opencode_task_control");
  assert.equal(state.calls.at(-1).params.arguments.answer.reply, "once");
  assert.match(state.calls.at(-1).params.arguments.operation_id, /^[0-9a-f-]{36}$/);
  const again = await retry(env, first.body.result.requestState);
  assert.equal(state.calls.at(-1).params.arguments.operation_id, state.calls.at(-3).params.arguments.operation_id);
  assert.equal(JSON.parse(again.body.result.content[0].text).mode, "applied");
});

test("legacy, unauthenticated, missing key and unsupported question paths fail or fall back", async () => {
  const { env, state } = fixture();
  assert.equal((await read(env, {}, formCaps, true, "wrong")).status, 401);
  assert.equal((await read(env, {}, {}, true)).body.error.code, -32602);
  assert.equal((await read(env, {}, formCaps, false)).body.error.code, -32602);
  delete env.HOST_TOKENS_JSON;
  const fallback = await read(env);
  assert.equal(JSON.parse(fallback.body.result.content[0].text).mode, "standard");
  assert.equal(state.calls.length, 1);
  env.HOST_TOKENS_JSON = JSON.stringify({ "host-a": "host-token" });
  state.kind = "question";
  const originalFetch = env.GATEWAY_SESSIONS.get;
  env.GATEWAY_SESSIONS.get = (...params) => {
    const stub = originalFetch(...params);
    return { fetch: async (url, init) => {
      const response = await stub.fetch(url, init);
      const body = await response.json();
      const view = JSON.parse(body.result.content[0].text);
      view.pending_interactions[0].detail.questions[0].custom = true;
      body.result.content[0].text = JSON.stringify(view);
      return new Response(JSON.stringify(body));
    } };
  };
  assert.equal(JSON.parse((await read(env)).body.result.content[0].text).mode, "standard");
});

test("stale, deferred, unknown, tampered, rotated, and expired state never controls Host", async () => {
  const { env, state } = fixture();
  const first = (await read(env)).body.result.requestState;
  const base = state.calls.length;
  const separator = first.lastIndexOf(".");
  const body = first.slice(0, separator);
  const mac = first.slice(separator + 1);
  const macBytes = Buffer.from(mac, "base64url");
  assert.equal(macBytes.length, 32);
  const tamperedMacBytes = Buffer.from(macBytes);
  tamperedMacBytes[0] ^= 1;
  assert.notDeepEqual(tamperedMacBytes, macBytes);
  const tampered = `${body}.${tamperedMacBytes.toString("base64url")}`;
  assert.equal((await retry(env, tampered)).body.error.code, -32602);
  assert.equal(state.calls.length, base);
  env.HOST_TOKENS_JSON = JSON.stringify({ "host-a": "rotated-token" });
  assert.equal((await retry(env, first)).body.error.code, -32602);
  env.HOST_TOKENS_JSON = JSON.stringify({ "host-a": "host-token" });
  state.revision = 5;
  assert.equal((await retry(env, first)).body.error.code, -32004);
  state.revision = 4; state.deferred = true;
  assert.equal((await retry(env, first)).body.error.code, -32004);
  state.deferred = false; state.status = "unknown";
  assert.equal((await retry(env, first)).body.error.code, -32004);
  state.status = "waiting_approval"; state.missing = true;
  assert.equal((await retry(env, first)).body.error.code, -32004);
  assert.ok(state.calls.every((c) => c.params.name === "opencode_task_get"));
});

test("numbered question options map only from the current Host read", async () => {
  const { env, state } = fixture();
  state.kind = "question";
  const first = await read(env);
  assert.deepEqual(first.body.result.inputRequests.interaction.params.requestedSchema.properties.answer.enum, ["1", "2"]);
  assert.doesNotMatch(JSON.stringify(first.body), /private choice/);
  const done = await retry(env, first.body.result.requestState, "2");
  assert.equal(JSON.parse(done.body.result.content[0].text).mode, "applied");
  assert.deepEqual(state.calls.at(-1).params.arguments.answer, { answers: [["private choice B"]] });
});

test("state is bound to arguments, principal and expiry before any Host read", async () => {
  const { env } = fixture();
  const first = (await read(env)).body.result.requestState;
  const meta = { "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
    "io.modelcontextprotocol/clientCapabilities": formCaps };
  const makeRpc = (argumentsValue) => ({ jsonrpc: "2.0", id: 8, method: "tools/call", params: {
    name: "fabric_interaction_read", arguments: argumentsValue, _meta: meta,
    requestState: first, inputResponses: { interaction: { action: "accept", content: { answer: "once" } } },
  } });
  let called = false;
  const hostCall = async () => { called = true; throw new Error("Host must not be read"); };
  assert.equal((await handleInteractionCall(makeRpc(args), env, { subject: "another-owner" }, hostCall)).error.code, -32602);
  assert.equal((await handleInteractionCall(makeRpc({ ...args, session_id: "session-b" }), env,
    { subject: "client-token" }, hostCall)).error.code, -32602);
  assert.equal((await handleInteractionCall(makeRpc(args), env, { subject: "client-token" }, hostCall,
    () => Math.floor(Date.now() / 1000) + 121)).error.code, -32602);
  assert.equal(called, false);
});
