import { authorizeLegacyFederatedHost } from "../enrollment.js";
import { jsonResponse, readJson, unauthorizedHost, withCors } from "../http.js";
import { validateHostId, validateSessionId } from "../protocol.js";
import { currentSession } from "./service.js";
import { durableReady, recordTransition } from "./repository.js";
import { sweepEventOutbox } from "./outbox.js";

const JOB_STATES = new Set(["running", "completed", "failed", "stopped", "unknown"]);
const SESSION_STATES = new Set(["starting", "active", "stopping", "stopped", "crashed", "failed"]);

function validTimestamp(value) {
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z$/.test(value)) return false;
  const date = new Date(value);
  return Number.isFinite(date.valueOf()) && date.toISOString().slice(0, 19) === value.slice(0, 19);
}

export function transitionHostId(pathname) {
  const match = /^\/v1\/hosts\/([^/]+)\/events\/transition$/.exec(pathname);
  if (!match) return null;
  try { const hostId = decodeURIComponent(match[1]); return validateHostId(hostId) ? hostId : null; }
  catch { return null; }
}

export async function handleHostTransition(request, env, hostId) {
  if (request.method !== "POST") return withCors(new Response(null, { status: 405 }));
  if (request.headers.get("x-temote-host-id") !== hostId || !await authorizeLegacyFederatedHost(request, env, hostId)) return unauthorizedHost();
  if (!await durableReady(env)) return withCors(jsonResponse({ error: "events_unavailable" }, 503));
  const parsed = await readJson(request, 4096);
  if (!parsed.ok) return withCors(jsonResponse({ error: "invalid_json" }, 400));
  const body = parsed.value;
  if (!body || typeof body !== "object" || Array.isArray(body)
    || Object.keys(body).some((key) => !["name", "session_id", "job_id", "state", "timestamp", "generation", "host_instance_id", "session_started_at", "session_process_id", "session_restart_count", "session_permission_mode", "session_cwd", "session_permitted_directories"].includes(key))
    || !validateSessionId(body.session_id)
    || !Number.isSafeInteger(body.generation) || body.generation < 1
    || !Number.isSafeInteger(body.session_started_at) || body.session_started_at < 1
    || !Number.isSafeInteger(body.session_process_id) || body.session_process_id <= 0
    || !Number.isSafeInteger(body.session_restart_count) || body.session_restart_count < 0
    || !["agent", "ask"].includes(body.session_permission_mode)
    || typeof body.host_instance_id !== "string" || !body.host_instance_id || body.host_instance_id.length > 128
    || typeof body.session_cwd !== "string" || !body.session_cwd || body.session_cwd.length > 4096 || body.session_cwd.includes("\0")
    || !Array.isArray(body.session_permitted_directories) || body.session_permitted_directories.length === 0 || body.session_permitted_directories.length > 32
    || body.session_permitted_directories.some((path) => typeof path !== "string" || !path || path.length > 4096 || path.includes("\0"))
    || !validTimestamp(body.timestamp)
    || Math.abs(Date.now() - Date.parse(body.timestamp)) > 86_400_000) {
    return withCors(jsonResponse({ error: "invalid_transition" }, 400));
  }
  if (body.name === "job.state.changed") {
    if (typeof body.job_id !== "string" || !/^[A-Za-z0-9._:-]{1,128}$/.test(body.job_id) || !JOB_STATES.has(body.state)) {
      return withCors(jsonResponse({ error: "invalid_transition" }, 400));
    }
  } else if (body.name === "session.state.changed") {
    if (body.job_id !== undefined || !SESSION_STATES.has(body.state)) return withCors(jsonResponse({ error: "invalid_transition" }, 400));
  } else return withCors(jsonResponse({ error: "invalid_transition" }, 400));
  const current = await currentSession(env, hostId, body.session_id);
  const instanceKey = JSON.stringify([hostId, body.session_id, body.session_started_at, body.session_process_id,
    body.session_restart_count, body.session_permission_mode, body.session_cwd, body.session_permitted_directories]);
  if (!current || current.generation !== body.generation || current.host_instance_id !== body.host_instance_id
    || current.instance_key !== instanceKey) {
    return withCors(jsonResponse({ error: "ownership_unavailable" }, 409));
  }
  // The authenticated Host may reconnect after recording this transition.
  // Its retained full owner must still match; its present state need not be
  // the historical state. The projection rejects duplicate/backward changes.
  const data = {
    host_id: hostId, session_id: body.session_id,
    ...(body.name === "job.state.changed" ? { job_id: body.job_id } : {}),
    state: body.state, timestamp: new Date(body.timestamp).toISOString(),
  };
  try {
    await recordTransition(env.OBSERVATION_DB, { name: body.name, host_id: hostId, session_id: body.session_id, instance_key: instanceKey, data });
    await sweepEventOutbox(env);
    return withCors(jsonResponse({ accepted: true }));
  } catch { return withCors(jsonResponse({ error: "event_persistence_failed" }, 503)); }
}
