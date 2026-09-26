import {
  gatewayVersion,
  isModernRequest,
  modernizeResult,
  rpcError,
  validateHostId,
  validateSessionId,
} from "./protocol.js";
import { authorizeFederatedHost, authorizeLegacyHost } from "./access.js";
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
const MAX_HOST_RESPONSE_BODY_BYTES = 52 * 1024 * 1024;
const MAX_INTERNAL_RPC_RESPONSE_BYTES = MAX_HOST_RESPONSE_BODY_BYTES;
const MAX_INTERNAL_ERROR_RESPONSE_BYTES = 64 * 1024;
const MAX_REGISTRY_RESPONSE_BYTES = 1024 * 1024;
const MAX_RPC_METHOD_BYTES = 256;
const MAX_RPC_ID_BYTES = 256;
const MAX_RPC_TOOL_NAME_BYTES = 256;
const SESSION_AVAILABILITY_VALUES = ["ready", "session_unavailable", "unavailable"];

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
      session_availability: SESSION_AVAILABILITY_VALUES.includes(host.session_availability)
        ? host.session_availability
        : "not_checked",
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

    const availability = normalizeSessionAvailability(body.session_availability);
    if (availability === null || (availability !== undefined && !host.host_id)) {
      return jsonResponse({ error: "invalid_session_availability" }, 400);
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
