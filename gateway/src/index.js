import {
  MODERN_PROTOCOL_VERSION,
  PUBLIC_TOOLS,
  discoverResult,
  gatewayVersion,
  isModernRequest,
  modernProtocolVersion,
  modernizeResult,
  negotiateProtocolVersion,
  publicContractFingerprint,
  rpcError,
  rpcResult,
  hostIdFromRpc,
  sessionIdFromRpc,
  textResult,
  validateHostId,
  validateModernRequestBody,
  validateSessionId,
} from "./protocol.js";
import {
  authorizeClient,
  authorizeFederatedHost,
  authorizeLegacyHost,
  boundedLogField,
} from "./access.js";
import {
  agentRoute,
  agentRouteKey,
  compareSessionRoute,
  hostStub,
  registryStub,
  sessionStub,
  withoutHostRoutingArgument,
} from "./routing.js";
import {
  handleHostApi,
  listGatewaySessions,
  proxyToHost,
  proxyToLegacySession,
  readOnlineHosts,
  resolveUnqualifiedSession,
  validRpcRequestShape,
  validRpcToolName,
} from "./routing-runtime.js";
import {
  jsonResponse,
  mcpJson,
  readJson,
  unauthorizedClient,
  withCors,
} from "./http.js";

export {
  accessEmailAllowed,
  accessKidAllowed,
  boundedLogField,
  federatedHostToken,
  normalizeAccessTeamDomain,
  validateAccessJwtShape,
} from "./access.js";
export {
  OBSERVATION_SCHEMA_VERSION,
  observationPlaneBindings,
} from "./observation/index.js";
export { contextPlaneBindings } from "./context/index.js";
export { GatewayRegistry, GatewaySession } from "./routing-runtime.js";
export {
  gatewaySessionBodyLimit,
  hostApiBodyLimit,
  nextGatewayGeneration,
  normalizeSessionAvailability,
  pruneExpiredRegistrySessions,
  shouldReplaceRegistrySession,
  validHostRpcResponse,
  validRpcId,
  validRpcRequestShape,
  validRpcToolName,
} from "./routing-runtime.js";
export { readBoundedBytes } from "./http.js";

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
      contractFingerprint: await publicContractFingerprint(),
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
