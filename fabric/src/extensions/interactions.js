import { federatedHostToken } from "../access.js";
import { isModernRequest, rpcError, rpcResult, validateHostId, validateSessionId } from "../protocol.js";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const encoder = new TextEncoder();
const plain = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
const now = () => Math.floor(Date.now() / 1000);
const b64 = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const unb64 = (s) => {
  if (typeof s !== "string" || !/^[A-Za-z0-9_-]+$/.test(s) || s.length > 4096) return null;
  try { return Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0)); }
  catch { return null; }
};
const canonical = (v) => Array.isArray(v) ? `[${v.map(canonical).join(",")}]`
  : plain(v) ? `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canonical(v[k])}`).join(",")}}`
    : JSON.stringify(v);
async function digest(v) {
  return b64(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(canonical(v)))));
}
async function keyFor(env, hostId) {
  const material = typeof env?.FABRIC_INTERACTION_SECRET === "string" && env.FABRIC_INTERACTION_SECRET.length >= 32
    ? env.FABRIC_INTERACTION_SECRET : federatedHostToken(env, hostId);
  if (!material) return null;
  const base = await crypto.subtle.importKey("raw", encoder.encode(material), "HKDF", false, ["deriveKey"]);
  return crypto.subtle.deriveKey({ name: "HKDF", hash: "SHA-256",
    salt: encoder.encode("temote-fabric-interaction-v2"), info: encoder.encode("mcp-mrtr-request-state") },
  base, { name: "HMAC", hash: "SHA-256", length: 256 }, false, ["sign", "verify"]);
}
async function seal(payload, key) {
  const body = b64(encoder.encode(JSON.stringify(payload)));
  const mac = b64(new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(body))));
  return `${body}.${mac}`;
}
async function unseal(state, key) {
  if (!key || typeof state !== "string" || state.length > 4096) return null;
  const parts = state.split(".");
  if (parts.length !== 2) return null;
  const body = unb64(parts[0]); const mac = unb64(parts[1]);
  if (!body || !mac || mac.length !== 32 || !await crypto.subtle.verify("HMAC", key, mac, encoder.encode(parts[0]))) return null;
  try { const value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body)); return plain(value) ? value : null; }
  catch { return null; }
}

export const INTERACTION_TOOL_NAME = "fabric_interaction_read";
export const INTERACTION_TOOL = Object.freeze({
  name: INTERACTION_TOOL_NAME, title: "Read a pending Host interaction",
  description: "Read one fresh OpenCode interaction. Compatible clients may receive an MCP form; other clients use the ordinary OpenCode task tools.",
  annotations: { readOnlyHint: false, openWorldHint: true },
  inputSchema: { type: "object", additionalProperties: false, properties: {
    host_id: { type: "string" }, session_id: { type: "string" },
    task_id: { type: "string", format: "uuid" }, interaction_id: { type: "string", format: "uuid" },
  }, required: ["host_id", "session_id", "task_id", "interaction_id"] },
});
export function supportsOpenAIForm(rpc) {
  return isModernRequest(rpc) && plain(rpc?.params?._meta?.["io.modelcontextprotocol/clientCapabilities"]?.extensions?.["openai/elicitation"]?.form);
}
function validArgs(a) {
  return plain(a) && Object.keys(a).length === 4 && validateHostId(a.host_id) && validateSessionId(a.session_id)
    && UUID.test(a.task_id ?? "") && UUID.test(a.interaction_id ?? "");
}
function hostView(response) {
  if (!response?.ok) return null;
  const content = response.value?.result?.content;
  if (!Array.isArray(content) || content.length !== 1 || content[0]?.type !== "text"
    || typeof content[0].text !== "string" || content[0].text.length > 256 * 1024) return null;
  try { const v = JSON.parse(content[0].text); return plain(v) ? v : null; }
  catch { return null; }
}
function selected(view, args) {
  if (!view || view.task_id !== args.task_id || !Number.isSafeInteger(view.revision) || view.revision < 1
    || view.reconciliation_deferred === true || view.reconciliation_required === true
    || view.pending_interactions_unavailable === true || view.status !== "waiting_approval"
    || !Array.isArray(view.pending_interactions) || view.pending_interactions.length > 64) return null;
  const matches = view.pending_interactions.filter((i) => i?.interaction_id === args.interaction_id);
  return matches.length === 1 && ["permission", "question"].includes(matches[0].kind) ? matches[0] : null;
}
function choices(item) {
  if (item.kind === "permission") return ["reject", "once"];
  const questions = item.detail?.questions;
  if (!Array.isArray(questions) || questions.length !== 1 || questions[0]?.multiple === true
    || questions[0]?.custom !== false || !Array.isArray(questions[0]?.options)
    || questions[0].options.length < 2 || questions[0].options.length > 8) return null;
  const labels = questions[0].options.map((o) => o?.label);
  return labels.every((s) => typeof s === "string" && s.length > 0 && s.length <= 256)
    && new Set(labels).size === labels.length ? labels : null;
}
function fallback(a, view, item) {
  return { backend: "opencode", host_id: a.host_id, session_id: a.session_id, task_id: a.task_id,
    interaction_id: a.interaction_id, revision: view.revision, kind: item.kind,
    standard_read_tool: "opencode_task_get", standard_control_tool: "opencode_task_control" };
}
function form(item, labels) {
  return { mode: "form",
    message: item.kind === "permission"
      ? "Choose reject or allow once. Inspect the Host task before deciding."
      : "Choose an option number. Inspect the Host task for the option meanings.",
    requestedSchema: { type: "object", properties: { answer: { type: "string",
      enum: item.kind === "permission" ? ["reject", "once"] : labels.map((_, i) => String(i + 1)) } },
    required: ["answer"] } };
}
const result = (rpc, v) => rpcResult(rpc.id, { content: [{ type: "text", text: JSON.stringify(v) }] });
export async function handleInteractionCall(rpc, env, identity, hostCall, clock = now) {
  const a = rpc?.params?.arguments;
  if (!identity?.subject) return rpcError(rpc.id, -32001, "unauthorized");
  if (!validArgs(a)) return rpcError(rpc.id, -32602, "invalid interaction scope");
  const resumed = Object.hasOwn(rpc.params, "requestState") || Object.hasOwn(rpc.params, "inputResponses");
  if (resumed && (!supportsOpenAIForm(rpc) || typeof rpc.params.requestState !== "string"))
    return rpcError(rpc.id, -32602, "invalid interaction retry");
  const key = await keyFor(env, a.host_id);
  if (resumed && !key) return rpcError(rpc.id, -32001, "interaction_signing_unavailable");
  const binding = await digest({ method: rpc.method, name: rpc.params.name, arguments: a });
  const principal = await digest(identity.subject);
  let state;
  if (resumed) {
    state = await unseal(rpc.params.requestState, key);
    if (!state || state.v !== 2 || state.binding !== binding || state.principal !== principal
      || state.interaction_id !== a.interaction_id || !UUID.test(state.operation_id ?? "")
      || !Number.isSafeInteger(state.revision) || !Number.isSafeInteger(state.expires_at)
      || state.expires_at <= clock() || state.expires_at > clock() + 120)
      return rpcError(rpc.id, -32602, "invalid_or_expired_request_state");
  }
  const read = { jsonrpc: "2.0", id: rpc.id, method: "tools/call", params: {
    name: "opencode_task_get", arguments: { session_id: a.session_id, task_id: a.task_id } } };
  const view = hostView(await hostCall(read, env, a.host_id));
  const item = selected(view, a);
  if (!item) return rpcError(rpc.id, -32004, "interaction_stale_or_unavailable");
  const labels = choices(item);
  const standard = fallback(a, view, item);
  if (!resumed) {
    if (!supportsOpenAIForm(rpc) || !key || !labels) return result(rpc, { mode: "standard", ...standard });
    const requestState = await seal({ v: 2, binding, principal, revision: view.revision,
      interaction_id: a.interaction_id, expires_at: clock() + 120, operation_id: crypto.randomUUID() }, key);
    return rpcResult(rpc.id, { resultType: "input_required", inputRequests: {
      interaction: { method: "openai/elicitation/create", params: form(item, labels) },
    }, requestState });
  }
  if (state.revision !== view.revision || !labels)
    return rpcError(rpc.id, -32004, "interaction_stale_or_unavailable");
  const response = rpc.params.inputResponses?.interaction;
  if (!plain(response) || !["accept", "decline", "cancel"].includes(response.action))
    return rpcError(rpc.id, -32602, "invalid_input_responses");
  if (response.action !== "accept") return result(rpc, { mode: "declined", ...standard });
  const answer = response.content?.answer;
  const options = item.kind === "permission" ? ["reject", "once"] : labels.map((_, i) => String(i + 1));
  if (!plain(response.content) || Object.keys(response.content).length !== 1 || !options.includes(answer))
    return rpcError(rpc.id, -32602, "invalid_input_responses");
  const control = { jsonrpc: "2.0", id: rpc.id, method: "tools/call", params: {
    name: "opencode_task_control", arguments: { session_id: a.session_id, task_id: a.task_id,
      action: "answer", interaction_id: a.interaction_id, operation_id: state.operation_id,
      answer: item.kind === "permission" ? { reply: answer } : { answers: [[labels[Number(answer) - 1]]] } } } };
  const applied = hostView(await hostCall(control, env, a.host_id));
  if (!applied || applied.task_id !== a.task_id || applied.interaction?.interaction_id !== a.interaction_id
    || applied.interaction.result !== "answered" || applied.interaction.applied !== true)
    return rpcError(rpc.id, -32001, "interaction_control_rejected");
  return result(rpc, { mode: "applied", ...standard, operation_id: state.operation_id });
}
