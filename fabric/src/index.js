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
  serverInfo,
  hostIdFromRpc,
  sessionIdFromRpc,
  textResult,
  validateHostId,
  validateModernRequestBody,
  validateSessionId,
} from "./protocol.js";
import {
  authorizeClient,
  boundedLogField,
} from "./access.js";
import { activeBrowserGrant, handleEnrollmentRequest, sameBrowserGrantSnapshot, sweepHostGrantOutbox } from "./enrollment.js";
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
  sessionsFromHost,
  validRpcRequestShape,
  validRpcToolName,
} from "./routing-runtime.js";
import {
  jsonResponse,
  mcpJson,
  readJson,
  safeBoundedJson,
  unauthorizedClient,
  withCors,
} from "./http.js";
import {
  handleObservationSync,
  observationSyncHostId,
} from "./observation/index.js";
import {
  callFabricTool, fabricResources, isFabricTool, readFabricResource,
  FABRIC_RESOURCES_CAPABILITY,
} from "./fabric-app/index.js";
import { resolveCloudContext } from "./context/index.js";
import {
  handleDashboardRequest,
  isDashboardPathname,
} from "./dashboard/index.js";
import { consumeMemoryBatch, sweepMemoryOutbox } from "./memory/index.js";
import { durableReady, repositoryReady } from "./events/repository.js";
import { eventsEligible, handleEventMethod } from "./events/service.js";
import { sweepEventOutbox } from "./events/outbox.js";
import { handleHostTransition, transitionHostId } from "./events/ingest.js";
import {
  extensionTools, handleExtensionToolCall, handleInteractionCall,
  INTERACTION_TOOL_NAME, MENTION_TOOL_NAME, supportsOpenAIForm,
} from "./extensions/index.js";
import { validateSessionStart } from "./session-start.js";

export {
  accessEmailAllowed,
  accessKidAllowed,
  boundedLogField,
  federatedHostToken,
  normalizeAccessTeamDomain,
  validateAccessJwtShape,
} from "./access.js";
export { observationPlaneBindings } from "./observation/index.js";
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
  async queue(batch, env) {
    await consumeMemoryBatch(batch, env);
  },
  async scheduled(_controller, env) {
    await sweepMemoryOutbox(env);
    await sweepEventOutbox(env);
    await sweepHostGrantOutbox(env);
  },
  async fetch(request, env) {
    if (isDashboardPathname(new URL(request.url).pathname)) {
      return handleDashboardRequest(request, env);
    }
    try {
      return await handleRequest(request, env);
    } catch (error) {
      // Errors may contain provider/SQL payloads. Ordinary logs carry only a
      // stable diagnostic code, never request or extractor content.
      console.error("gateway request failed: internal_error");
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
      service: "temote-fabric",
      readiness: "ready",
      identity: "temote-fabric",
      compatibilityIdentity: "temote-mcp-gateway",
      contractFingerprint: await publicContractFingerprint(),
    }));
  }
  if (url.pathname === "/mcp") {
    const identity = await authorizeClient(request, env);
    if (!identity) return unauthorizedClient();
    return handleMcp(request, env, identity);
  }
  if (url.pathname === "/v1/enrollment-identity" || url.pathname.startsWith("/v1/enrollments/")) {
    return handleEnrollmentRequest(request, env, url.pathname);
  }
  const observationHostId = observationSyncHostId(url.pathname);
  if (observationHostId) {
    return handleObservationSync(request, env, observationHostId);
  }
  const eventHostId = transitionHostId(url.pathname);
  if (eventHostId) return handleHostTransition(request, env, eventHostId);
  if (url.pathname.startsWith("/v1/hosts/")) {
    return handleHostApi(request, env, url.pathname.slice("/v1/hosts/".length));
  }
  return withCors(jsonResponse({ error: "not_found" }, 404));
}

async function handleMcp(request, env, identity) {
  const browserMode = identity.auth_mode === "browser_owner";
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
        capabilities: { tools: { listChanged: false }, resources: FABRIC_RESOURCES_CAPABILITY },
        serverInfo: serverInfo(gatewayVersion(env)),
        instructions:
          "This is one MCP gateway for multiple federated Temote hosts. Use host_list and session_list, then pass host_id with session_id. Unqualified session IDs are routed only when ownership is unambiguous.",
      }));
    case "server/discover":
      if (browserMode) return mcpJson(rpcError(id, -32601, "method not found: server/discover"));
      return mcpJson(rpcResult(id, discoverResult(gatewayVersion(env), eventsEligible(identity) && await repositoryReady(env))));
    case "events/list":
    case "events/subscribe":
    case "events/unsubscribe": {
      if (browserMode) return mcpJson(rpcError(id, -32601, `method not found: ${rpc.method}`));
      if (!isModernRequest(rpc) || !eventsEligible(identity)
        || !await (rpc.method === "events/unsubscribe" ? durableReady(env) : repositoryReady(env))) {
        return mcpJson(rpcError(id, -32601, `method not found: ${rpc.method}`));
      }
      const eventResponse = await handleEventMethod(rpc, env, identity);
      if (eventResponse.result) eventResponse.result = modernizeResult(rpc.method, eventResponse.result, gatewayVersion(env));
      return mcpJson(eventResponse);
    }
    case "ping": {
      const result = isModernRequest(rpc)
        ? modernizeResult("ping", {}, gatewayVersion(env))
        : {};
      return mcpJson(rpcResult(id, result));
    }
    case "tools/list": {
      const tools = browserMode ? PUBLIC_TOOLS.filter((tool) => browserToolAllowed(tool.name)) : [
        ...PUBLIC_TOOLS, ...extensionTools({
          mentions: supportsUiExtension(rpc), interactions: supportsOpenAIForm(rpc),
        }),
      ];
      const result = { tools };
      return mcpJson(rpcResult(
        id,
        isModernRequest(rpc)
          ? modernizeResult("tools/list", result, gatewayVersion(env))
          : result,
      ));
    }
    case "resources/list": {
      if (browserMode) return mcpJson(rpcError(id, -32601, "method not found: resources/list"));
      const result = fabricResources();
      return mcpJson(rpcResult(id, isModernRequest(rpc)
        ? modernizeResult("resources/list", result, gatewayVersion(env)) : result));
    }
    case "resources/read": {
      if (browserMode) return mcpJson(rpcError(id, -32601, "method not found: resources/read"));
      const resource = readFabricResource(rpc.params);
      if (resource.error) return mcpJson(rpcError(id, resource.error.code, resource.error.message));
      return mcpJson(rpcResult(id, isModernRequest(rpc)
        ? modernizeResult("resources/read", resource.result, gatewayVersion(env)) : resource.result));
    }
    case "tools/call":
      return handleToolCall(rpc, env, identity);
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

function supportsUiExtension(rpc) {
  const value = rpc?.params?._meta?.["io.modelcontextprotocol/clientCapabilities"]?.extensions?.["io.modelcontextprotocol/ui"];
  return isModernRequest(rpc) && value !== null && typeof value === "object" && !Array.isArray(value);
}

async function extensionHostCall(hostRpc, env, hostId) {
  const response = await proxyToHost(hostRpc, env, hostId);
  if (!response.ok) return { ok: false };
  const value = await safeBoundedJson(response, 300 * 1024, "interaction Host response");
  return value ? { ok: true, value } : { ok: false };
}

async function handleToolCall(rpc, env, identity) {
  const id = rpc.id ?? null;
  const name = rpc?.params?.name;
  if (!validRpcToolName(name)) return mcpJson(rpcError(id, -32602, "missing or invalid tool name"));
  const browserMode = identity.auth_mode === "browser_owner";
  if (browserMode && !browserToolAllowed(name)) {
    return mcpJson(rpcError(id, -32601, `tool unavailable for this identity: ${name}`));
  }
  if (name === MENTION_TOOL_NAME && supportsUiExtension(rpc)) {
    const response = await handleExtensionToolCall(rpc, env, identity);
    if (response.result) response.result = modernizeResult("tools/call", response.result, gatewayVersion(env));
    return mcpJson(response);
  }
  if (name === INTERACTION_TOOL_NAME && supportsOpenAIForm(rpc)) {
    const response = await handleInteractionCall(rpc, env, identity, extensionHostCall);
    if (response.result && response.result.resultType !== "input_required") {
      response.result = modernizeResult("tools/call", response.result, gatewayVersion(env));
    }
    return mcpJson(response);
  }
  if (!PUBLIC_TOOLS.some((tool) => tool.name === name)) {
    return mcpJson(rpcError(id, -32602, `unknown tool: ${name}`));
  }

  const args = rpc?.params?.arguments ?? {};
  if (!args || typeof args !== "object" || Array.isArray(args)) {
    return mcpJson(rpcError(id, -32602, "tool arguments must be an object"));
  }

  if (isFabricTool(name)) {
    if (browserMode) return mcpJson(rpcError(id, -32601, `tool unavailable for this identity: ${name}`));
    const response = await callFabricTool(id, name, args, env);
    if (response.result && isModernRequest(rpc)) {
      response.result = modernizeResult("tools/call", response.result, gatewayVersion(env));
    }
    return mcpJson(response);
  }

  if (name === "host_list") {
    if (Object.keys(args).length !== 0) {
      return mcpJson(rpcError(id, -32602, "host_list takes no arguments"));
    }
    const hosts = await readOnlineHosts(env, null, browserMode ? await browserOwnerKey(identity) : null);
    if (!hosts.ok) return mcpJson(rpcError(id, -32001, hosts.error));
    if (browserMode && !await browserHostSnapshotsCurrent(env, hosts.value)) {
      return mcpJson(rpcError(id, -32004, "host_authorization_changed"));
    }
    if (hosts.unavailable.length > 0) {
      return mcpJson(rpcError(id, -32006, "host_discovery_unavailable", {
        unavailable_hosts: hosts.unavailable,
      }));
    }
    return toolTextResponse(rpc, env, browserMode ? hosts.value.map(browserSafeHost) : hosts.value);
  }

  if (name === "host_info") {
    const hostId = hostIdFromRpc(rpc);
    if (!hostId || Object.keys(args).some((key) => key !== "host_id")) {
      return mcpJson(rpcError(id, -32602, "host_info requires only a valid host_id"));
    }
    const hosts = await readOnlineHosts(env, hostId, browserMode ? await browserOwnerKey(identity) : null);
    if (!hosts.ok) return mcpJson(rpcError(id, -32001, hosts.error));
    if (browserMode && !await browserHostSnapshotsCurrent(env, hosts.value)) {
      return mcpJson(rpcError(id, -32004, "host_authorization_changed", { host_id: hostId }));
    }
    if (hosts.unavailable.includes(hostId)) {
      return mcpJson(rpcError(id, -32006, "host_status_unavailable", { host_id: hostId }));
    }
    const host = hosts.value.find((candidate) => candidate.host_id === hostId);
    if (!host) return mcpJson(rpcError(id, -32004, "host_offline", { host_id: hostId }));
    return toolTextResponse(rpc, env, browserMode ? browserSafeHost(host) : host);
  }

  if (name === "session_list") {
    if (Object.keys(args).some((key) => key !== "host_id")) {
      return mcpJson(rpcError(id, -32602, "session_list accepts only host_id"));
    }
    const hostId = Object.hasOwn(args, "host_id") ? hostIdFromRpc(rpc) : null;
    if (Object.hasOwn(args, "host_id") && !hostId) {
      return mcpJson(rpcError(id, -32602, "invalid params.arguments.host_id"));
    }
    const sessions = await listGatewaySessions(env, hostId, browserMode ? await browserOwnerKey(identity) : null);
    if (!sessions.ok) return mcpJson(rpcError(id, sessions.code ?? -32001, sessions.error, sessions.data));
    return toolTextResponse(rpc, env, sessions.value);
  }

  if (name === "session_start") {
    const hostId = hostIdFromRpc(rpc);
    if (!hostId) {
      return mcpJson(rpcError(id, -32602, "session_start requires a valid host_id"));
    }
    const startError = validateSessionStart(args);
    if (startError) return mcpJson(rpcError(id, -32602, startError));
    if (Object.hasOwn(args, "session_id") && !validateSessionId(args.session_id)) {
      return mcpJson(rpcError(id, -32602, "invalid params.arguments.session_id"));
    }
    if (browserMode && !await activeBrowserGrant(env, hostId, await browserOwnerKey(identity))) {
      return mcpJson(rpcError(id, -32004, "host_unavailable"));
    }
    return browserMode ? proxyBrowserHost(rpc, env, hostId, identity) : proxyToHost(rpc, env, hostId);
  }

  if (name === "context_resolve" || name === "context_status") {
    const cloud = await resolveCloudContext(name, args, env);
    if (cloud.handled) {
      if (cloud.error) {
        return mcpJson(rpcError(id, cloud.code ?? -32001, cloud.error, cloud.data));
      }
      return toolTextResponse(rpc, env, cloud.value, true);
    }
  }

  const sessionId = sessionIdFromRpc(rpc);
  if (!sessionId) {
    return mcpJson(rpcError(id, -32602, "missing or invalid params.arguments.session_id"));
  }
  const explicitHostId = Object.hasOwn(args, "host_id") ? hostIdFromRpc(rpc) : null;
  if (Object.hasOwn(args, "host_id") && !explicitHostId) {
    return mcpJson(rpcError(id, -32602, "invalid params.arguments.host_id"));
  }

  if (browserMode && !explicitHostId) {
    return mcpJson(rpcError(id, -32602, "browser-authenticated requests require host_id"));
  }
  if (explicitHostId) {
    if (browserMode && !await activeBrowserGrant(env, explicitHostId, await browserOwnerKey(identity))) {
      return mcpJson(rpcError(id, -32004, "host_unavailable"));
    }
    return browserMode ? proxyBrowserHost(rpc, env, explicitHostId, identity) : proxyToHost(rpc, env, explicitHostId);
  }

  const route = await resolveUnqualifiedSession(env, sessionId);
  if (!route.ok) {
    return mcpJson(rpcError(id, route.code ?? -32001, route.error, route.data));
  }
  if (route.kind === "host") return proxyToHost(rpc, env, route.host_id);
  return proxyToLegacySession(rpc, env, sessionId);
}

function browserToolAllowed(name) {
  return [
    "host_list", "host_info", "session_list", "session_start", "session_info", "session_stop", "session_restart",
    "codex_status", "codex_task_start", "codex_task_get", "codex_task_control",
    "evidence_read", "task_list", "poll_job", "job_list", "stop_job",
  ].includes(name);
}

function browserRequestAllowed(rpc, approvedRoots, routedHostId) {
  if (rpc?.method !== "tools/call") return false;
  const name = rpc?.params?.name;
  if (!browserToolAllowed(name)) return false;
  const args = rpc?.params?.arguments;
  if (!args || typeof args !== "object" || Array.isArray(args)
      || args.host_id !== routedHostId) return false;
  if (name === "session_start") {
    if (Object.hasOwn(args, "source") || Object.keys(args).some((key) => !["host_id", "path", "session_id"].includes(key))
        || typeof args.path !== "string" || args.path.startsWith("/") || args.path.includes("\\")
        || args.path.includes("\0") || /%(?:2f|5c)/i.test(args.path)) return false;
    const parts = args.path.split("/");
    return parts.length > 0 && parts.every((part) => part && part !== "." && part !== "..")
      && approvedRoots.includes(parts[0]);
  }
  if (name === "session_list") return Object.keys(args).length === 1;
  if (["session_info", "session_stop", "session_restart", "codex_status",
    "codex_task_start", "codex_task_get", "codex_task_control", "evidence_read",
    "task_list", "poll_job", "job_list", "stop_job"].includes(name)) {
    return browserSessionToolArgumentsAllowed(name, args);
  }
  return false;
}

function browserSessionToolArgumentsAllowed(name, args) {
  const keys = {
    session_info: ["host_id", "session_id"],
    session_stop: ["host_id", "session_id"],
    session_restart: ["host_id", "session_id"],
    codex_status: ["host_id", "session_id"],
    codex_task_start: ["host_id", "session_id", "operation_id", "task", "model", "effort", "continuation"],
    codex_task_get: ["host_id", "session_id", "task_id", "after_revision", "wait_ms"],
    codex_task_control: ["host_id", "session_id", "task_id", "operation_id", "action", "input"],
    evidence_read: ["host_id", "session_id", "evidence_id", "offset_bytes", "max_bytes"],
    task_list: ["host_id", "session_id", "limit"],
    poll_job: ["host_id", "session_id", "job_id", "output_limit_bytes", "status_only"],
    job_list: ["host_id", "session_id", "limit"],
    stop_job: ["host_id", "session_id", "job_id"],
  }[name];
  if (!keys || Object.keys(args).some((key) => !keys.includes(key))
      || typeof args.session_id !== "string" || !args.session_id || args.session_id.length > 256) return false;
  const uuid = (value) => typeof value === "string"
    && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
  if (name === "codex_task_start") {
    return uuid(args.operation_id) && typeof args.task === "string" && args.task.length > 0
      && args.task.length <= 1_048_576 && typeof args.model === "string" && args.model.length > 0
      && args.model.length <= 256 && typeof args.effort === "string" && args.effort.length > 0
      && args.effort.length <= 256;
  }
  if (name === "codex_task_get") return uuid(args.task_id)
    && (!Object.hasOwn(args, "after_revision") || Number.isSafeInteger(args.after_revision) && args.after_revision >= 0)
    && (!Object.hasOwn(args, "wait_ms") || Number.isSafeInteger(args.wait_ms) && args.wait_ms >= 0 && args.wait_ms <= 30_000);
  if (name === "codex_task_control") return uuid(args.task_id) && uuid(args.operation_id)
    && ["steer", "resume", "interrupt"].includes(args.action)
    && (!Object.hasOwn(args, "input") || typeof args.input === "string" && args.input.length <= 1_048_576);
  if (name === "evidence_read") return uuid(args.evidence_id)
    && (!Object.hasOwn(args, "offset_bytes") || Number.isSafeInteger(args.offset_bytes) && args.offset_bytes >= 0)
    && (!Object.hasOwn(args, "max_bytes") || Number.isSafeInteger(args.max_bytes) && args.max_bytes >= 1 && args.max_bytes <= 65_536);
  if (["task_list", "job_list"].includes(name)) return !Object.hasOwn(args, "limit")
    || Number.isSafeInteger(args.limit) && args.limit >= 1 && args.limit <= 128;
  if (["poll_job", "stop_job"].includes(name)) return typeof args.job_id === "string"
    && args.job_id.length > 0 && args.job_id.length <= 256
    && (name === "stop_job" || (!Object.hasOwn(args, "status_only") || typeof args.status_only === "boolean")
      && (!Object.hasOwn(args, "output_limit_bytes") || Number.isSafeInteger(args.output_limit_bytes)
        && args.output_limit_bytes >= 256 && args.output_limit_bytes <= 1_048_576));
  return ["session_info", "session_stop", "session_restart", "codex_status"].includes(name);
}

async function browserOwnerKey(identity) {
  const bytes = new Uint8Array(await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`${identity.issuer}\0${identity.subject}`),
  ));
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function browserSafeHost(host) {
  const safe = {};
  for (const field of ["host_id", "availability", "session_availability", "lease", "approved_root_names"]) {
    const value = host?.[field];
    if (field === "approved_root_names" && Array.isArray(value)) {
      safe[field] = value.filter((root) => typeof root === "string" && root.length <= 64);
    } else if (typeof value === "string" && value.length <= 128) {
      safe[field] = value;
    }
  }
  return safe;
}

async function proxyBrowserHost(rpc, env, hostId, identity) {
  const ownerKey = await browserOwnerKey(identity);
  const before = await activeBrowserGrant(env, hostId, ownerKey);
  if (!before) return mcpJson(rpcError(rpc.id ?? null, -32004, "host_unavailable"));
  if (!browserRequestAllowed(rpc, before.approved_roots, hostId)) {
    return mcpJson(rpcError(rpc.id ?? null, -32601, "operation outside the approved Host scope"));
  }
  let sessionBinding = null;
  const toolName = rpc?.params?.name;
  const arguments_ = rpc?.params?.arguments ?? {};
  if (!new Set(["session_list", "session_start"]).has(toolName)) {
    const hosts = await readOnlineHosts(env, hostId, ownerKey);
    if (!hosts.ok) return mcpJson(rpcError(rpc.id ?? null, -32001, hosts.error));
    if (hosts.unavailable.includes(hostId)) {
      return mcpJson(rpcError(rpc.id ?? null, -32006, "host_status_unavailable", { host_id: hostId }));
    }
    const host = hosts.value.find((candidate) => candidate.host_id === hostId);
    if (!host) return mcpJson(rpcError(rpc.id ?? null, -32004, "host_offline", { host_id: hostId }));
    const inventory = await sessionsFromHost(env, host, before.approved_roots, { includeBinding: true });
    if (!inventory.ok) return mcpJson(rpcError(rpc.id ?? null, -32006, "host_session_unavailable", { host_id: hostId }));
    const sessionId = arguments_.session_id;
    const session = inventory.sessions.find((candidate) => candidate.session_id === sessionId);
    if (!session || typeof session.session_instance !== "string") {
      return mcpJson(rpcError(rpc.id ?? null, -32004, "session_instance_unavailable", { host_id: hostId }));
    }
    sessionBinding = { session_id: sessionId, session_instance: session.session_instance };
  }
  const response = await proxyToHost(rpc, env, hostId, ownerKey, {
    mode: "browser",
    owner_key: before.owner_key,
    grant_id: before.grant_id,
    grant_generation: before.grant_generation,
    approved_roots: before.approved_roots,
  }, sessionBinding);
  const after = await activeBrowserGrant(env, hostId, ownerKey);
  if (!after || after.grant_id !== before.grant_id
      || after.grant_generation !== before.grant_generation
      || JSON.stringify(after.approved_roots) !== JSON.stringify(before.approved_roots)) {
    return mcpJson(rpcError(rpc.id ?? null, -32004, "host_unavailable"));
  }
  return response;
}

async function browserHostSnapshotsCurrent(env, hosts) {
  for (const host of hosts) {
    const current = await activeBrowserGrant(env, host.host_id, host.fabric_auth?.owner_key);
    if (!sameBrowserGrantSnapshot(current, host.fabric_auth)) return false;
  }
  return true;
}

function toolTextResponse(rpc, env, value, compact = false) {
  // Cloud context budgets apply to the serialized context text. Preserve that
  // exact serialization rather than expanding it through pretty printing.
  const result = textResult(compact ? JSON.stringify(value) : JSON.stringify(value, null, 2));
  return mcpJson(rpcResult(
    rpc.id ?? null,
    isModernRequest(rpc)
      ? modernizeResult("tools/call", result, gatewayVersion(env))
      : result,
  ));
}
