import {
  MODERN_PROTOCOL_VERSION,
  PUBLIC_TOOLS,
  discoverResult,
  isModernRequest,
  modernProtocolVersion,
  modernizeResult,
  negotiateProtocolVersion,
  rpcError,
  rpcResult,
  sessionIdFromRpc,
  textResult,
  validateModernRequestBody,
  validateSessionId,
} from "./protocol.js";

const HOST_LEASE_MS = 90_000;
const POLL_TIMEOUT_MS = 20_000;
const RPC_TIMEOUT_MS = 35_000;
const MAX_PENDING_HOST_REQUESTS = 64;
const MAX_REQUEST_ID_ATTEMPTS = 8;
const MAX_REGISTRY_SESSIONS = 1024;
const MAX_BODY_BYTES = 8 * 1024 * 1024;
const MAX_INTERNAL_DISPATCH_ENVELOPE_BYTES = 64 * 1024;
const MAX_INTERNAL_DISPATCH_BODY_BYTES = MAX_BODY_BYTES + MAX_INTERNAL_DISPATCH_ENVELOPE_BYTES;
// A 32 MiB image expands to ~42.7 MiB base64, and the JSON-RPC id may consume most of the 8 MiB public request budget.
const MAX_HOST_RESPONSE_BODY_BYTES = 52 * 1024 * 1024;
const MAX_INTERNAL_RPC_RESPONSE_BYTES = MAX_HOST_RESPONSE_BODY_BYTES;
const MAX_INTERNAL_ERROR_RESPONSE_BYTES = 64 * 1024;
const MAX_REGISTRY_RESPONSE_BYTES = 1024 * 1024;
const MAX_JWKS_BYTES = 1024 * 1024;
const MAX_ACCESS_JWT_BYTES = 64 * 1024;
const MAX_ACCESS_JWT_HEADER_BYTES = 8 * 1024;
const MAX_ACCESS_JWT_CLAIMS_BYTES = 32 * 1024;
const MAX_ACCESS_JWT_SIGNATURE_BYTES = 8 * 1024;
const MAX_ACCESS_KID_CHARS = 256;
const MAX_LOG_FIELD_CHARS = 256;
const MAX_RPC_METHOD_BYTES = 256;
const MAX_RPC_ID_BYTES = 256;
const MAX_RPC_TOOL_NAME_BYTES = 256;
const jwksCache = new Map();

export default {
  async fetch(request, env) {
    try {
      return await handleRequest(request, env);
    } catch (error) {
      console.error("gateway request failed", error);
      return withCors(jsonResponse({ error: "internal_error" }, 500));
    }
  },
};

async function handleRequest(request, env) {
  const url = new URL(request.url);
  if (request.method === "OPTIONS") return withCors(new Response(null, { status: 204 }));
  if (url.pathname === "/healthz") {
    return withCors(jsonResponse({ status: "ok", service: "temote-mcp-gateway" }));
  }
  if (url.pathname === "/mcp") {
    const identity = await authorizeClient(request, env);
    if (!identity) return unauthorizedClient();
    return handleMcp(request, env, identity);
  }
  if (url.pathname.startsWith("/v1/hosts/")) {
    if (!authorizeHost(request, env)) return unauthorizedHost();
    return handleHostApi(request, env, url.pathname.slice("/v1/hosts/".length));
  }
  return withCors(jsonResponse({ error: "not_found" }, 404));
}

async function handleMcp(request, env, identity) {
  if (request.method === "GET") {
    return withCors(new Response("SSE is not supported; use Streamable HTTP POST", { status: 405 }));
  }
  if (request.method === "DELETE") return withCors(new Response(null, { status: 200 }));
  if (request.method !== "POST") return withCors(new Response(null, { status: 405 }));

  const body = await readJson(request);
  if (!body.ok) return withCors(jsonResponse(rpcError(null, -32700, body.error), 400));
  const rpc = body.value;
  if (!validRpcRequestShape(rpc)) {
    return withCors(jsonResponse(rpcError(null, -32600, "invalid JSON-RPC request"), 400));
  }
  const id = rpc?.id ?? null;
  const protocolError = validateModernHttpRequest(request, rpc);
  if (protocolError) return protocolError;
  if (rpc?.id === undefined) return withCors(new Response(null, { status: 202 }));

  console.log(JSON.stringify({
    event: "mcp_request",
    subject: boundedLogField(identity.subject),
    email: boundedLogField(identity.email),
    method: boundedLogField(rpc?.method, "unknown"),
    tool: boundedLogField(rpc?.params?.name),
    session_id: boundedLogField(rpc?.params?.arguments?.session_id),
  }));

  switch (rpc?.method) {
    case "initialize":
      return mcpJson(rpcResult(id, {
        protocolVersion: negotiateProtocolVersion(rpc),
        capabilities: { tools: { listChanged: false } },
        serverInfo: {
          name: "temote-mcp-gateway",
          title: "Temote MCP Gateway",
          version: env.GATEWAY_VERSION || "2026.8.0",
        },
        instructions:
          "This is one MCP gateway for multiple endpoint sessions. Use session_list, then pass the selected session_id to every other tool. Mac and Windows/WSL2 sessions use different IDs.",
      }));
    case "server/discover":
      return mcpJson(rpcResult(id, discoverResult(env.GATEWAY_VERSION || "2026.8.0")));
    case "ping": {
      const result = isModernRequest(rpc)
        ? modernizeResult("ping", {}, env.GATEWAY_VERSION || "2026.8.0")
        : {};
      return mcpJson(rpcResult(id, result));
    }
    case "tools/list": {
      const result = { tools: PUBLIC_TOOLS };
      return mcpJson(rpcResult(
        id,
        isModernRequest(rpc)
          ? modernizeResult("tools/list", result, env.GATEWAY_VERSION || "2026.8.0")
          : result,
      ));
    }
    case "tools/call":
      return handleToolCall(rpc, env);
    default:
      return mcpJson(rpcError(id, -32601, `method not found: ${rpc?.method ?? ""}`));
  }
}

function validateModernHttpRequest(request, rpc) {
  const bodyVersion = modernProtocolVersion(rpc);
  const headerVersion = request.headers.get("mcp-protocol-version");
  const modern = isModernRequest(rpc) || headerVersion === MODERN_PROTOCOL_VERSION;
  if (!modern) return null;

  const id = rpc?.id ?? null;
  const bodyError = validateModernRequestBody(rpc);
  if (bodyError) {
    return withCors(jsonResponse(rpcError(id, bodyError.code, bodyError.message, bodyError.data), 400));
  }
  if (headerVersion !== bodyVersion) {
    return withCors(jsonResponse(
      rpcError(id, -32020, "MCP-Protocol-Version header must match request _meta"),
      400,
    ));
  }
  if (bodyVersion !== MODERN_PROTOCOL_VERSION) {
    return withCors(jsonResponse(
      rpcError(id, -32022, "unsupported MCP protocol version", {
        supported: [MODERN_PROTOCOL_VERSION],
        requested: bodyVersion,
      }),
      400,
    ));
  }
  const method = typeof rpc?.method === "string" ? rpc.method : "";
  if (request.headers.get("mcp-method") !== method) {
    return withCors(jsonResponse(
      rpcError(id, -32020, "Mcp-Method header must match the JSON-RPC method"),
      400,
    ));
  }
  if (method === "tools/call") {
    const name = rpc?.params?.name;
    if (typeof name !== "string" || request.headers.get("mcp-name") !== name) {
      return withCors(jsonResponse(
        rpcError(id, -32020, "Mcp-Name header must match params.name"),
        400,
      ));
    }
  }
  return null;
}

async function handleToolCall(rpc, env) {
  const id = rpc.id ?? null;
  const name = rpc?.params?.name;
  if (!validRpcToolName(name)) return mcpJson(rpcError(id, -32602, "missing or invalid tool name"));

  if (name === "session_list") {
    const args = rpc?.params?.arguments ?? {};
    if (Object.keys(args).length !== 0) {
      return mcpJson(rpcError(id, -32602, "session_list takes no arguments"));
    }
    const registry = registryStub(env);
    const response = await registry.fetch("https://registry.internal/list");
    if (!response.ok) return mcpJson(rpcError(id, -32001, "session registry unavailable"));
    const sessions = await safeBoundedJson(response, MAX_REGISTRY_RESPONSE_BYTES, "session registry response");
    if (!Array.isArray(sessions)) {
      return mcpJson(rpcError(id, -32001, "session registry returned invalid JSON"));
    }
    const result = textResult(JSON.stringify(sessions, null, 2));
    return mcpJson(rpcResult(
      id,
      isModernRequest(rpc)
        ? modernizeResult("tools/call", result, env.GATEWAY_VERSION || "2026.8.0")
        : result,
    ));
  }

  if (!PUBLIC_TOOLS.some((tool) => tool.name === name)) {
    return mcpJson(rpcError(id, -32602, `unknown tool: ${name}`));
  }
  const sessionId = sessionIdFromRpc(rpc);
  if (!sessionId) {
    return mcpJson(rpcError(id, -32602, "missing or invalid params.arguments.session_id"));
  }

  const stub = sessionStub(env, sessionId);
  const response = await stub.fetch("https://session.internal/dispatch", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ request: rpc }),
  });
  if (response.ok) {
    const payload = await safeBoundedJson(response, MAX_INTERNAL_RPC_RESPONSE_BYTES, "host dispatch response");
    if (!payload) return mcpJson(rpcError(id, -32001, "host returned invalid JSON"));
    if (isModernRequest(rpc) && payload.result) {
      payload.result = modernizeResult(
        rpc.method,
        payload.result,
        env.GATEWAY_VERSION || "2026.8.0",
      );
    }
    return mcpJson(payload);
  }

  const failure = await safeBoundedJson(response, MAX_INTERNAL_ERROR_RESPONSE_BYTES, "host dispatch error");
  const message = failure?.error || "host request failed";
  return mcpJson(rpcError(id, -32001, message, {
    session_id: sessionId,
    gateway_status: response.status,
    detail: failure?.detail,
  }));
}

async function handleHostApi(request, env, action) {
  if (request.method !== "POST") return withCors(new Response(null, { status: 405 }));
  if (!["connect", "poll", "respond", "disconnect"].includes(action)) {
    return withCors(jsonResponse({ error: "not_found" }, 404));
  }
  const body = await readJson(request, hostApiBodyLimit(action));
  if (!body.ok) return withCors(jsonResponse({ error: "invalid_json", detail: body.error }, 400));
  const sessionId = body.value?.session_id;
  if (!validateSessionId(sessionId)) {
    return withCors(jsonResponse({ error: "invalid_session_id" }, 400));
  }
  const stub = sessionStub(env, sessionId);
  const response = await stub.fetch(`https://session.internal/${action}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body.value),
  });
  return withCors(response);
}

export class GatewaySession {
  constructor(state, env, options = {}) {
    this.state = state;
    this.env = env;
    this.rpcTimeoutMs = options.rpcTimeoutMs ?? RPC_TIMEOUT_MS;
    this.requestId = options.requestId ?? (() => crypto.randomUUID());
    this.pending = new Map();
    this.queue = [];
    this.waitingPoll = null;
  }

  async fetch(request) {
    const action = new URL(request.url).pathname.slice(1);
    const body = await readJson(request, gatewaySessionBodyLimit(action));
    if (!body.ok) return jsonResponse({ error: "invalid_json", detail: body.error }, 400);
    switch (action) {
      case "connect":
        return this.connect(body.value);
      case "poll":
        return this.poll(body.value);
      case "respond":
        return this.respond(body.value);
      case "disconnect":
        return this.disconnect(body.value);
      case "dispatch":
        return this.dispatch(body.value);
      default:
        return jsonResponse({ error: "not_found" }, 404);
    }
  }

  async connect(body) {
    const validation = validateHostIdentity(body, false);
    if (validation) return validation;
    const previousGeneration = await this.state.storage.get("generation");
    const generation = nextGatewayGeneration(previousGeneration);
    if (generation === null) {
      return jsonResponse({ error: "generation_unavailable" }, 503);
    }
    const now = Date.now();
    const host = {
      session_id: body.session_id,
      instance_id: body.instance_id,
      generation,
      platform: normalizePlatform(body.platform),
      connected_at: now,
      last_seen: now,
      expires_at: now + HOST_LEASE_MS,
    };

    this.failAllPending(502, "host_replaced");
    this.queue.length = 0;
    this.replaceWaitingPoll("generation_replaced");
    await this.state.storage.put({ generation, host });
    await this.upsertRegistry(host);
    return jsonResponse({
      session_id: host.session_id,
      generation,
      lease_seconds: Math.floor(HOST_LEASE_MS / 1000),
    });
  }

  async poll(body) {
    const validation = validateHostIdentity(body, true);
    if (validation) return validation;
    const host = await this.currentHost();
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;

    const now = Date.now();
    host.last_seen = now;
    host.expires_at = now + HOST_LEASE_MS;
    await this.state.storage.put("host", host);
    await this.upsertRegistry(host);

    const queued = this.takeQueuedRequest(host.generation);
    if (queued) return jsonResponse(queued);
    this.replaceWaitingPoll("poll_replaced");
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        if (this.waitingPoll?.resolve === resolve) this.waitingPoll = null;
        resolve(new Response(null, { status: 204 }));
      }, POLL_TIMEOUT_MS);
      this.waitingPoll = {
        resolve,
        timer,
        generation: body.generation,
        instance_id: body.instance_id,
      };
    });
  }

  async respond(body) {
    const validation = validateHostIdentity(body, true);
    if (validation) return validation;
    if (typeof body.request_id !== "string" || !body.response || typeof body.response !== "object") {
      return jsonResponse({ error: "invalid_response" }, 400);
    }
    const host = await this.currentHost();
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;

    const pending = this.pending.get(body.request_id);
    if (!pending) return jsonResponse({ error: "stale_request" }, 409);
    if (!validHostRpcResponse(body.response, pending.rpc_id)) {
      return jsonResponse({ error: "invalid_response", detail: "JSON-RPC response does not match pending request" }, 400);
    }
    clearTimeout(pending.timer);
    this.pending.delete(body.request_id);
    pending.resolve({ response: body.response });

    const now = Date.now();
    host.last_seen = now;
    host.expires_at = now + HOST_LEASE_MS;
    await this.state.storage.put("host", host);
    await this.upsertRegistry(host);
    return new Response(null, { status: 204 });
  }

  async disconnect(body) {
    const validation = validateHostIdentity(body, true);
    if (validation) return validation;
    const host = await this.currentHost();
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;
    await this.clearHost(host, "host_disconnected");
    return new Response(null, { status: 204 });
  }

  async dispatch(body) {
    const request = body?.request;
    if (!request || typeof request !== "object" || !validRpcId(request.id)) {
      return jsonResponse({ error: "invalid_rpc_request" }, 400);
    }
    const host = await this.currentHost();
    if (!host) return jsonResponse({ error: "host_offline" }, 503);
    if (host.expires_at <= Date.now()) {
      await this.clearHost(host, "host_lease_expired");
      return jsonResponse({ error: "host_offline", detail: "lease expired" }, 503);
    }

    if (this.pending.size >= MAX_PENDING_HOST_REQUESTS) {
      return jsonResponse({ error: "gateway_busy", detail: "too many pending host requests" }, 503);
    }

    const requestId = this.allocateRequestId();
    if (!requestId) {
      return jsonResponse({ error: "request_id_unavailable" }, 503);
    }
    const outcome = new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(requestId);
        this.removeQueuedRequest(requestId);
        resolve({ status: 504, error: "host_request_timeout" });
      }, this.rpcTimeoutMs);
      this.pending.set(requestId, { resolve, timer, generation: host.generation, rpc_id: request.id });
    });
    this.queue.push({ request_id: requestId, request, generation: host.generation });
    this.flushWaitingPoll();

    const result = await outcome;
    if (result.response) return jsonResponse(result.response);
    return jsonResponse({ error: result.error }, result.status);
  }

  async currentHost() {
    return (await this.state.storage.get("host")) || null;
  }

  allocateRequestId() {
    for (let attempt = 0; attempt < MAX_REQUEST_ID_ATTEMPTS; attempt += 1) {
      const requestId = this.requestId();
      if (
        typeof requestId === "string"
        && requestId.length > 0
        && !this.pending.has(requestId)
        && !this.queue.some((entry) => entry.request_id === requestId)
      ) {
        return requestId;
      }
    }
    return null;
  }

  takeQueuedRequest(generation) {
    while (this.queue.length > 0) {
      const entry = this.queue.shift();
      if (entry.generation === generation) {
        return { request_id: entry.request_id, request: entry.request };
      }
      const pending = this.pending.get(entry.request_id);
      if (pending && pending.generation === entry.generation) {
        clearTimeout(pending.timer);
        this.pending.delete(entry.request_id);
        pending.resolve({ status: 502, error: "host_replaced" });
      }
    }
    return null;
  }

  removeQueuedRequest(requestId) {
    const index = this.queue.findIndex((entry) => entry.request_id === requestId);
    if (index >= 0) this.queue.splice(index, 1);
  }

  flushWaitingPoll() {
    if (!this.waitingPoll || this.queue.length === 0) return;
    const waiting = this.waitingPoll;
    const queued = this.takeQueuedRequest(waiting.generation);
    if (!queued) return;
    this.waitingPoll = null;
    clearTimeout(waiting.timer);
    waiting.resolve(jsonResponse(queued));
  }

  replaceWaitingPoll(error) {
    if (!this.waitingPoll) return;
    const waiting = this.waitingPoll;
    this.waitingPoll = null;
    clearTimeout(waiting.timer);
    waiting.resolve(jsonResponse({ error }, 409));
  }

  failAllPending(status, error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.resolve({ status, error });
    }
    this.pending.clear();
  }

  async clearHost(host, reason) {
    this.failAllPending(503, reason);
    this.queue.length = 0;
    this.replaceWaitingPoll(reason);
    await this.state.storage.delete("host");
    await this.removeRegistry(host);
  }

  async upsertRegistry(host) {
    try {
      await registryStub(this.env).fetch("https://registry.internal/upsert", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(host),
      });
    } catch (error) {
      console.error("registry upsert failed", error);
    }
  }

  async removeRegistry(host) {
    try {
      await registryStub(this.env).fetch("https://registry.internal/remove", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ session_id: host.session_id, generation: host.generation }),
      });
    } catch (error) {
      console.error("registry remove failed", error);
    }
  }
}

export class GatewayRegistry {
  constructor(state, _env, options = {}) {
    this.state = state;
    this.maxSessions = options.maxSessions ?? MAX_REGISTRY_SESSIONS;
    this.now = options.now ?? (() => Date.now());
  }

  async fetch(request) {
    const action = new URL(request.url).pathname.slice(1);
    if (action === "list") return this.list();
    const body = await readJson(request);
    if (!body.ok) return jsonResponse({ error: "invalid_json" }, 400);
    if (action === "upsert") return this.upsert(body.value);
    if (action === "remove") return this.remove(body.value);
    return jsonResponse({ error: "not_found" }, 404);
  }

  async list() {
    const sessions = (await this.state.storage.get("sessions")) || {};
    const changed = pruneExpiredRegistrySessions(sessions, this.now());
    if (changed) await this.state.storage.put("sessions", sessions);
    return jsonResponse(Object.values(sessions).sort((a, b) => a.session_id.localeCompare(b.session_id)));
  }

  async upsert(host) {
    if (
      !validateSessionId(host?.session_id)
      || !Number.isSafeInteger(host?.generation)
      || host.generation < 1
      || !Number.isSafeInteger(host?.expires_at)
      || host.expires_at <= 0
    ) {
      return jsonResponse({ error: "invalid_host" }, 400);
    }
    const sessions = (await this.state.storage.get("sessions")) || {};
    pruneExpiredRegistrySessions(sessions, this.now());
    const existing = sessions[host.session_id];
    if (!existing && Object.keys(sessions).length >= this.maxSessions) {
      await this.state.storage.put("sessions", sessions);
      return jsonResponse({ error: "registry_full" }, 503);
    }
    if (existing && !shouldReplaceRegistrySession(existing, host)) {
      await this.state.storage.put("sessions", sessions);
      return new Response(null, { status: 204 });
    }
    sessions[host.session_id] = host;
    await this.state.storage.put("sessions", sessions);
    return new Response(null, { status: 204 });
  }

  async remove(body) {
    if (!validateSessionId(body?.session_id) || !Number.isSafeInteger(body?.generation)) {
      return jsonResponse({ error: "invalid_host" }, 400);
    }
    const sessions = (await this.state.storage.get("sessions")) || {};
    if (sessions[body.session_id]?.generation === body.generation) {
      delete sessions[body.session_id];
      await this.state.storage.put("sessions", sessions);
    }
    return new Response(null, { status: 204 });
  }
}

function utf8Within(value, maxBytes) {
  if (typeof value !== "string" || value.length > maxBytes) return false;
  return new TextEncoder().encode(value).byteLength <= maxBytes;
}

export function validRpcId(value) {
  return value === null
    || (typeof value === "string" && utf8Within(value, MAX_RPC_ID_BYTES))
    || (typeof value === "number" && Number.isFinite(value));
}

export function validRpcToolName(value) {
  return typeof value === "string" && value.length > 0 && utf8Within(value, MAX_RPC_TOOL_NAME_BYTES);
}

export function validRpcRequestShape(request) {
  if (!request || typeof request !== "object" || Array.isArray(request)) return false;
  if (request.jsonrpc !== "2.0") return false;
  if (typeof request.method !== "string" || request.method.length === 0 || !utf8Within(request.method, MAX_RPC_METHOD_BYTES)) return false;
  return !Object.hasOwn(request, "id") || validRpcId(request.id);
}

export function validHostRpcResponse(response, expectedId) {
  if (!response || typeof response !== "object" || Array.isArray(response)) return false;
  if (response.jsonrpc !== "2.0" || !validRpcId(response.id) || response.id !== expectedId) return false;
  const hasResult = Object.prototype.hasOwnProperty.call(response, "result");
  const hasError = Object.prototype.hasOwnProperty.call(response, "error");
  return hasResult !== hasError;
}

export function nextGatewayGeneration(previous) {
  if (previous === undefined || previous === null) return 1;
  if (!Number.isSafeInteger(previous) || previous < 0 || previous >= Number.MAX_SAFE_INTEGER) {
    return null;
  }
  return previous + 1;
}

export function shouldReplaceRegistrySession(existing, incoming) {
  if (incoming.generation !== existing.generation) {
    return incoming.generation > existing.generation;
  }
  return incoming.expires_at >= existing.expires_at;
}

export function pruneExpiredRegistrySessions(sessions, now) {
  let changed = false;
  for (const [sessionId, session] of Object.entries(sessions)) {
    if (!Number.isSafeInteger(session?.expires_at) || session.expires_at <= now) {
      delete sessions[sessionId];
      changed = true;
    }
  }
  return changed;
}

function validateHostIdentity(body, requireGeneration) {
  if (!validateSessionId(body?.session_id)) return jsonResponse({ error: "invalid_session_id" }, 400);
  if (typeof body?.instance_id !== "string" || body.instance_id.length < 1 || body.instance_id.length > 128) {
    return jsonResponse({ error: "invalid_instance_id" }, 400);
  }
  if (requireGeneration && (!Number.isSafeInteger(body?.generation) || body.generation < 1)) {
    return jsonResponse({ error: "invalid_generation" }, 400);
  }
  return null;
}

export function hostApiBodyLimit(action) {
  return action === "respond" ? MAX_HOST_RESPONSE_BODY_BYTES : MAX_BODY_BYTES;
}

export function gatewaySessionBodyLimit(action) {
  if (action === "respond") return MAX_HOST_RESPONSE_BODY_BYTES;
  if (action === "dispatch") return MAX_INTERNAL_DISPATCH_BODY_BYTES;
  return MAX_BODY_BYTES;
}

function verifyGeneration(host, body) {
  if (!host) return jsonResponse({ error: "host_offline" }, 409);
  if (host.generation !== body.generation || host.instance_id !== body.instance_id) {
    return jsonResponse({ error: "stale_generation" }, 409);
  }
  return null;
}

function normalizePlatform(value) {
  return ["macos", "linux", "wsl2", "windows"].includes(value) ? value : "unknown";
}

function sessionStub(env, sessionId) {
  const id = env.GATEWAY_SESSIONS.idFromName(sessionId);
  return env.GATEWAY_SESSIONS.get(id);
}

function registryStub(env) {
  const id = env.GATEWAY_REGISTRY.idFromName("global");
  return env.GATEWAY_REGISTRY.get(id);
}

function authorizeHost(request, env) {
  if (!env.HOST_TOKEN) return false;
  const authorization = request.headers.get("authorization") || "";
  return authorization === `Bearer ${env.HOST_TOKEN}`;
}

async function authorizeClient(request, env) {
  const authorization = request.headers.get("authorization") || "";
  if (env.CLIENT_TOKEN && authorization === `Bearer ${env.CLIENT_TOKEN}`) {
    return { subject: "client-token", email: "-" };
  }
  const assertion = request.headers.get("cf-access-jwt-assertion");
  if (!assertion) return null;
  try {
    return await verifyAccessJwt(assertion, env);
  } catch (error) {
    console.error("Access JWT rejected", error);
    return null;
  }
}

async function verifyAccessJwt(token, env) {
  if (!env.ACCESS_TEAM_DOMAIN || !env.ACCESS_AUDIENCE) {
    throw new Error("Access JWT validation is not configured");
  }
  const parts = validateAccessJwtShape(token);
  const header = decodeJwtPart(parts[0]);
  const claims = decodeJwtPart(parts[1]);
  if (
    header.alg !== "RS256"
    || !accessKidAllowed(header.kid)
  ) throw new Error("unsupported JWT key");

  const issuer = normalizeAccessTeamDomain(env.ACCESS_TEAM_DOMAIN);
  const jwks = await getJwks(issuer);
  const jwk = jwks.find((candidate) => candidate.kid === header.kid);
  if (!jwk) throw new Error("JWT signing key not found");
  const key = await crypto.subtle.importKey(
    "jwk",
    jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const valid = await crypto.subtle.verify(
    "RSASSA-PKCS1-v1_5",
    key,
    base64UrlBytes(parts[2]),
    new TextEncoder().encode(`${parts[0]}.${parts[1]}`),
  );
  if (!valid) throw new Error("invalid JWT signature");

  const now = Math.floor(Date.now() / 1000);
  if (typeof claims.exp !== "number" || claims.exp <= now) throw new Error("expired JWT");
  if (typeof claims.nbf === "number" && claims.nbf > now + 60) throw new Error("JWT not active");
  if (claims.iss !== issuer) throw new Error("invalid JWT issuer");
  const audiences = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  if (!audiences.includes(env.ACCESS_AUDIENCE)) throw new Error("invalid JWT audience");

  const email = typeof claims.email === "string" ? claims.email : "";
  if (!accessEmailAllowed(env.ACCESS_ALLOWED_EMAILS, email)) {
    throw new Error("email is not allowed or ACCESS_ALLOWED_EMAILS is empty");
  }
  if (typeof claims.sub !== "string" || !claims.sub) throw new Error("JWT subject missing");
  return { subject: claims.sub, email: claims.email || "-" };
}

export function boundedLogField(value, fallback = "-") {
  if (typeof value !== "string") return fallback;
  if (value.length <= MAX_LOG_FIELD_CHARS) return value;
  return `${value.slice(0, MAX_LOG_FIELD_CHARS)}…`;
}

export function validateAccessJwtShape(token) {
  if (typeof token !== "string" || token.length === 0 || token.length > MAX_ACCESS_JWT_BYTES) {
    throw new Error("invalid JWT size");
  }
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("invalid JWT");
  const limits = [MAX_ACCESS_JWT_HEADER_BYTES, MAX_ACCESS_JWT_CLAIMS_BYTES, MAX_ACCESS_JWT_SIGNATURE_BYTES];
  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index];
    if (part.length === 0 || part.length > limits[index] || !/^[A-Za-z0-9_-]+$/.test(part)) {
      throw new Error("invalid JWT segment");
    }
  }
  return parts;
}

export function accessKidAllowed(value) {
  return typeof value === "string" && value.length > 0 && value.length <= MAX_ACCESS_KID_CHARS;
}

export function normalizeAccessTeamDomain(value) {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error("ACCESS_TEAM_DOMAIN is invalid");
  }
  const raw = value.trim();
  const candidate = raw.includes("://") ? raw : `https://${raw}`;
  let parsed;
  try {
    parsed = new URL(candidate);
  } catch {
    throw new Error("ACCESS_TEAM_DOMAIN is invalid");
  }
  if (
    parsed.protocol !== "https:"
    || parsed.username !== ""
    || parsed.password !== ""
    || parsed.search !== ""
    || parsed.hash !== ""
    || parsed.pathname.replaceAll("/", "") !== ""
    || parsed.hostname === ""
  ) {
    throw new Error("ACCESS_TEAM_DOMAIN must be an HTTPS origin without a path");
  }
  return parsed.origin;
}

export function accessEmailAllowed(configured, email) {
  const allowedEmails = (configured || "")
    .split(",")
    .map((value) => value.trim().toLowerCase())
    .filter(Boolean);
  if (allowedEmails.length === 0) return false;
  const normalizedEmail = typeof email === "string" ? email.trim().toLowerCase() : "";
  return normalizedEmail.length > 0 && allowedEmails.includes(normalizedEmail);
}

async function getJwks(teamOrigin) {
  const cached = jwksCache.get(teamOrigin);
  if (cached && cached.expiresAt > Date.now()) return cached.keys;
  const response = await fetch(`${teamOrigin}/cdn-cgi/access/certs`, {
    cf: { cacheTtl: 300, cacheEverything: true },
  });
  if (!response.ok) throw new Error(`failed to fetch Access keys: ${response.status}`);
  const body = await readBoundedJson(response, MAX_JWKS_BYTES, "Access key response");
  if (!Array.isArray(body.keys)) throw new Error("invalid Access key response");
  jwksCache.set(teamOrigin, { keys: body.keys, expiresAt: Date.now() + 300_000 });
  return body.keys;
}

function decodeJwtPart(value) {
  return JSON.parse(new TextDecoder().decode(base64UrlBytes(value)));
}

function base64UrlBytes(value) {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const decoded = atob(padded);
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}

async function readJson(request, limit = MAX_BODY_BYTES) {
  try {
    return { ok: true, value: await readBoundedJson(request, limit, "request body") };
  } catch (error) {
    return { ok: false, error: String(error) };
  }
}

async function readBoundedJson(message, limit, label) {
  const bytes = await readBoundedBytes(message, limit, label);
  return JSON.parse(new TextDecoder().decode(bytes));
}

export async function readBoundedBytes(message, limit, label) {
  const rawLength = message.headers.get("content-length");
  if (rawLength !== null) {
    const length = Number(rawLength);
    if (!Number.isSafeInteger(length) || length < 0 || length > limit) {
      throw new Error(`${label} is too large`);
    }
  }
  if (!message.body) return new Uint8Array();

  const reader = message.body.getReader();
  const chunks = [];
  let total = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = value instanceof Uint8Array ? value : new Uint8Array(value);
      if (chunk.byteLength > limit - total) throw new Error(`${label} is too large`);
      chunks.push(chunk);
      total += chunk.byteLength;
    }
  } finally {
    reader.releaseLock();
  }

  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

async function safeBoundedJson(response, limit, label) {
  try {
    return await readBoundedJson(response, limit, label);
  } catch {
    return null;
  }
}

function unauthorizedClient() {
  return withCors(jsonResponse({ error: "access_unauthorized" }, 401, { "cache-control": "no-store" }));
}

function unauthorizedHost() {
  return withCors(jsonResponse({ error: "host_unauthorized" }, 401, { "cache-control": "no-store" }));
}

function mcpJson(value) {
  return withCors(jsonResponse(value, 200, mcpHeaders()));
}

function mcpHeaders() {
  return { "content-type": "application/json", "cache-control": "no-store" };
}

function jsonResponse(value, status = 200, extraHeaders = {}) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", ...extraHeaders },
  });
}

function withCors(response) {
  const headers = new Headers(response.headers);
  headers.set("access-control-allow-origin", "*");
  headers.set("access-control-allow-methods", "GET,POST,DELETE,OPTIONS");
  headers.set(
    "access-control-allow-headers",
    "accept,authorization,content-type,mcp-protocol-version,mcp-method,mcp-name,mcp-session-id,cf-access-client-id,cf-access-client-secret",
  );
  headers.set("access-control-expose-headers", "mcp-session-id");
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}
