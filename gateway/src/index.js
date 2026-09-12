import {
  MODERN_PROTOCOL_VERSION,
  PUBLIC_TOOLS,
  discoverResult,
  gatewayVersion,
  isModernRequest,
  modernProtocolVersion,
  modernizeResult,
  negotiateProtocolVersion,
  rpcError,
  rpcResult,
  hostIdFromRpc,
  sessionIdFromRpc,
  textResult,
  validateHostId,
  validateModernRequestBody,
  validateSessionId,
} from "./protocol.js";

const HOST_LEASE_MS = 90_000;
const HOST_AGENT_PROTOCOL_VERSION = 1;
const HOST_CONTROL_PROTOCOL_VERSION = 2;
const POLL_TIMEOUT_MS = 20_000;
const RPC_TIMEOUT_MS = 35_000;
const MAX_PENDING_HOST_REQUESTS = 64;
const MAX_REQUEST_ID_ATTEMPTS = 8;
const MAX_REGISTRY_SESSIONS = 1024;
const MAX_REGISTRY_HOSTS = 256;
const MAX_SESSION_STATUS_CHECK_CONCURRENCY = 16;
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
    return withCors(jsonResponse({
      status: "ok",
      service: "temote-mcp-gateway",
      readiness: "ready",
      identity: "temote-mcp-gateway",
    }));
  }
  if (url.pathname === "/mcp") {
    const identity = await authorizeClient(request, env);
    if (!identity) return unauthorizedClient();
    return handleMcp(request, env, identity);
  }
  if (url.pathname.startsWith("/v1/hosts/")) {
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
    host_id: boundedLogField(rpc?.params?.arguments?.host_id),
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
          version: gatewayVersion(env),
        },
        instructions:
          "This is one MCP gateway for multiple federated Temote hosts. Use host_list and session_list, then pass host_id with session_id. Unqualified session IDs are routed only when ownership is unambiguous.",
      }));
    case "server/discover":
      return mcpJson(rpcResult(id, discoverResult(gatewayVersion(env))));
    case "ping": {
      const result = isModernRequest(rpc)
        ? modernizeResult("ping", {}, gatewayVersion(env))
        : {};
      return mcpJson(rpcResult(id, result));
    }
    case "tools/list": {
      const result = { tools: PUBLIC_TOOLS };
      return mcpJson(rpcResult(
        id,
        isModernRequest(rpc)
          ? modernizeResult("tools/list", result, gatewayVersion(env))
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
  if (!PUBLIC_TOOLS.some((tool) => tool.name === name)) {
    return mcpJson(rpcError(id, -32602, `unknown tool: ${name}`));
  }

  const args = rpc?.params?.arguments ?? {};
  if (!args || typeof args !== "object" || Array.isArray(args)) {
    return mcpJson(rpcError(id, -32602, "tool arguments must be an object"));
  }

  if (name === "host_list") {
    if (Object.keys(args).length !== 0) {
      return mcpJson(rpcError(id, -32602, "host_list takes no arguments"));
    }
    const hosts = await readOnlineHosts(env);
    if (!hosts.ok) return mcpJson(rpcError(id, -32001, hosts.error));
    if (hosts.unavailable.length > 0) {
      return mcpJson(rpcError(id, -32006, "host_discovery_unavailable", {
        unavailable_hosts: hosts.unavailable,
      }));
    }
    return toolTextResponse(rpc, env, hosts.value);
  }

  if (name === "host_info") {
    const hostId = hostIdFromRpc(rpc);
    if (!hostId || Object.keys(args).some((key) => key !== "host_id")) {
      return mcpJson(rpcError(id, -32602, "host_info requires only a valid host_id"));
    }
    const hosts = await readOnlineHosts(env);
    if (!hosts.ok) return mcpJson(rpcError(id, -32001, hosts.error));
    if (hosts.unavailable.includes(hostId)) {
      return mcpJson(rpcError(id, -32006, "host_status_unavailable", { host_id: hostId }));
    }
    const host = hosts.value.find((candidate) => candidate.host_id === hostId);
    if (!host) return mcpJson(rpcError(id, -32004, "host_offline", { host_id: hostId }));
    return toolTextResponse(rpc, env, host);
  }

  if (name === "session_list") {
    if (Object.keys(args).some((key) => key !== "host_id")) {
      return mcpJson(rpcError(id, -32602, "session_list accepts only host_id"));
    }
    const hostId = Object.hasOwn(args, "host_id") ? hostIdFromRpc(rpc) : null;
    if (Object.hasOwn(args, "host_id") && !hostId) {
      return mcpJson(rpcError(id, -32602, "invalid params.arguments.host_id"));
    }
    const sessions = await listGatewaySessions(env, hostId);
    if (!sessions.ok) return mcpJson(rpcError(id, sessions.code ?? -32001, sessions.error, sessions.data));
    return toolTextResponse(rpc, env, sessions.value);
  }

  if (name === "session_start") {
    const hostId = hostIdFromRpc(rpc);
    if (!hostId) {
      return mcpJson(rpcError(id, -32602, "session_start requires a valid host_id"));
    }
    if (typeof args.path !== "string" || args.path.length === 0) {
      return mcpJson(rpcError(id, -32602, "session_start requires path"));
    }
    if (Object.hasOwn(args, "session_id") && !validateSessionId(args.session_id)) {
      return mcpJson(rpcError(id, -32602, "invalid params.arguments.session_id"));
    }
    if (Object.keys(args).some((key) => !["host_id", "path", "session_id"].includes(key))) {
      return mcpJson(rpcError(id, -32602, "session_start accepts only host_id, path, and session_id"));
    }
    return proxyToHost(rpc, env, hostId);
  }

  const sessionId = sessionIdFromRpc(rpc);
  if (!sessionId) {
    return mcpJson(rpcError(id, -32602, "missing or invalid params.arguments.session_id"));
  }
  const explicitHostId = Object.hasOwn(args, "host_id") ? hostIdFromRpc(rpc) : null;
  if (Object.hasOwn(args, "host_id") && !explicitHostId) {
    return mcpJson(rpcError(id, -32602, "invalid params.arguments.host_id"));
  }

  if (explicitHostId) return proxyToHost(rpc, env, explicitHostId);

  const route = await resolveUnqualifiedSession(env, sessionId);
  if (!route.ok) {
    return mcpJson(rpcError(id, route.code ?? -32001, route.error, route.data));
  }
  if (route.kind === "host") return proxyToHost(rpc, env, route.host_id);
  return proxyToLegacySession(rpc, env, sessionId);
}

function toolTextResponse(rpc, env, value) {
  const result = textResult(JSON.stringify(value, null, 2));
  return mcpJson(rpcResult(
    rpc.id ?? null,
    isModernRequest(rpc)
      ? modernizeResult("tools/call", result, gatewayVersion(env))
      : result,
  ));
}

function withoutHostRoutingArgument(rpc) {
  const routed = structuredClone(rpc);
  const args = routed?.params?.arguments;
  if (args && typeof args === "object" && !Array.isArray(args)) delete args.host_id;
  return routed;
}

async function proxyToHost(rpc, env, hostId) {
  const routed = withoutHostRoutingArgument(rpc);
  return proxyDispatch(rpc, routed, env, hostStub(env, hostId), { host_id: hostId });
}

async function proxyToLegacySession(rpc, env, sessionId) {
  return proxyDispatch(rpc, rpc, env, sessionStub(env, sessionId), {
    session_id: sessionId,
    routing_mode: "legacy-session",
  });
}

async function proxyDispatch(originalRpc, routedRpc, env, stub, routeData) {
  const id = originalRpc.id ?? null;
  let response;
  try {
    response = await stub.fetch("https://route.internal/dispatch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ request: routedRpc }),
    });
  } catch {
    return mcpJson(rpcError(id, -32001, "host request failed", routeData));
  }
  if (response.ok) {
    const payload = await safeBoundedJson(response, MAX_INTERNAL_RPC_RESPONSE_BYTES, "host dispatch response");
    if (!payload) return mcpJson(rpcError(id, -32001, "host returned invalid JSON", routeData));
    if (isModernRequest(originalRpc) && payload.result) {
      payload.result = modernizeResult(originalRpc.method, payload.result, gatewayVersion(env));
    }
    return mcpJson(payload);
  }

  const failure = await safeBoundedJson(response, MAX_INTERNAL_ERROR_RESPONSE_BYTES, "host dispatch error");
  return mcpJson(rpcError(id, -32001, failure?.error || "host request failed", {
    ...routeData,
    gateway_status: response.status,
    detail: failure?.detail,
  }));
}

async function readRegistryCollection(env, action, maxEntries) {
  let response;
  try {
    response = await registryStub(env).fetch(`https://registry.internal/${action}`);
  } catch {
    return { ok: false, error: "gateway registry unavailable" };
  }
  if (!response.ok) return { ok: false, error: "gateway registry unavailable" };
  const values = await safeBoundedJson(response, MAX_REGISTRY_RESPONSE_BYTES, "gateway registry response");
  if (!Array.isArray(values) || values.length > maxEntries) {
    return { ok: false, error: "gateway registry returned invalid JSON" };
  }
  return { ok: true, value: values };
}

async function readOnlineHosts(env) {
  const hosts = await readRegistryCollection(env, "hosts", MAX_REGISTRY_HOSTS);
  if (!hosts.ok) return hosts;
  const filtered = await filterOnlineRegistryHosts(hosts.value, env);
  return { ok: true, value: filtered.online, unavailable: filtered.unavailable };
}

async function readOnlineLegacySessions(env) {
  const sessions = await readRegistryCollection(env, "list", MAX_REGISTRY_SESSIONS);
  if (!sessions.ok) return sessions;
  const filtered = await filterOnlineRegistrySessions(sessions.value, env);
  return { ok: true, value: filtered.online, unavailable: filtered.unavailable };
}

function parseSessionListPayload(payload, hostId) {
  const text = payload?.result?.content?.find((entry) => entry?.type === "text")?.text;
  if (typeof text !== "string") throw new Error("host session_list returned no text result");
  const sessions = JSON.parse(text);
  if (!Array.isArray(sessions) || sessions.length > MAX_REGISTRY_SESSIONS) {
    throw new Error("host session_list returned invalid session collection");
  }
  return sessions.map((session) => ({ ...session, host_id: hostId, routing_mode: "host" }));
}

async function sessionsFromHost(env, host) {
  const rpc = {
    jsonrpc: "2.0",
    id: `gateway-session-list-${crypto.randomUUID()}`,
    method: "tools/call",
    params: { name: "session_list", arguments: {} },
  };
  let response;
  try {
    response = await hostStub(env, host.host_id).fetch("https://host.internal/dispatch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ request: rpc }),
    });
  } catch {
    return { ok: false, host_id: host.host_id };
  }
  if (!response.ok) return { ok: false, host_id: host.host_id };
  const payload = await safeBoundedJson(response, MAX_INTERNAL_RPC_RESPONSE_BYTES, "host session_list response");
  if (!payload || payload.error) return { ok: false, host_id: host.host_id };
  try {
    return { ok: true, host_id: host.host_id, sessions: parseSessionListPayload(payload, host.host_id) };
  } catch {
    return { ok: false, host_id: host.host_id };
  }
}

async function collectHostSessions(env, hosts) {
  const results = [];
  const limit = MAX_SESSION_STATUS_CHECK_CONCURRENCY;
  for (let offset = 0; offset < hosts.length; offset += limit) {
    const batch = await Promise.all(hosts.slice(offset, offset + limit).map((host) => sessionsFromHost(env, host)));
    results.push(...batch);
  }
  return results;
}

async function listGatewaySessions(env, hostId) {
  const hosts = await readOnlineHosts(env);
  if (!hosts.ok) return hosts;
  if (hostId) {
    if (hosts.unavailable.includes(hostId)) {
      return { ok: false, code: -32006, error: "host_status_unavailable", data: { host_id: hostId } };
    }
    const host = hosts.value.find((candidate) => candidate.host_id === hostId);
    if (!host) return { ok: false, code: -32004, error: "host_offline", data: { host_id: hostId } };
    const result = await sessionsFromHost(env, host);
    if (!result.ok) return { ok: false, error: "host session discovery failed", data: { host_id: hostId } };
    return { ok: true, value: result.sessions };
  }

  if (hosts.unavailable.length > 0) {
    return {
      ok: false,
      code: -32006,
      error: "host session discovery incomplete",
      data: { unavailable_hosts: hosts.unavailable },
    };
  }
  const legacy = await readOnlineLegacySessions(env);
  if (!legacy.ok) return legacy;
  if (legacy.unavailable.length > 0) {
    return {
      ok: false,
      code: -32006,
      error: "legacy session discovery incomplete",
      data: { unavailable_sessions: legacy.unavailable },
    };
  }
  const hostResults = await collectHostSessions(env, hosts.value);
  const unavailable = hostResults.filter((result) => !result.ok).map((result) => result.host_id);
  if (unavailable.length > 0) {
    return { ok: false, error: "host session discovery incomplete", data: { unavailable_hosts: unavailable } };
  }
  const legacySessions = legacy.value.map((session) => ({
    ...session,
    host_id: null,
    routing_mode: "legacy-session",
  }));
  const federated = hostResults.flatMap((result) => result.sessions);
  return { ok: true, value: [...legacySessions, ...federated].sort(compareSessionRoute) };
}

function compareSessionRoute(a, b) {
  const aHost = a.host_id ?? "";
  const bHost = b.host_id ?? "";
  return aHost.localeCompare(bHost) || String(a.session_id ?? "").localeCompare(String(b.session_id ?? ""));
}

async function resolveUnqualifiedSession(env, sessionId) {
  const [hosts, legacy] = await Promise.all([
    readOnlineHosts(env),
    readOnlineLegacySessions(env),
  ]);
  if (!hosts.ok) return hosts;
  if (!legacy.ok) return legacy;
  if (hosts.unavailable.length > 0) {
    return {
      ok: false,
      code: -32006,
      error: "session_ownership_unavailable",
      data: { session_id: sessionId, unavailable_hosts: hosts.unavailable },
    };
  }
  if (legacy.unavailable.includes(sessionId)) {
    return {
      ok: false,
      code: -32006,
      error: "session_ownership_unavailable",
      data: { session_id: sessionId, unavailable_legacy_sessions: [sessionId] },
    };
  }

  const hostResults = await collectHostSessions(env, hosts.value);
  const unavailable = hostResults.filter((result) => !result.ok).map((result) => result.host_id);
  if (unavailable.length > 0) {
    return {
      ok: false,
      error: "session ownership cannot be resolved while a leased host is unavailable",
      data: { session_id: sessionId, unavailable_hosts: unavailable },
    };
  }

  const candidates = [];
  if (legacy.value.some((session) => session.session_id === sessionId)) {
    candidates.push({ kind: "legacy", session_id: sessionId });
  }
  for (const result of hostResults) {
    if (result.sessions.some((session) => session.session_id === sessionId)) {
      candidates.push({ kind: "host", host_id: result.host_id, session_id: sessionId });
    }
  }
  if (candidates.length === 0) {
    return { ok: false, code: -32004, error: "session_missing", data: { session_id: sessionId } };
  }
  if (candidates.length > 1) {
    return {
      ok: false,
      code: -32005,
      error: "ambiguous_session_identity",
      data: {
        session_id: sessionId,
        hosts: candidates.map((candidate) => candidate.kind === "host" ? candidate.host_id : "legacy-session"),
      },
    };
  }
  return { ok: true, ...candidates[0] };
}

async function handleHostApi(request, env, action) {
  if (request.method !== "POST") return withCors(new Response(null, { status: 405 }));
  if (!["connect", "poll", "respond", "disconnect", "status"].includes(action)) {
    return withCors(jsonResponse({ error: "not_found" }, 404));
  }

  const headerHostId = request.headers.get("x-temote-host-id");
  const federated = headerHostId !== null;
  if (federated) {
    if (!validateHostId(headerHostId) || !authorizeFederatedHost(request, env, headerHostId)) {
      return unauthorizedHost();
    }
  } else if (!authorizeLegacyHost(request, env)) {
    return unauthorizedHost();
  }

  const body = await readJson(request, hostApiBodyLimit(action));
  if (!body.ok) return withCors(jsonResponse({ error: "invalid_json", detail: body.error }, 400));

  let stub;
  if (federated) {
    if (body.value?.host_id !== headerHostId || Object.hasOwn(body.value ?? {}, "session_id")) {
      return withCors(jsonResponse({ error: "host_identity_mismatch" }, 403));
    }
    stub = hostStub(env, headerHostId);
  } else {
    const sessionId = body.value?.session_id;
    if (!validateSessionId(sessionId) || Object.hasOwn(body.value ?? {}, "host_id")) {
      return withCors(jsonResponse({ error: "invalid_session_id" }, 400));
    }
    stub = sessionStub(env, sessionId);
  }

  const response = await stub.fetch(`https://route.internal/${action}`, {
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
    if (action === "status") return this.status();
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

  async status() {
    const host = await this.currentHost();
    if (!host) return jsonResponse({ status: "not_registered" }, 404);
    if (!Number.isSafeInteger(host.expires_at) || host.expires_at <= Date.now()) {
      await this.clearHost(host, "host_lease_expired");
      return jsonResponse({ status: "lease_expired" }, 404);
    }
    return jsonResponse({
      status: "registered",
      host_id: host.host_id,
      generation: host.generation,
      lease: "active",
      session_availability: "not_checked",
    });
  }

  async connect(body) {
    const validation = validateAgentIdentity(body, false);
    if (validation) return validation;
    const route = agentRoute(body);
    if (route.host_id) {
      const metadataValidation = validateFederatedHostMetadata(body);
      if (metadataValidation) return metadataValidation;
    }
    const previousGeneration = await this.state.storage.get("generation");
    const generation = nextGatewayGeneration(previousGeneration);
    if (generation === null) {
      return jsonResponse({ error: "generation_unavailable" }, 503);
    }
    const now = Date.now();
    const host = {
      ...route,
      instance_id: body.instance_id,
      generation,
      platform: normalizePlatform(body.platform),
      connected_at: now,
      last_seen: now,
      expires_at: now + HOST_LEASE_MS,
      ...(route.host_id ? {
        agent_protocol: body.agent_protocol,
        runtime_version: body.runtime_version,
        control_protocol: body.control_protocol,
        capabilities: [...body.capabilities],
        named_roots: [...body.named_roots],
        protocol_compatibility: "compatible",
      } : {}),
    };

    this.failAllPending(502, "host_replaced");
    this.queue.length = 0;
    this.replaceWaitingPoll("generation_replaced");
    await this.state.storage.put({ generation, host });
    const registry = await this.upsertRegistry(host);
    if (!registry.ok) {
      await this.clearHost(host, "registry_registration_failed");
      return jsonResponse({
        error: registry.error,
        ...(registry.status ? { gateway_status: registry.status } : {}),
      }, registry.status ?? 503);
    }
    return jsonResponse({
      ...route,
      generation,
      lease_seconds: Math.floor(HOST_LEASE_MS / 1000),
      ...(route.host_id ? { agent_protocol: HOST_AGENT_PROTOCOL_VERSION } : {}),
    });
  }

  async poll(body) {
    const validation = validateAgentIdentity(body, true);
    if (validation) return validation;
    const host = await this.currentHost();
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;

    const now = Date.now();
    host.last_seen = now;
    host.expires_at = now + HOST_LEASE_MS;
    await this.state.storage.put("host", host);
    const registry = await this.upsertRegistry(host);
    if (!registry.ok) {
      await this.clearHost(host, "registry_renewal_failed");
      return jsonResponse({
        error: registry.error,
        ...(registry.status ? { gateway_status: registry.status } : {}),
      }, registry.status ?? 503);
    }

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
    const validation = validateAgentIdentity(body, true);
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
    const registry = await this.upsertRegistry(host);
    if (!registry.ok) {
      await this.clearHost(host, "registry_renewal_failed");
      return jsonResponse({
        error: registry.error,
        ...(registry.status ? { gateway_status: registry.status } : {}),
      }, registry.status ?? 503);
    }
    return new Response(null, { status: 204 });
  }

  async disconnect(body) {
    const validation = validateAgentIdentity(body, true);
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
    let response;
    try {
      response = await registryStub(this.env).fetch("https://registry.internal/upsert", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(host),
      });
    } catch (error) {
      console.error("registry upsert failed", error);
      return { ok: false, status: 503, error: "registry_unavailable" };
    }
    if (response.ok) return { ok: true };

    const failure = await safeBoundedJson(
      response,
      MAX_INTERNAL_ERROR_RESPONSE_BYTES,
      "registry upsert error",
    );
    const error = typeof failure?.error === "string" && /^[a-z0-9_]{1,64}$/.test(failure.error)
      ? failure.error
      : "registry_update_failed";
    const status = response.status >= 400 && response.status <= 599 ? response.status : 503;
    console.error("registry upsert failed", { status, error });
    return { ok: false, status, error };
  }

  async removeRegistry(host) {
    try {
      await registryStub(this.env).fetch("https://registry.internal/remove", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ ...agentRoute(host), generation: host.generation }),
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
    this.maxHosts = options.maxHosts ?? MAX_REGISTRY_HOSTS;
    this.now = options.now ?? (() => Date.now());
  }

  async fetch(request) {
    const action = new URL(request.url).pathname.slice(1);
    if (action === "list") return this.listCollection("sessions", "session_id");
    if (action === "hosts") return this.listCollection("hosts", "host_id");
    const body = await readJson(request);
    if (!body.ok) return jsonResponse({ error: "invalid_json" }, 400);
    if (action === "upsert") return this.upsert(body.value);
    if (action === "remove") return this.remove(body.value);
    return jsonResponse({ error: "not_found" }, 404);
  }

  async listCollection(storageKey, identityKey) {
    const entries = (await this.state.storage.get(storageKey)) || {};
    const changed = pruneExpiredRegistrySessions(entries, this.now());
    if (changed) await this.state.storage.put(storageKey, entries);
    return jsonResponse(
      Object.values(entries).sort((a, b) => String(a[identityKey]).localeCompare(String(b[identityKey]))),
    );
  }

  async upsert(host) {
    const descriptor = registryDescriptor(host, this.maxSessions, this.maxHosts);
    if (
      !descriptor
      || !Number.isSafeInteger(host?.generation)
      || host.generation < 1
      || !Number.isSafeInteger(host?.expires_at)
      || host.expires_at <= 0
    ) {
      return jsonResponse({ error: "invalid_host" }, 400);
    }
    const entries = (await this.state.storage.get(descriptor.storageKey)) || {};
    pruneExpiredRegistrySessions(entries, this.now());
    const existing = entries[descriptor.identity];
    if (!existing && Object.keys(entries).length >= descriptor.limit) {
      await this.state.storage.put(descriptor.storageKey, entries);
      return jsonResponse({ error: "registry_full" }, 503);
    }
    if (existing && !shouldReplaceRegistrySession(existing, host)) {
      await this.state.storage.put(descriptor.storageKey, entries);
      return new Response(null, { status: 204 });
    }
    entries[descriptor.identity] = host;
    await this.state.storage.put(descriptor.storageKey, entries);
    return new Response(null, { status: 204 });
  }

  async remove(body) {
    const descriptor = registryDescriptor(body, this.maxSessions, this.maxHosts);
    if (!descriptor || !Number.isSafeInteger(body?.generation)) {
      return jsonResponse({ error: "invalid_host" }, 400);
    }
    const entries = (await this.state.storage.get(descriptor.storageKey)) || {};
    if (entries[descriptor.identity]?.generation === body.generation) {
      delete entries[descriptor.identity];
      await this.state.storage.put(descriptor.storageKey, entries);
    }
    return new Response(null, { status: 204 });
  }
}

function registryDescriptor(value, maxSessions, maxHosts) {
  if (validateHostId(value?.host_id) && !Object.hasOwn(value ?? {}, "session_id")) {
    return { storageKey: "hosts", identity: value.host_id, limit: maxHosts };
  }
  if (validateSessionId(value?.session_id) && !Object.hasOwn(value ?? {}, "host_id")) {
    return { storageKey: "sessions", identity: value.session_id, limit: maxSessions };
  }
  return null;
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

function agentRoute(value) {
  if (validateHostId(value?.host_id) && !Object.hasOwn(value ?? {}, "session_id")) {
    return { host_id: value.host_id };
  }
  if (validateSessionId(value?.session_id) && !Object.hasOwn(value ?? {}, "host_id")) {
    return { session_id: value.session_id };
  }
  return {};
}

function agentRouteKey(value) {
  const route = agentRoute(value);
  if (route.host_id) return `host:${route.host_id}`;
  if (route.session_id) return `session:${route.session_id}`;
  return null;
}

function validateAgentIdentity(body, requireGeneration) {
  if (!agentRouteKey(body)) return jsonResponse({ error: "invalid_route_identity" }, 400);
  if (typeof body?.instance_id !== "string" || body.instance_id.length < 1 || body.instance_id.length > 128) {
    return jsonResponse({ error: "invalid_instance_id" }, 400);
  }
  if (requireGeneration && (!Number.isSafeInteger(body?.generation) || body.generation < 1)) {
    return jsonResponse({ error: "invalid_generation" }, 400);
  }
  return null;
}

function validateFederatedHostMetadata(body) {
  if (body.agent_protocol !== HOST_AGENT_PROTOCOL_VERSION) {
    return jsonResponse({
      error: "protocol_incompatible",
      supported_agent_protocol: HOST_AGENT_PROTOCOL_VERSION,
    }, 409);
  }
  if (typeof body.runtime_version !== "string" || !utf8Within(body.runtime_version, 128)) {
    return jsonResponse({ error: "invalid_runtime_version" }, 400);
  }
  if (body.control_protocol !== HOST_CONTROL_PROTOCOL_VERSION) {
    return jsonResponse({
      error: "protocol_incompatible",
      supported_control_protocol: HOST_CONTROL_PROTOCOL_VERSION,
    }, 409);
  }
  if (
    !Array.isArray(body.capabilities)
    || body.capabilities.length > 32
    || body.capabilities.some((value) => typeof value !== "string" || value.length < 1 || !utf8Within(value, 64))
  ) {
    return jsonResponse({ error: "invalid_capabilities" }, 400);
  }
  if (
    !Array.isArray(body.named_roots)
    || body.named_roots.length > 64
    || body.named_roots.some((value) => typeof value !== "string" || !/^[A-Za-z0-9_-]+$/.test(value))
  ) {
    return jsonResponse({ error: "invalid_named_roots" }, 400);
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
  if (
    host.generation !== body.generation
    || host.instance_id !== body.instance_id
    || agentRouteKey(host) !== agentRouteKey(body)
  ) {
    return jsonResponse({ error: "stale_generation" }, 409);
  }
  return null;
}

function normalizePlatform(value) {
  return ["macos", "linux", "wsl2", "windows"].includes(value) ? value : "unknown";
}

export async function filterOnlineRegistryHosts(
  hosts,
  env,
  concurrency = MAX_SESSION_STATUS_CHECK_CONCURRENCY,
) {
  const online = [];
  const unavailable = [];
  const limit = Math.max(1, Math.min(MAX_SESSION_STATUS_CHECK_CONCURRENCY, concurrency));
  for (let offset = 0; offset < hosts.length; offset += limit) {
    const batch = hosts.slice(offset, offset + limit);
    const checked = await Promise.all(batch.map(async (host) => {
      const hostId = host?.host_id;
      if (!validateHostId(hostId)) return { state: "unavailable", id: "invalid_registry_host" };
      try {
        const response = await hostStub(env, hostId).fetch("https://host.internal/status");
        if (response.ok) return { state: "online", value: host };
        if (response.status === 404) return { state: "offline" };
        return { state: "unavailable", id: hostId };
      } catch {
        return { state: "unavailable", id: hostId };
      }
    }));
    for (const result of checked) {
      if (result.state === "online") online.push(result.value);
      if (result.state === "unavailable") unavailable.push(result.id);
    }
  }
  return { online, unavailable };
}

export async function filterOnlineRegistrySessions(
  sessions,
  env,
  concurrency = MAX_SESSION_STATUS_CHECK_CONCURRENCY,
) {
  const online = [];
  const unavailable = [];
  const limit = Math.max(1, Math.min(MAX_SESSION_STATUS_CHECK_CONCURRENCY, concurrency));
  for (let offset = 0; offset < sessions.length; offset += limit) {
    const batch = sessions.slice(offset, offset + limit);
    const checked = await Promise.all(batch.map(async (session) => {
      const sessionId = session?.session_id;
      if (!validateSessionId(sessionId)) return { state: "unavailable", id: "invalid_registry_session" };
      try {
        const response = await sessionStub(env, sessionId).fetch("https://session.internal/status");
        if (response.ok) return { state: "online", value: session };
        if (response.status === 404) return { state: "offline" };
        return { state: "unavailable", id: sessionId };
      } catch {
        return { state: "unavailable", id: sessionId };
      }
    }));
    for (const result of checked) {
      if (result.state === "online") online.push(result.value);
      if (result.state === "unavailable") unavailable.push(result.id);
    }
  }
  return { online, unavailable };
}

function sessionStub(env, sessionId) {
  const id = env.GATEWAY_SESSIONS.idFromName(sessionId);
  return env.GATEWAY_SESSIONS.get(id);
}

function hostStub(env, hostId) {
  const id = env.GATEWAY_SESSIONS.idFromName(`host:${hostId}`);
  return env.GATEWAY_SESSIONS.get(id);
}

function registryStub(env) {
  const id = env.GATEWAY_REGISTRY.idFromName("global");
  return env.GATEWAY_REGISTRY.get(id);
}

function authorizeLegacyHost(request, env) {
  if (!env.HOST_TOKEN) return false;
  const authorization = request.headers.get("authorization") || "";
  return authorization === `Bearer ${env.HOST_TOKEN}`;
}

export function federatedHostToken(env, hostId) {
  if (!validateHostId(hostId) || typeof env?.HOST_TOKENS_JSON !== "string") return null;
  try {
    const tokens = JSON.parse(env.HOST_TOKENS_JSON);
    if (!tokens || typeof tokens !== "object" || Array.isArray(tokens)) return null;
    const token = tokens[hostId];
    return typeof token === "string" && token.length > 0 ? token : null;
  } catch {
    return null;
  }
}

function authorizeFederatedHost(request, env, hostId) {
  const token = federatedHostToken(env, hostId);
  if (!token) return false;
  const authorization = request.headers.get("authorization") || "";
  return authorization === `Bearer ${token}`;
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
    "accept,authorization,content-type,mcp-protocol-version,mcp-method,mcp-name,mcp-session-id,cf-access-client-id,cf-access-client-secret,x-temote-host-id",
  );
  headers.set("access-control-expose-headers", "mcp-session-id");
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}
