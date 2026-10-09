import {
  gatewayVersion,
  isModernRequest,
  modernizeResult,
  rpcError,
  validateHostId,
  validateSessionId,
} from "./protocol.js";
import { authorizeLegacyHost, validateLegacyHostInventory } from "./access.js";
import { activeBrowserGrant, allBrowserHostIds, authorizeBrowserHost, authorizeLegacyFederatedHost, browserHostIdsForOwner, sameBrowserGrantSnapshot } from "./enrollment.js";
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
  jsonResponse,
  mcpJson,
  readJson,
  safeBoundedJson,
  unauthorizedHost,
  withCors,
} from "./http.js";
import { observeHostResponse } from "./events/transitions.js";

const HOST_LEASE_MS = 90_000;
const HOST_AGENT_PROTOCOL_VERSION = 1;
const HOST_CONTROL_PROTOCOL_VERSION = 2;
const POLL_TIMEOUT_MS = 20_000;
const RPC_TIMEOUT_MS = 35_000;
const MAX_PENDING_HOST_REQUESTS = 64;
const MAX_CONCURRENT_AGENT_REQUESTS = 8;
const MAX_REQUEST_ID_ATTEMPTS = 8;
const MAX_REGISTRY_SESSIONS = 1024;
const MAX_REGISTRY_HOSTS = 256;
const MAX_SESSION_STATUS_CHECK_CONCURRENCY = 16;
const MAX_BODY_BYTES = 8 * 1024 * 1024;
const MAX_INTERNAL_DISPATCH_ENVELOPE_BYTES = 64 * 1024;
const MAX_INTERNAL_DISPATCH_BODY_BYTES = MAX_BODY_BYTES + MAX_INTERNAL_DISPATCH_ENVELOPE_BYTES;
const MAX_HOST_RESPONSE_BODY_BYTES = 52 * 1024 * 1024;
const MAX_INTERNAL_RPC_RESPONSE_BYTES = MAX_HOST_RESPONSE_BODY_BYTES;
const MAX_INTERNAL_ERROR_RESPONSE_BYTES = 64 * 1024;
const MAX_REGISTRY_RESPONSE_BYTES = 1024 * 1024;
const MAX_RPC_METHOD_BYTES = 256;
const MAX_RPC_ID_BYTES = 256;
const MAX_RPC_TOOL_NAME_BYTES = 256;
const SESSION_AVAILABILITY_VALUES = ["ready", "session_unavailable", "unavailable"];
const BROWSER_ROUTED_TOOLS = new Set([
  "session_list", "session_start", "session_info", "session_stop", "session_restart",
  "codex_status", "codex_task_start", "codex_task_get", "codex_task_control",
  "evidence_read", "task_list", "poll_job", "job_list", "stop_job",
]);

function browserDispatchAllowed(request, approvedRoots) {
  if (request?.method !== "tools/call" || !BROWSER_ROUTED_TOOLS.has(request?.params?.name)) return false;
  const args = request?.params?.arguments;
  if (!args || typeof args !== "object" || Array.isArray(args)) return false;
  if (request.params.name === "session_start") {
    if (typeof args.path !== "string" || Object.hasOwn(args, "source")
        || Object.keys(args).some((key) => !["path", "session_id"].includes(key))
        || args.path.startsWith("/") || args.path.includes("\\") || args.path.includes("\0")
        || /%(?:2f|5c)/i.test(args.path)) return false;
    const parts = args.path.split("/");
    if (parts.some((part) => !part || part === "." || part === "..")) return false;
    return approvedRoots.includes(parts[0]);
  }
  if (request.params.name === "session_list") return Object.keys(args).length === 0;
  const allowedKeys = {
    session_info: ["session_id"],
    session_stop: ["session_id"],
    session_restart: ["session_id"],
    codex_status: ["session_id"],
    codex_task_start: ["session_id", "operation_id", "task", "model", "effort", "continuation"],
    codex_task_get: ["session_id", "task_id", "after_revision", "wait_ms"],
    codex_task_control: ["session_id", "task_id", "operation_id", "action", "input"],
    evidence_read: ["session_id", "evidence_id", "offset_bytes", "max_bytes"],
    task_list: ["session_id", "limit"],
    poll_job: ["session_id", "job_id", "output_limit_bytes", "status_only"],
    job_list: ["session_id", "limit"],
    stop_job: ["session_id", "job_id"],
  }[request.params.name];
  if (!allowedKeys || Object.keys(args).some((key) => !allowedKeys.includes(key))
      || typeof args.session_id !== "string" || !args.session_id || args.session_id.length > 256) return false;
  const uuid = (value) => typeof value === "string"
    && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
  switch (request.params.name) {
    case "codex_task_start":
      return uuid(args.operation_id) && typeof args.task === "string" && args.task.length > 0
        && args.task.length <= 1_048_576 && typeof args.model === "string" && args.model.length > 0
        && args.model.length <= 256 && typeof args.effort === "string" && args.effort.length > 0
        && args.effort.length <= 256;
    case "codex_task_get":
      return uuid(args.task_id)
        && (!Object.hasOwn(args, "after_revision") || Number.isSafeInteger(args.after_revision) && args.after_revision >= 0)
        && (!Object.hasOwn(args, "wait_ms") || Number.isSafeInteger(args.wait_ms) && args.wait_ms >= 0 && args.wait_ms <= 30_000);
    case "codex_task_control":
      return uuid(args.task_id) && uuid(args.operation_id)
        && ["steer", "resume", "interrupt"].includes(args.action)
        && (!Object.hasOwn(args, "input") || typeof args.input === "string" && args.input.length <= 1_048_576);
    case "evidence_read":
      return uuid(args.evidence_id)
        && (!Object.hasOwn(args, "offset_bytes") || Number.isSafeInteger(args.offset_bytes) && args.offset_bytes >= 0)
        && (!Object.hasOwn(args, "max_bytes") || Number.isSafeInteger(args.max_bytes) && args.max_bytes >= 1 && args.max_bytes <= 65_536);
    case "task_list":
    case "job_list":
      return !Object.hasOwn(args, "limit") || Number.isSafeInteger(args.limit) && args.limit >= 1 && args.limit <= 128;
    case "poll_job":
      return typeof args.job_id === "string" && args.job_id.length > 0 && args.job_id.length <= 256
        && (!Object.hasOwn(args, "status_only") || typeof args.status_only === "boolean")
        && (!Object.hasOwn(args, "output_limit_bytes") || Number.isSafeInteger(args.output_limit_bytes)
          && args.output_limit_bytes >= 256 && args.output_limit_bytes <= 1_048_576);
    case "stop_job":
      return typeof args.job_id === "string" && args.job_id.length > 0 && args.job_id.length <= 256;
    default:
      return ["session_info", "session_stop", "session_restart", "codex_status"].includes(request.params.name);
  }
}

function browserSessionBinding(value, request) {
  const needsBinding = !["session_list", "session_start"].includes(request?.params?.name);
  if (!needsBinding) return value === undefined ? { ok: true, value: null } : { ok: false };
  const args = request?.params?.arguments;
  if (!value || typeof value !== "object" || Array.isArray(value)
      || Object.keys(value).sort().join(",") !== "session_id,session_instance"
      || value.session_id !== args?.session_id
      || typeof value.session_id !== "string" || value.session_id.length < 1 || value.session_id.length > 256
      || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value.session_instance ?? "")) {
    return { ok: false };
  }
  return { ok: true, value: { session_id: value.session_id, session_instance: value.session_instance } };
}

async function proxyToHost(rpc, env, hostId, ownerKey = null, expectedAuth = null, browserSession = null) {
  if (ownerKey === null && expectedAuth === null) {
    const reserved = await allBrowserHostIds(env);
    if (!reserved) return mcpJson(rpcError(rpc.id ?? null, -32001, "host grant inventory unavailable"));
    if (reserved.has(hostId)) return mcpJson(rpcError(rpc.id ?? null, -32004, "host_offline", { host_id: hostId }));
    const routed = withoutHostRoutingArgument(rpc);
    return proxyDispatch(rpc, routed, env, hostStub(env, hostId), { host_id: hostId });
  }
  const hosts = await readOnlineHosts(env, hostId, ownerKey);
  if (!hosts.ok) return mcpJson(rpcError(rpc.id ?? null, -32001, hosts.error));
  if (hosts.unavailable.includes(hostId)) {
    return mcpJson(rpcError(rpc.id ?? null, -32006, "host_status_unavailable", { host_id: hostId }));
  }
  if (!hosts.value.some((host) => host.host_id === hostId)) {
    return mcpJson(rpcError(rpc.id ?? null, -32004, "host_offline", { host_id: hostId }));
  }
  const routed = withoutHostRoutingArgument(rpc);
  const auth = ownerKey === null ? null : hosts.value.find((host) => host.host_id === hostId)?.fabric_auth ?? null;
  if (expectedAuth && !sameBrowserGrantSnapshot({
    owner_key: auth?.owner_key,
    grant_id: auth?.grant_id,
    grant_generation: auth?.grant_generation,
    approved_roots: auth?.approved_roots,
  }, expectedAuth)) {
    return mcpJson(rpcError(rpc.id ?? null, -32004, "host_authorization_changed", { host_id: hostId }));
  }
  return proxyDispatch(rpc, routed, env, hostStub(env, hostId), { host_id: hostId }, auth, browserSession);
}

async function proxyToLegacySession(rpc, env, sessionId) {
  return proxyDispatch(rpc, rpc, env, sessionStub(env, sessionId), {
    session_id: sessionId,
    routing_mode: "legacy-session",
  });
}

async function proxyDispatch(originalRpc, routedRpc, env, stub, routeData, fabricAuth = null, browserSession = null) {
  const id = originalRpc.id ?? null;
  let response;
  try {
    response = await stub.fetch("https://route.internal/dispatch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        request: routedRpc,
        ...(fabricAuth ? { _fabric_auth: fabricAuth } : {}),
        ...(browserSession ? { _fabric_session: browserSession } : {}),
      }),
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
    if (routeData.host_id && !fabricAuth && payload.result) {
      try { await observeHostResponse(env, routeData.host_id, originalRpc, payload); }
      catch { /* Event persistence failure cannot change an accepted Host tool response. */ }
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

async function readOnlineHosts(env, hostId = null, ownerKey = null) {
  const hosts = await readRegistryCollection(env, "hosts", MAX_REGISTRY_HOSTS);
  if (!hosts.ok) return hosts;
  // A qualified lookup must not wait for, or probe, unrelated host leases.
  // Aggregate discovery still checks every host and fails closed above it.
  let candidates = hostId === null
    ? hosts.value
    : hosts.value.filter((host) => host?.host_id === hostId);
  if (ownerKey !== null) {
    const inventory = validateLegacyHostInventory(env);
    if (!inventory.ok) return { ok: false, error: "legacy host inventory unavailable" };
    const allowed = await browserHostIdsForOwner(env, ownerKey);
    if (!allowed) return { ok: false, error: "host grant inventory unavailable" };
    candidates = candidates.filter((host) => allowed.has(host?.host_id) && !inventory.ids.has(host?.host_id));
  }
  if (ownerKey === null) {
    const reserved = await allBrowserHostIds(env);
    if (!reserved) return { ok: false, error: "host grant inventory unavailable" };
    candidates = candidates.filter((host) => !reserved.has(host?.host_id) && host?.auth_mode !== "browser");
  }
  const filtered = await filterOnlineRegistryHosts(candidates, env);
  if (ownerKey !== null) {
    const authorized = [];
    for (const host of filtered.online) {
      const grant = await activeBrowserGrant(env, host.host_id, ownerKey);
      if (!grant) continue;
      host.approved_root_names = grant.approved_roots;
      host.fabric_auth = {
        mode: "browser",
        owner_key: grant.owner_key,
        grant_id: grant.grant_id,
        grant_generation: grant.grant_generation,
        approved_roots: grant.approved_roots,
      };
      authorized.push(host);
    }
    return { ok: true, value: authorized, unavailable: filtered.unavailable };
  }
  return { ok: true, value: filtered.online, unavailable: filtered.unavailable };
}

async function readOnlineLegacySessions(env) {
  const sessions = await readRegistryCollection(env, "list", MAX_REGISTRY_SESSIONS);
  if (!sessions.ok) return sessions;
  const filtered = await filterOnlineRegistrySessions(sessions.value, env);
  return { ok: true, value: filtered.online, unavailable: filtered.unavailable };
}

function parseSessionListPayload(payload, hostId, approvedRoots = null, { includeBinding = false } = {}) {
  const text = payload?.result?.content?.find((entry) => entry?.type === "text")?.text;
  if (typeof text !== "string") throw new Error("host session_list returned no text result");
  const sessions = JSON.parse(text);
  if (!Array.isArray(sessions) || sessions.length > MAX_REGISTRY_SESSIONS) {
    throw new Error("host session_list returned invalid session collection");
  }
  if (approvedRoots !== null) {
    return sessions.filter((session) => approvedRoots.includes(session?.root_name))
      .map((session) => browserSafeSession(session, hostId, includeBinding));
  }
  return sessions.map((session) => ({ ...session, host_id: hostId, routing_mode: "host" }));
}

function browserSafeSession(session, hostId, includeBinding) {
  const safe = { host_id: hostId, routing_mode: "host" };
  const fields = ["session_id", "root_name", "logical_path", "status", "permission_mode", "backend"];
  if (includeBinding) fields.push("session_instance");
  for (const field of fields) {
    const value = session?.[field];
    const safePath = field !== "logical_path"
      || (typeof value === "string" && value.split("/").every((part) => part !== ".." && part !== "."));
    const safeInstance = field !== "session_instance"
      || /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value ?? "");
    if (typeof value === "string" && value.length <= 256 && safePath && safeInstance && !value.includes("\\") && !value.startsWith("/")) {
      safe[field] = value;
    }
  }
  return safe;
}

export async function sessionsFromHost(env, host, approvedRoots = null, { includeBinding = false } = {}) {
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
      body: JSON.stringify({ request: rpc, ...(host.fabric_auth ? { _fabric_auth: host.fabric_auth } : {}) }),
    });
  } catch {
    return { ok: false, host_id: host.host_id };
  }
  if (!response.ok) return { ok: false, host_id: host.host_id };
  const payload = await safeBoundedJson(response, MAX_INTERNAL_RPC_RESPONSE_BYTES, "host session_list response");
  if (!payload || payload.error) return { ok: false, host_id: host.host_id };
  try {
    return {
      ok: true,
      host_id: host.host_id,
      sessions: parseSessionListPayload(payload, host.host_id, approvedRoots, { includeBinding }),
    };
  } catch {
    return { ok: false, host_id: host.host_id };
  }
}

async function collectHostSessions(env, hosts) {
  const results = [];
  const limit = MAX_SESSION_STATUS_CHECK_CONCURRENCY;
  for (let offset = 0; offset < hosts.length; offset += limit) {
    const batch = await Promise.all(hosts.slice(offset, offset + limit).map((host) => sessionsFromHost(
      env, host, Object.hasOwn(host, "approved_root_names") ? host.approved_root_names : null,
    )));
    results.push(...batch);
  }
  return results;
}

async function listGatewaySessions(env, hostId, ownerKey = null) {
  const hosts = await readOnlineHosts(env, hostId ?? null, ownerKey);
  if (!hosts.ok) return hosts;
  if (hostId) {
    if (hosts.unavailable.includes(hostId)) {
      return { ok: false, code: -32006, error: "host_status_unavailable", data: { host_id: hostId } };
    }
    const host = hosts.value.find((candidate) => candidate.host_id === hostId);
    if (!host) return { ok: false, code: -32004, error: "host_offline", data: { host_id: hostId } };
    const result = await sessionsFromHost(env, host, ownerKey === null ? null : host.approved_root_names ?? []);
    if (!result.ok) return { ok: false, error: "host session discovery failed", data: { host_id: hostId } };
    if (ownerKey !== null && !sameBrowserGrantSnapshot(
      await activeBrowserGrant(env, hostId, ownerKey), host.fabric_auth,
    )) {
      return { ok: false, error: "host session discovery failed", data: { host_id: hostId } };
    }
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
  const legacy = ownerKey === null ? await readOnlineLegacySessions(env) : { ok: true, value: [], unavailable: [] };
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
  if (ownerKey !== null) {
    for (const host of hosts.value) {
      if (!sameBrowserGrantSnapshot(await activeBrowserGrant(env, host.host_id, ownerKey), host.fabric_auth)) {
        return { ok: false, code: -32004, error: "host authorization changed during session discovery" };
      }
    }
    return { ok: true, value: federated.sort(compareSessionRoute) };
  }
  return { ok: true, value: [...legacySessions, ...federated].sort(compareSessionRoute) };
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
  let authContext = { mode: "legacy" };
  if (federated) {
    if (!validateHostId(headerHostId)) return unauthorizedHost();
    const browserGrantHeader = request.headers.get("x-temote-fabric-host-grant");
    if (browserGrantHeader !== null) {
      authContext = await authorizeBrowserHost(request, env, headerHostId);
      if (!authContext) return unauthorizedHost();
    } else if (!await authorizeLegacyFederatedHost(request, env, headerHostId)) {
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

  const trustedBody = federated
    ? { ...body.value, _fabric_auth: authContext }
    : body.value;
  const response = await stub.fetch(`https://route.internal/${action}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(trustedBody),
  });
  if (authContext.mode === "browser") {
    // D1 is the revoke/expiry commit point. A long poll or RPC that crossed a
    // revoke boundary must not return a stale response to the caller.
    const current = await authorizeBrowserHost(request, env, headerHostId);
    if (!current || current.owner_key !== authContext.owner_key
        || current.grant_id !== authContext.grant_id
        || current.grant_generation !== authContext.grant_generation) return unauthorizedHost();
  }
  return withCors(response);
}

function validateTransportContext(value) {
  if (value === undefined) return { mode: "legacy" };
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  if (value.mode === "legacy" && Object.keys(value).length === 1) return { mode: "legacy" };
  if (value.mode !== "browser" || !/^[0-9a-f]{64}$/.test(value.owner_key)
      || !UUID_PATTERN.test(value.grant_id) || !Number.isSafeInteger(value.grant_generation)
      || value.grant_generation < 1 || !validRootNameList(value.approved_roots)) return null;
  return {
    mode: "browser",
    owner_key: value.owner_key,
    grant_id: value.grant_id,
    grant_generation: value.grant_generation,
    approved_roots: [...value.approved_roots].sort(),
  };
}

function verifyTransportContext(host, supplied) {
  if (!host) return jsonResponse({ error: "host_offline" }, 409);
  const current = validateTransportContext(host.auth_mode === undefined ? undefined : {
    mode: host.auth_mode,
    ...(host.auth_mode === "browser" ? {
      owner_key: host.owner_key,
      grant_id: host.grant_id,
      grant_generation: host.grant_generation,
      approved_roots: host.approved_roots,
    } : {}),
  });
  const incoming = validateTransportContext(supplied);
  if (!current || !incoming || current.mode !== incoming.mode) return jsonResponse({ error: "stale_auth_generation" }, 409);
  if (current.mode === "browser" && (current.owner_key !== incoming.owner_key
      || current.grant_id !== incoming.grant_id
      || current.grant_generation !== incoming.grant_generation
      || !sameStringSet(current.approved_roots, incoming.approved_roots))) {
    return jsonResponse({ error: "stale_auth_generation" }, 409);
  }
  return null;
}

function sameStringSet(left, right) {
  return validRootNameList(left) && validRootNameList(right)
    && left.length === right.length && [...left].sort().every((value, index) => value === [...right].sort()[index]);
}

function validRootNameList(value) {
  return Array.isArray(value) && value.length > 0 && value.length <= 32
    && value.every((root) => typeof root === "string" && /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(root) && root !== "." && root !== "..")
    && new Set(value).size === value.length;
}

const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

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
    if (action === "events/identity") return this.eventsIdentity();
    const body = await readJson(request, gatewaySessionBodyLimit(action));
    if (!body.ok) return jsonResponse({ error: "invalid_json", detail: body.error }, 400);
    switch (action) {
      case "fence":
        return this.fence(body.value);
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
      return jsonResponse({ status: "lease_expired" }, 404);
    }
    return jsonResponse({
      status: "registered",
      host_id: host.host_id,
      generation: host.generation,
      lease: "active",
      session_availability: SESSION_AVAILABILITY_VALUES.includes(host.session_availability)
        ? host.session_availability
        : "not_checked",
    });
  }

  async eventsIdentity() {
    const status = await this.status();
    if (!status.ok) return status;
    const snapshot = await status.json();
    const host = await this.currentHost();
    if (!host || host.generation !== snapshot.generation) {
      return jsonResponse({ status: "not_registered" }, 404);
    }
    return jsonResponse({ ...snapshot, instance_id: host.instance_id });
  }

  async connect(body) {
    const validation = validateAgentIdentity(body, false);
    if (validation) return validation;
    const route = agentRoute(body);
    if (route.host_id) {
      const metadataValidation = validateFederatedHostMetadata(body);
      if (metadataValidation) return metadataValidation;
    }
    const authContext = route.host_id ? validateTransportContext(body._fabric_auth) : { mode: "legacy" };
    if (!authContext) return jsonResponse({ error: "host_auth_context_invalid" }, 403);
    if (authContext.mode === "browser" && !sameStringSet(body.named_roots, authContext.approved_roots)) {
      return jsonResponse({ error: "approved_roots_mismatch" }, 403);
    }
    const existingHost = await this.currentHost();
    let browserMarker = await this.state.storage.get("browser_authority");
    if (browserMarker !== undefined && browserMarker !== null && !validBrowserAuthorityMarker(browserMarker)) {
      return jsonResponse({ error: "browser_authority_marker_unavailable" }, 503);
    }
    if (!browserMarker && existingHost?.auth_mode === "browser") {
      if (!/^[0-9a-f]{64}$/.test(existingHost.owner_key ?? "")) {
        return jsonResponse({ error: "browser_authority_marker_unavailable" }, 503);
      }
      browserMarker = { version: 1, owner_key: existingHost.owner_key };
      await this.state.storage.put("browser_authority", browserMarker);
    }
    if (browserMarker) {
      if (authContext.mode !== "browser" || authContext.owner_key !== browserMarker.owner_key) {
        return jsonResponse({ error: "browser_host_cannot_downgrade", migration_required: true }, 409);
      }
    } else if (authContext.mode === "browser") {
      browserMarker = { version: 1, owner_key: authContext.owner_key };
      // Persist the anti-downgrade marker before publishing any live lease.
      // A later connect/registry failure must not turn this Host id back into
      // a static-token identity.
      await this.state.storage.put("browser_authority", browserMarker);
    }
    if (existingHost && Number.isSafeInteger(existingHost.expires_at) && existingHost.expires_at > Date.now()) {
      const sameBrowserOwner = existingHost.auth_mode === "browser"
        && authContext.mode === "browser"
        && existingHost.owner_key === authContext.owner_key;
      if (sameBrowserOwner) {
        // Same owner may reconnect after an explicit grant generation/root
        // update. The D1 grant check on every routed request remains the
        // authorization commit point.
      } else {
      const authMismatch = verifyTransportContext(existingHost, authContext);
      if (authMismatch) return jsonResponse({ error: "active_host_auth_context_conflict" }, 409);
      }
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
        auth_mode: authContext.mode,
        ...(authContext.mode === "browser" ? {
          owner_key: authContext.owner_key,
          grant_id: authContext.grant_id,
          grant_generation: authContext.grant_generation,
          approved_roots: [...authContext.approved_roots],
        } : {}),
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
    const replaced = verifyGeneration(await this.currentHost(), host);
    if (replaced) return replaced;
    return jsonResponse({
      ...route,
      generation,
      lease_seconds: Math.floor(HOST_LEASE_MS / 1000),
      concurrent_requests: MAX_CONCURRENT_AGENT_REQUESTS,
      ...(route.host_id ? { agent_protocol: HOST_AGENT_PROTOCOL_VERSION } : {}),
    });
  }

  async fence(body) {
    if (!validateHostId(body?.host_id) || !Number.isSafeInteger(body?.generation) || body.generation < 1) {
      return jsonResponse({ error: "invalid_fence" }, 400);
    }
    const host = await this.currentHost();
    if (!host || host.host_id !== body.host_id || host.auth_mode !== "browser"
        || !Number.isSafeInteger(host.grant_generation) || host.grant_generation >= body.generation) {
      return new Response(null, { status: 204 });
    }
    await this.clearHost(host, "host_grant_fenced");
    return new Response(null, { status: 204 });
  }

  async poll(body) {
    const validation = validateAgentIdentity(body, true);
    if (validation) return validation;
    const host = await this.currentHost();
    const authMismatch = verifyTransportContext(host, body._fabric_auth);
    if (authMismatch) return authMismatch;
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;

    const availability = normalizeSessionAvailability(body.session_availability);
    if (availability === null || (availability !== undefined && !host.host_id)) {
      return jsonResponse({ error: "invalid_session_availability" }, 400);
    }
    if (Object.hasOwn(body, "accept_requests") && typeof body.accept_requests !== "boolean") {
      return jsonResponse({ error: "invalid_accept_requests" }, 400);
    }

    const now = Date.now();
    host.last_seen = now;
    host.expires_at = now + HOST_LEASE_MS;
    if (availability !== undefined) host.session_availability = availability;
    await this.state.storage.put("host", host);
    const registry = await this.upsertRegistry(host);
    if (!registry.ok) {
      await this.clearHost(host, "registry_renewal_failed");
      return jsonResponse({
        error: registry.error,
        ...(registry.status ? { gateway_status: registry.status } : {}),
      }, registry.status ?? 503);
    }
    const replaced = verifyGeneration(await this.currentHost(), host);
    if (replaced) return replaced;
    // A saturated agent can renew its lease without dequeuing another request.
    // Existing agents omit this flag and keep the original long-poll behavior.
    if (body.accept_requests === false) return new Response(null, { status: 204 });

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
    const authMismatch = verifyTransportContext(host, body._fabric_auth);
    if (authMismatch) return authMismatch;
    const mismatch = verifyGeneration(host, body);
    if (mismatch) return mismatch;

    const pending = this.pending.get(body.request_id);
    if (!pending) return jsonResponse({ error: "stale_request" }, 409);
    if (JSON.stringify(body._fabric_session ?? null) !== JSON.stringify(pending.browser_session ?? null)) {
      return jsonResponse({ error: "stale_session_binding" }, 409);
    }
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
    const authMismatch = verifyTransportContext(host, body._fabric_auth);
    if (authMismatch) return authMismatch;
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
    const marker = await this.state.storage.get("browser_authority");
    if (marker !== undefined && marker !== null && !validBrowserAuthorityMarker(marker)) {
      return jsonResponse({ error: "browser_authority_marker_unavailable" }, 503);
    }
    const incomingAuth = validateTransportContext(body?._fabric_auth);
    if (!incomingAuth) return jsonResponse({ error: "host_auth_context_invalid" }, 403);
    if (marker && (incomingAuth.mode !== "browser" || incomingAuth.owner_key !== marker.owner_key)) {
      return jsonResponse({ error: "browser_host_cannot_downgrade", migration_required: true }, 403);
    }
    const host = await this.currentHost();
    if (!host) return jsonResponse({ error: "host_offline" }, 503);
    if (host.expires_at <= Date.now()) {
      await this.clearHost(host, "host_lease_expired");
      return jsonResponse({ error: "host_offline", detail: "lease expired" }, 503);
    }
    const authMismatch = verifyTransportContext(host, incomingAuth);
    if (authMismatch) return authMismatch;
    let browserSession = null;
    if (incomingAuth.mode === "browser") {
      const committed = await activeBrowserGrant(this.env, host.host_id, incomingAuth.owner_key);
      if (!sameBrowserGrantSnapshot(committed, {
        mode: "browser",
        owner_key: incomingAuth.owner_key,
        grant_id: incomingAuth.grant_id,
        grant_generation: incomingAuth.grant_generation,
        approved_roots: incomingAuth.approved_roots,
      })) return jsonResponse({ error: "stale_auth_generation" }, 409);
      if (!browserDispatchAllowed(request, incomingAuth.approved_roots)) {
        return jsonResponse({ error: "browser_operation_not_allowed" }, 403);
      }
      const binding = browserSessionBinding(body?._fabric_session, request);
      if (!binding.ok) return jsonResponse({ error: "browser_session_binding_invalid" }, 403);
      browserSession = binding.value;
    } else if (body?._fabric_session !== undefined) {
      return jsonResponse({ error: "unexpected_session_binding" }, 403);
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
      this.pending.set(requestId, {
        resolve, timer, generation: host.generation, rpc_id: request.id,
        browser_session: browserSession,
      });
    });
    this.queue.push({
      request_id: requestId,
      request,
      generation: host.generation,
      ...(incomingAuth.mode === "browser" ? { _fabric_auth: incomingAuth } : {}),
      ...(browserSession ? { _fabric_session: browserSession } : {}),
    });
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
        return {
          request_id: entry.request_id,
          request: entry.request,
          ...(entry._fabric_auth ? { _fabric_auth: entry._fabric_auth } : {}),
          ...(entry._fabric_session ? { _fabric_session: entry._fabric_session } : {}),
        };
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
    // Registry I/O can yield to a newer connect. A failed renewal from an old
    // generation must not clear the replacement host or its pending requests.
    if (verifyGeneration(await this.currentHost(), host)) return;
    if (host.auth_mode === "browser") {
      const marker = await this.state.storage.get("browser_authority");
      if (marker === undefined || marker === null) {
        if (!/^[0-9a-f]{64}$/.test(host.owner_key ?? "")) {
          return jsonResponse({ error: "browser_authority_marker_unavailable" }, 503);
        }
        await this.state.storage.put("browser_authority", { version: 1, owner_key: host.owner_key });
      } else if (!validBrowserAuthorityMarker(marker) || marker.owner_key !== host.owner_key) {
        return jsonResponse({ error: "browser_authority_marker_unavailable" }, 503);
      }
    }
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

function validBrowserAuthorityMarker(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && value.version === 1 && /^[0-9a-f]{64}$/.test(value.owner_key ?? "");
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

// Normalizes an optional host-reported session availability value.
// `undefined` means the field was absent (old agent); `null` means invalid.
export function normalizeSessionAvailability(value) {
  if (value === undefined) return undefined;
  if (typeof value !== "string" || !SESSION_AVAILABILITY_VALUES.includes(value)) return null;
  return value;
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

export {
  handleHostApi,
  listGatewaySessions,
  proxyToHost,
  proxyToLegacySession,
  readOnlineHosts,
  resolveUnqualifiedSession,
};
