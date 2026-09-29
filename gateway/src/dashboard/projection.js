import { validateHostId, validateSessionId } from "../protocol.js";
import { safeBoundedJson } from "../http.js";
import { hostStub, registryStub } from "../routing.js";
import { filterOnlineRegistryHosts, validHostRpcResponse } from "../routing-runtime.js";

export const DASHBOARD_MAX_HOSTS = 256;
export const DASHBOARD_MAX_SESSIONS = 1024;
export const DASHBOARD_MAX_SESSION_PROJECTION = 256;
export const DASHBOARD_MAX_HOST_RPC_BYTES = 512 * 1024;
export const DASHBOARD_MAX_REGISTRY_BYTES = 1024 * 1024;
export const DASHBOARD_MAX_TIMELINE_LIMIT = 256;
export const DASHBOARD_DEFAULT_TIMELINE_LIMIT = 100;

const BACKENDS = Object.freeze(["codex", "opencode", "devin_acp", "devin_cloud"]);
const INTERACTION_STATES = new Set(["none", "pending", "unknown", "unsupported", "unavailable"]);
const INTERACTION_TYPES = new Set(["permission", "question", "approval"]);
const OBSERVATION_KINDS = new Set([
  "instruction",
  "operation_accepted",
  "execution_state",
  "evidence",
  "verification",
  "delivery",
  "reconciliation",
]);
const SAFE_LABEL = /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$/;
const OWNER_ID = /^[A-Za-z0-9._:@/-]{1,256}$/;

export function dashboardMembership(env) {
  const raw = env?.HOST_TOKENS_JSON;
  if (typeof raw !== "string" || raw.length === 0 || new TextEncoder().encode(raw).byteLength > 64 * 1024) {
    return { ok: false, error_code: "membership_unavailable" };
  }
  let tokens;
  try {
    tokens = JSON.parse(raw);
  } catch {
    return { ok: false, error_code: "membership_unavailable" };
  }
  if (!tokens || typeof tokens !== "object" || Array.isArray(tokens)) {
    return { ok: false, error_code: "membership_unavailable" };
  }
  const hostIds = Object.keys(tokens);
  if (hostIds.length > DASHBOARD_MAX_HOSTS) return { ok: false, error_code: "membership_unavailable" };
  for (const hostId of hostIds) {
    const token = Object.hasOwn(tokens, hostId) ? tokens[hostId] : undefined;
    if (
      !validateHostId(hostId)
      || typeof token !== "string"
      || token.length < 1
      || token.length > 4096
      || token.trim() !== token
    ) {
      return { ok: false, error_code: "membership_unavailable" };
    }
  }
  return { ok: true, hostIds: hostIds.sort() };
}

export function dashboardOwnerId(env) {
  const value = env?.OBSERVATION_OWNER_ID;
  return typeof value === "string" && OWNER_ID.test(value) ? value : null;
}

export function validateDashboardRegistryHosts(value) {
  if (!Array.isArray(value) || value.length > DASHBOARD_MAX_HOSTS) return null;
  const seen = new Set();
  for (const host of value) {
    if (
      !host
      || typeof host !== "object"
      || Array.isArray(host)
      || !validateHostId(host.host_id)
      || seen.has(host.host_id)
      || typeof host.instance_id !== "string"
      || host.instance_id.length < 1
      || host.instance_id.length > 128
      || !Number.isSafeInteger(host.generation)
      || host.generation < 1
      || !Number.isSafeInteger(host.expires_at)
      || host.expires_at < 1
      || !Number.isSafeInteger(host.connected_at)
      || host.connected_at < 1
      || !Number.isSafeInteger(host.last_seen)
      || host.last_seen < 1
    ) {
      return null;
    }
    if (host.platform !== undefined && !["macos", "linux", "wsl2", "windows", "unknown"].includes(host.platform)) {
      return null;
    }
    if (host.runtime_version !== undefined && !boundedString(host.runtime_version, 128)) return null;
    if (host.agent_protocol !== undefined && !Number.isSafeInteger(host.agent_protocol)) return null;
    if (host.control_protocol !== undefined && !Number.isSafeInteger(host.control_protocol)) return null;
    if (host.protocol_compatibility !== undefined && !["compatible", "incompatible"].includes(host.protocol_compatibility)) {
      return null;
    }
    for (const field of ["capabilities", "named_roots"]) {
      if (host[field] !== undefined && (
        !Array.isArray(host[field])
        || host[field].length > (field === "capabilities" ? 32 : 64)
        || host[field].some((item) => !boundedString(item, field === "capabilities" ? 64 : 128))
      )) return null;
    }
    seen.add(host.host_id);
  }
  return value;
}

export async function dashboardRegistryLiveness(env, configuredHostIds) {
  let response;
  try {
    response = await registryStub(env).fetch("https://registry.internal/hosts");
  } catch {
    return { ok: false, error_code: "registry_unavailable" };
  }
  if (!response.ok) return { ok: false, error_code: "registry_unavailable" };
  const registryRows = await safeBoundedJson(response, DASHBOARD_MAX_REGISTRY_BYTES, "dashboard registry response");
  const validRows = validateDashboardRegistryHosts(registryRows);
  if (!validRows) return { ok: false, error_code: "registry_incomplete" };

  const configured = new Set(configuredHostIds);
  const relevantRows = validRows.filter((row) => configured.has(row.host_id));
  let filtered;
  try {
    // Keep the gateway's established Durable Object status probe semantics,
    // while avoiding probes for valid hosts outside this dashboard's inventory.
    filtered = await filterOnlineRegistryHosts(relevantRows, env);
  } catch {
    return { ok: false, error_code: "registry_probe_unavailable" };
  }
  const byId = new Map(validRows.map((row) => [row.host_id, row]));
  const online = new Set(filtered.online.map((row) => row.host_id));
  const unavailable = new Set(filtered.unavailable);
  return { ok: true, byId, online, unavailable };
}

export async function dashboardHostLive(env, hostId, membership = dashboardMembership(env)) {
  if (!membership.ok) return { ok: false, status: 503, error_code: "membership_unavailable" };
  if (!membership.hostIds.includes(hostId)) return { ok: false, status: 404, error_code: "host_not_found" };
  const liveness = await dashboardRegistryLiveness(env, [hostId]);
  if (!liveness.ok) return { ok: false, status: 503, error_code: "host_unknown" };
  if (liveness.unavailable.has(hostId)) return { ok: false, status: 503, error_code: "host_unknown" };
  if (!liveness.online.has(hostId)) return { ok: false, status: 503, error_code: "host_offline" };
  return { ok: true, registryHost: liveness.byId.get(hostId) ?? null };
}

export async function callDashboardHostTool(env, hostId, toolName, args) {
  if (!["session_list", "task_list", "context_resolve", "context_status"].includes(toolName)) {
    return { ok: false, error_code: "host_tool_unavailable" };
  }
  const request = {
    jsonrpc: "2.0",
    id: `dashboard-${crypto.randomUUID()}`,
    method: "tools/call",
    params: { name: toolName, arguments: args },
  };
  let response;
  try {
    response = await hostStub(env, hostId).fetch("https://host.internal/dispatch", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ request }),
    });
  } catch {
    return { ok: false, error_code: "host_request_unavailable" };
  }
  if (!response.ok) return { ok: false, error_code: "host_request_unavailable" };
  const payload = await safeBoundedJson(response, DASHBOARD_MAX_HOST_RPC_BYTES, "dashboard host response");
  if (!validHostRpcResponse(payload, request.id) || payload.error || payload.result?.isError) {
    return { ok: false, error_code: "host_tool_unavailable" };
  }
  const text = Array.isArray(payload.result?.content)
    ? payload.result.content.find((entry) => entry?.type === "text")?.text
    : undefined;
  if (typeof text !== "string" || new TextEncoder().encode(text).byteLength > DASHBOARD_MAX_HOST_RPC_BYTES) {
    return { ok: false, error_code: "host_projection_unavailable" };
  }
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch {
    return { ok: false, error_code: "host_projection_unavailable" };
  }
}

export function projectSessionList(value, hostId, { includeWorkspace = false } = {}) {
  if (!Array.isArray(value) || value.length > DASHBOARD_MAX_SESSIONS) return null;
  const sessions = [];
  const seen = new Set();
  for (const session of value) {
    const sessionId = session?.session_id ?? session?.id;
    if (
      !validateSessionId(sessionId)
      || (session?.host_id !== undefined && session.host_id !== null && session.host_id !== hostId)
      || (session?.session_id !== undefined && session?.id !== undefined && session.session_id !== session.id)
      || seen.has(sessionId)
    ) return null;
    seen.add(sessionId);
    if (sessions.length >= DASHBOARD_MAX_SESSION_PROJECTION) continue;
    const projected = {
      host_id: hostId,
      session_id: sessionId,
      status: safeLabel(session?.status) ?? "unknown",
      ...optionalNumberField(session, "started_at"),
      ...optionalNumberField(session, "stopped_at"),
      ...optionalStringField(session, "permission_mode", 32),
      ...(typeof session?.yolo === "boolean" ? { yolo: session.yolo } : {}),
    };
    if (includeWorkspace) {
      const workspace = projectWorkspace(session?.workspace);
      if (workspace) projected.workspace = workspace;
    }
    sessions.push(projected);
  }
  return { sessions, truncated: value.length > DASHBOARD_MAX_SESSION_PROJECTION };
}

export function projectSessionInfo(value, hostId, sessionId) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  if (
    (value.session_id ?? value.id) !== sessionId
    || (value.host_id !== undefined && value.host_id !== null && value.host_id !== hostId)
    || (value.session_id !== undefined && value.id !== undefined && value.session_id !== value.id)
  ) return null;
  const view = {
    host_id: hostId,
    session_id: sessionId,
    status: safeLabel(value.status) ?? "unknown",
    ...optionalNumberField(value, "started_at"),
    ...optionalNumberField(value, "stopped_at"),
    ...optionalStringField(value, "permission_mode", 32),
    ...(typeof value.yolo === "boolean" ? { yolo: value.yolo } : {}),
  };
  const workspace = projectWorkspace(value.workspace);
  if (workspace) view.workspace = workspace;
  return view;
}

export function projectWorkspace(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const workspace = {};
  if (["canonical_checkout", "managed_worktree", "legacy_worktree"].includes(value.workspace_type)) {
    workspace.workspace_type = value.workspace_type;
  }
  for (const field of ["repository", "branch", "task"]) {
    if (boundedString(value[field], 256)) workspace[field] = value[field];
  }
  return Object.keys(workspace).length > 0 ? workspace : null;
}

export function projectTaskList(value, nowSeconds = Math.floor(Date.now() / 1000)) {
  if (!value || typeof value !== "object" || Array.isArray(value) || !Array.isArray(value.tasks) || value.tasks.length > 256) {
    return null;
  }
  const sourceBackends = value.backends && typeof value.backends === "object" && !Array.isArray(value.backends)
    ? value.backends
    : {};
  const tasksByBackend = new Map(BACKENDS.map((backend) => [backend, []]));
  for (const task of value.tasks) {
    const backend = BACKENDS.includes(task?.backend) ? task.backend : null;
    if (!backend || typeof task.task_id !== "string" || !boundedString(task.task_id, 256)) continue;
    tasksByBackend.get(backend).push({
      backend,
      task_id: task.task_id,
      status: safeLabel(task.status) ?? "unknown",
      ...(nonNegativeInt(task.revision) ? { revision: task.revision } : {}),
      ...(timestamp(task.last_updated_at) ? { last_updated_at: isoTimestamp(task.last_updated_at) } : {}),
      pending_interaction: projectPendingInteraction(task.pending_interaction, nowSeconds),
    });
  }
  const backends = BACKENDS.map((backend) => {
    const result = sourceBackends[backend];
    if (result?.status === "ok") {
      const total = nonNegativeInt(result.total) ? result.total : tasksByBackend.get(backend).length;
      const skipped = nonNegativeInt(result.skipped) ? result.skipped : 0;
      const truncated = skipped > 0 || total > tasksByBackend.get(backend).length || value.truncated === true;
      return {
        backend,
        status: "confirmed",
        tasks: tasksByBackend.get(backend),
        total,
        skipped,
        truncated,
      };
    }
    return { backend, status: "unavailable", error_code: "backend_unavailable" };
  });
  const projectedTotal = [...tasksByBackend.values()].reduce((total, tasks) => total + tasks.length, 0);
  return {
    backends,
    total: nonNegativeInt(value.total) ? value.total : projectedTotal,
    limit: nonNegativeInt(value.limit) ? value.limit : 64,
    truncated: value.truncated === true || backends.some((backend) => backend.truncated === true),
  };
}

export function projectPendingInteraction(value, nowSeconds = Math.floor(Date.now() / 1000)) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return { state: "unsupported" };
  if (value.state === undefined) return { state: "unsupported" };
  if (!INTERACTION_STATES.has(value.state)) return { state: "unknown" };
  let state = value.state;
  const expiredProjection = state === "unavailable"
    && value.expires_at !== undefined
    && nonNegativeInt(value.expires_at)
    && value.expires_at <= nowSeconds
    && value.count === undefined
    && value.types === undefined
    && value.truncated === undefined;
  if (["none", "pending", "unavailable"].includes(state)) {
    const validSummary = nonNegativeInt(value.summary_revision)
      && value.summary_revision > 0
      && nonNegativeInt(value.producer_epoch)
      && value.producer_epoch > 0
      && nonNegativeInt(value.observed_at)
      && nonNegativeInt(value.expires_at)
      && value.expires_at === value.observed_at + 30
      && ["runtime_owner", "host_remote_observer"].includes(value.producer_kind);
    const countValid = value.count === null || (nonNegativeInt(value.count) && value.count <= 64);
    const typesValid = Array.isArray(value.types)
      && value.types.length <= 4
      && value.types.every((type) => INTERACTION_TYPES.has(type))
      && new Set(value.types).size === value.types.length;
    const truncatedValid = typeof value.truncated === "boolean";
    if (!validSummary || (!expiredProjection && (!countValid || !typesValid || !truncatedValid))) {
      return { state: "unavailable" };
    }
    if (state === "none" && !expiredProjection
      && ((value.count !== null && value.count !== 0) || value.types.length !== 0 || value.truncated)) {
      return { state: "unavailable" };
    }
    if (expiredProjection || value.expires_at <= nowSeconds) state = "unavailable";
  }
  const output = { state };
  if (nonNegativeInt(value.count) && value.count <= 64) output.count = value.count;
  if (Array.isArray(value.types)) {
    output.types = [...new Set(value.types.filter((item) => INTERACTION_TYPES.has(item)))].slice(0, 4);
  }
  for (const field of ["summary_revision", "observed_at", "producer_epoch", "expires_at"]) {
    if (nonNegativeInt(value[field])) output[field] = value[field];
  }
  if (["runtime_owner", "host_remote_observer"].includes(value.producer_kind)) {
    output.producer_kind = value.producer_kind;
  }
  if (typeof value.truncated === "boolean") output.truncated = value.truncated;
  return output;
}

export async function readDashboardHostReplicas(env, ownerId, hostIds) {
  if (!isD1(env?.OBSERVATION_DB) || !ownerId) return { ok: false, error_code: "replica_unavailable" };
  if (hostIds.length === 0) return { ok: true, byHost: new Map() };
  const placeholders = hostIds.map(() => "?").join(",");
  const sql = [
    "SELECT host_id, MAX(last_synced_at) AS last_synced_at,",
    "MAX(source_head_revision) AS source_head_revision,",
    "MAX(acked_through_revision) AS acked_through_revision,",
    "MAX(cloud_head_seq) AS cloud_head_seq,",
    "MAX(journal_degraded) AS journal_degraded, SUM(gap_count) AS gap_count",
    `FROM observation_sources WHERE owner_id = ? AND host_id IN (${placeholders}) GROUP BY host_id`,
  ].join(" ");
  try {
    const result = await env.OBSERVATION_DB.prepare(sql).bind(ownerId, ...hostIds).all();
    if (!Array.isArray(result?.results)) return { ok: false, error_code: "replica_unavailable" };
    const memberIds = new Set(hostIds);
    const byHost = new Map();
    for (const row of result.results) {
      if (!memberIds.has(row?.host_id) || byHost.has(row.host_id) || !validReplicaRow(row)) {
        return { ok: false, error_code: "replica_unavailable" };
      }
      byHost.set(row.host_id, row);
    }
    return { ok: true, byHost };
  } catch {
    return { ok: false, error_code: "replica_unavailable" };
  }
}

export async function readDashboardReplicaSource(env, ownerId, hostId, sessionId) {
  if (!isD1(env?.OBSERVATION_DB) || !ownerId) return { ok: false, error_code: "replica_unavailable" };
  const sql = [
    "SELECT host_id, session_id, source_base_revision, source_head_revision,",
    "acked_through_revision, cloud_head_seq, journal_degraded, gap_count, last_synced_at",
    "FROM observation_sources WHERE owner_id = ? AND host_id = ? AND session_id = ?",
  ].join(" ");
  try {
    const row = await env.OBSERVATION_DB.prepare(sql).bind(ownerId, hostId, sessionId).first();
    if (row === null || row === undefined) return { ok: true, source: null };
    if (row.host_id !== hostId || row.session_id !== sessionId || !validReplicaRow(row, true)) {
      return { ok: false, error_code: "replica_unavailable" };
    }
    return { ok: true, source: row };
  } catch {
    return { ok: false, error_code: "replica_unavailable" };
  }
}

export function projectReplica(row) {
  if (!row) return { status: "unknown", authority: "fabric_replica", freshness: "unknown" };
  const degraded = Number(row.journal_degraded) === 1;
  const gaps = Number(row.gap_count) || 0;
  const stale = degraded || gaps > 0;
  return {
    status: stale ? "stale" : "confirmed",
    authority: "fabric_replica",
    freshness: "stale",
    ...(typeof row.last_synced_at === "string" && row.last_synced_at.length <= 64
      ? { last_synced_at: row.last_synced_at }
      : {}),
    source_base_revision: numberOrNull(row.source_base_revision),
    source_head_revision: numberOrNull(row.source_head_revision),
    acked_through_revision: numberOrNull(row.acked_through_revision),
    cloud_head_seq: numberOrNull(row.cloud_head_seq),
    journal_degraded: degraded,
    gap_count: gaps,
  };
}

export function isDashboardSessionId(value) {
  return validateSessionId(value);
}

export function isoTimestamp(value) {
  if (typeof value === "string") return validIsoDate(value) ? value : undefined;
  if (!Number.isSafeInteger(value) || value < 0) return undefined;
  const millis = value < 1_000_000_000_000 ? value * 1000 : value;
  if (!Number.isSafeInteger(millis)) return undefined;
  try {
    return new Date(millis).toISOString();
  } catch {
    return undefined;
  }
}

export function timestamp(value) {
  return isoTimestamp(value) !== undefined;
}

export function safeLabel(value) {
  return typeof value === "string" && SAFE_LABEL.test(value) ? value : null;
}

function optionalNumberField(value, key) {
  const normalized = isoTimestamp(value?.[key]);
  return normalized === undefined ? {} : { [key]: normalized };
}

function optionalStringField(value, key, maxBytes) {
  return boundedString(value?.[key], maxBytes) ? { [key]: value[key] } : {};
}

function boundedString(value, maxBytes) {
  return typeof value === "string" && value.length <= maxBytes
    && new TextEncoder().encode(value).byteLength <= maxBytes;
}

function nonNegativeInt(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function numberOrNull(value) {
  return nonNegativeInt(Number(value)) ? Number(value) : null;
}

function validReplicaRow(row, sessionScoped = false) {
  const integerFields = sessionScoped
    ? ["source_base_revision", "source_head_revision", "acked_through_revision", "cloud_head_seq", "journal_degraded", "gap_count"]
    : ["source_head_revision", "acked_through_revision", "cloud_head_seq", "journal_degraded", "gap_count"];
  if (integerFields.some((field) => !nonNegativeInt(Number(row?.[field])))) return false;
  if (![0, 1].includes(Number(row.journal_degraded))) return false;
  if (row.last_synced_at !== null && row.last_synced_at !== undefined && !validIsoDate(row.last_synced_at)) return false;
  if (sessionScoped && Number(row.acked_through_revision) > Number(row.source_head_revision)) return false;
  return true;
}

function validIsoDate(value) {
  return typeof value === "string" && value.length <= 64 && Number.isFinite(Date.parse(value));
}

function isD1(value) {
  return value && typeof value.prepare === "function";
}
