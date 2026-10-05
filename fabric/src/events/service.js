import { hostStub } from "../routing.js";
import { rpcError, rpcResult, validateHostId, validateSessionId } from "../protocol.js";
import { safeBoundedJson } from "../http.js";
import { EVENT_CATALOG, validArguments } from "./catalog.js";
import { callbackUrl, canonicalJson, grantedTtl, subscriptionId, validSecret } from "./validation.js";
import { durableReady, putSubscription, removeSubscription } from "./repository.js";

const SESSION_RESPONSE_LIMIT = 128 * 1024;
const CALLBACK_ERROR = -32015;

async function tokenDigest(value) {
  const bytes = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export async function principalKey(identity, env) {
  if (identity.subject === "client-token") {
    if (typeof env.CLIENT_TOKEN !== "string" || !env.CLIENT_TOKEN) return null;
    return JSON.stringify(["client-token", await tokenDigest(env.CLIENT_TOKEN)]);
  }
  // A one-time Access JWT cannot be revalidated after it expires without a
  // separate durable revocation authority. Do not turn it into a standing
  // delivery grant based on an email allowlist alone.
  return null;
}

export function eventsEligible(identity) {
  return identity?.subject === "client-token";
}

export async function principalStillAllowed(principal, env) {
  let kind, subject;
  try { [kind, subject] = JSON.parse(principal); } catch { return false; }
  if (kind === "client-token") {
    return typeof env.CLIENT_TOKEN === "string" && env.CLIENT_TOKEN.length > 0
      && subject === await tokenDigest(env.CLIENT_TOKEN);
  }
  return false;
}

const SESSION_STATES = new Set(["starting", "active", "stopping", "stopped", "crashed", "failed"]);

export function sessionIdentity(view, hostId, sessionId) {
  if (!view || (view.session_id ?? view.id) !== sessionId || view.host_id !== hostId
    || view.yolo === true || !SESSION_STATES.has(view.status)
    || !Number.isSafeInteger(view.started_at) || view.started_at <= 0
    || !Number.isSafeInteger(view.process_id) || view.process_id <= 0
    || !Number.isSafeInteger(view.restart_count) || view.restart_count < 0
    || !["agent", "ask"].includes(view.permission_mode)
    || typeof view.cwd !== "string" || !view.cwd || view.cwd.length > 4096 || view.cwd.includes("\0")
    || !Array.isArray(view.permitted_directories) || view.permitted_directories.length === 0 || view.permitted_directories.length > 32
    || view.permitted_directories.some((path) => typeof path !== "string" || !path || path.length > 4096 || path.includes("\0"))) return null;
  // pid is intentionally omitted: the authoritative view clears it on stop.
  // process_id and scope remain in retained session metadata.
  return JSON.stringify([hostId, sessionId, view.started_at, view.process_id, view.restart_count,
    view.permission_mode, view.cwd, view.permitted_directories]);
}

export async function currentSession(env, hostId, sessionId) {
  if (!validateHostId(hostId) || !validateSessionId(sessionId)) return null;
  try {
    const stub = hostStub(env, hostId);
    const status = await stub.fetch("https://host.internal/events/identity", { signal: AbortSignal.timeout(8000) });
    if (!status.ok) return null;
    const host = await safeBoundedJson(status, 4096, "event host status");
    if (host?.host_id !== hostId || host?.status !== "registered" || !Number.isSafeInteger(host.generation)
      || typeof host.instance_id !== "string" || host.instance_id.length > 128 || !host.instance_id) return null;
    const rpc = { jsonrpc: "2.0", id: `events-${crypto.randomUUID()}`, method: "tools/call",
      params: { name: "session_info", arguments: { session_id: sessionId } } };
    const response = await stub.fetch("https://host.internal/dispatch", {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ request: rpc }),
      signal: AbortSignal.timeout(8000),
    });
    if (!response.ok) return null;
    const result = await safeBoundedJson(response, SESSION_RESPONSE_LIMIT, "event session info");
    if (result?.error) return null;
    const text = result?.result?.content?.find((entry) => entry?.type === "text")?.text;
    const view = JSON.parse(text);
    const instanceKey = sessionIdentity(view, hostId, sessionId);
    if (!instanceKey) return null;
    return { host_id: hostId, session_id: sessionId, instance_key: instanceKey,
      state: view.status, generation: host.generation, host_instance_id: host.instance_id };
  } catch { return null; }
}

export async function sendThroughHost(env, envelope) {
  if (!await durableReady(env)) return { ok: false, reason: "sender_unavailable" };
  try {
    const response = await fetch(env.EVENT_SENDER_URL, {
      method: "POST", redirect: "error", signal: AbortSignal.timeout(26_000),
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${env.EVENT_SENDER_BEARER}`,
        "CF-Access-Client-Id": env.EVENT_SENDER_ACCESS_CLIENT_ID,
        "CF-Access-Client-Secret": env.EVENT_SENDER_ACCESS_CLIENT_SECRET,
      },
      body: JSON.stringify(envelope),
    });
    const result = await safeBoundedJson(response, 2048, "sender response");
    if (!response.ok) {
      const reasons = new Set(["invalid_url", "invalid_address", "invalid_secret", "invalid_challenge", "invalid_event", "event_too_large", "delivery_failed", "response_failed", "response_too_large", "response_timeout"]);
      return { ok: false, reason: reasons.has(result?.error) ? result.error : "sender_unavailable" };
    }
    return result && Number.isInteger(result.status) && result.status >= 100 && result.status <= 599
      ? { ok: true, status: result.status, verified: result.verified === true }
      : { ok: false, reason: "sender_unavailable" };
  } catch (error) { return { ok: false, reason: error?.name === "TimeoutError" ? "timeout" : "sender_unavailable" }; }
}

export async function handleEventMethod(rpc, env, identity) {
  const id = rpc.id ?? null;
  const db = env.OBSERVATION_DB;
  const method = rpc.method;
  const params = rpc.params;
  if (!params || typeof params !== "object" || Array.isArray(params)) return rpcError(id, -32602, "invalid event parameters");
  if (method === "events/list") {
    if (Object.keys(params).some((key) => !["cursor", "_meta"].includes(key)) || params.cursor !== undefined && params.cursor !== null) {
      return rpcError(id, -32602, "unsupported event cursor");
    }
    return rpcResult(id, { events: EVENT_CATALOG, nextCursor: null });
  }
  const name = params.name;
  const args = params.arguments;
  if (!validArguments(name, args)) return rpcError(id, -32602, "invalid event name or arguments");
  const delivery = params.delivery;
  if (!delivery || typeof delivery !== "object" || Array.isArray(delivery)
    || delivery.mode !== "webhook" || Object.keys(delivery).some((key) => !["mode", "url", "secret"].includes(key))) {
    return rpcError(id, -32602, "webhook delivery required");
  }
  const url = callbackUrl(delivery.url);
  if (!url) return rpcError(id, CALLBACK_ERROR, "CallbackEndpointError", { reason: "invalid_url" });
  const principal = await principalKey(identity, env);
  if (!principal) return rpcError(id, -32006, "principal authorization unavailable");
  const subId = await subscriptionId(principal, url, name, args);
  if (method === "events/unsubscribe") {
    if (delivery.secret !== undefined || Object.keys(params).some((key) => !["name", "arguments", "delivery", "_meta"].includes(key))) {
      return rpcError(id, -32602, "invalid unsubscribe parameters");
    }
    await removeSubscription(db, subId, principal);
    return rpcResult(id, {});
  }
  if (method !== "events/subscribe") return rpcError(id, -32601, "method not found");
  if (!validSecret(delivery.secret) || Object.keys(params).some((key) => !["name", "arguments", "delivery", "ttlMs", "cursor", "_meta"].includes(key))
    || params.cursor !== undefined && params.cursor !== null) return rpcError(id, -32602, "invalid subscription parameters");
  const ttl = grantedTtl(params.ttlMs, Object.hasOwn(params, "ttlMs"));
  if (ttl === null) return rpcError(id, -32602, "invalid ttlMs");
  const session = await currentSession(env, args.host_id, args.session_id);
  if (!session) return rpcError(id, -32006, "session authorization unavailable");
  const challenge = crypto.randomUUID();
  const checked = await sendThroughHost(env, {
    kind: "verification", url, subscriptionId: subId, secret: delivery.secret, challenge,
  });
  if (!checked.ok || !checked.verified) {
    return rpcError(id, CALLBACK_ERROR, "CallbackEndpointError", { reason: checked.reason ?? (checked.ok ? "challenge_failed" : "timeout") });
  }
  const now = Date.now();
  const expiresAt = now + ttl;
  await putSubscription(db, {
    id: subId, principal, callback_url: url, name, arguments_json: canonicalJson(args),
    host_id: args.host_id, session_id: args.session_id, job_id: args.job_id ?? null,
    instance_key: session.instance_key, secret: delivery.secret,
    verified_at: now, expires_at: expiresAt, updated_at: now,
  });
  return rpcResult(id, { id: subId, refreshBefore: new Date(expiresAt).toISOString(), cursor: null, truncated: false });
}
