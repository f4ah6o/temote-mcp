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
const CONTENT_KINDS = new Set(["text", "view", "view_digest", "error", "none"]);
const CONTEXT_BACKENDS = new Set(["codex", "opencode", "devin_acp", "devin_cloud"]);
const CONTEXT_REASONS = new Set([
  "instruction or acceptance recorded without a state view",
  "latest observed state needs attention",
  "a dispatch returned an error; prefer the authoritative backend record before retrying",
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

export function projectSessionList(value, hostId, {
  includeWorkspace = false,
  projectionLimit = DASHBOARD_MAX_SESSION_PROJECTION,
} = {}) {
  if (!Array.isArray(value) || value.length > DASHBOARD_MAX_SESSIONS) return null;
  if (!Number.isSafeInteger(projectionLimit) || projectionLimit < 0 || projectionLimit > DASHBOARD_MAX_SESSIONS) return null;
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
    if (sessions.length >= projectionLimit) continue;
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
  return { sessions, truncated: value.length > projectionLimit };
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
    const countValid = value.count === undefined
      || value.count === null
      || (nonNegativeInt(value.count) && value.count <= 64);
    const typesValid = Array.isArray(value.types)
      && value.types.length <= 4
      && value.types.every((type) => INTERACTION_TYPES.has(type))
      && new Set(value.types).size === value.types.length;
    const truncatedValid = typeof value.truncated === "boolean";
    if (!validSummary || (!expiredProjection && (!countValid || !typesValid || !truncatedValid))) {
      return { state: "unavailable" };
    }
    if (state === "none" && !expiredProjection
      && ((value.count !== undefined && value.count !== null && value.count !== 0)
        || value.types.length !== 0 || value.truncated)) {
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
    if (result?.success === false || !Array.isArray(result?.results)) {
      return { ok: false, error_code: "replica_unavailable" };
    }
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

export function projectContextResolve(value, sessionId) {
  if (!value || typeof value !== "object" || Array.isArray(value) || value.session_id !== sessionId) return null;
  const summary = value.current_summary && typeof value.current_summary === "object"
    && !Array.isArray(value.current_summary) ? value.current_summary : {};
  const freshness = value.freshness && typeof value.freshness === "object"
    && !Array.isArray(value.freshness) ? value.freshness : {};
  const partial = value.partial && typeof value.partial === "object"
    && !Array.isArray(value.partial) ? value.partial : {};
  const workspace = projectWorkspace(value.workspace);
  const output = {
    session_id: sessionId,
    ...(nonNegativeInt(value.context_schema_version) ? { context_schema_version: value.context_schema_version } : {}),
    ...(workspace?.repository ? { workspace: { repository: workspace.repository } } : {}),
    current_summary: {
      ...copyNonNegativeInts(summary, [
        "observations", "journal_revision", "instructions", "tasks_total",
        "tasks_active", "tasks_terminal", "tasks_attention",
      ]),
      ...(Array.isArray(summary.backends)
        ? { backends: [...new Set(summary.backends.filter((backend) => CONTEXT_BACKENDS.has(backend)))].slice(0, 16) }
        : {}),
      ...(isoTimestamp(summary.last_observed_at) ? { last_observed_at: isoTimestamp(summary.last_observed_at) } : {}),
    },
    unresolved: Array.isArray(value.unresolved)
      ? value.unresolved.slice(0, 64).map(projectUnresolvedItem).filter(Boolean)
      : [],
    recent_related_tasks: Array.isArray(value.recent_related_tasks)
      ? value.recent_related_tasks.slice(0, 32).map(projectRelatedTask).filter(Boolean)
      : [],
    refs: projectObservationRefs(value.refs),
    freshness: {
      ...(nonNegativeInt(freshness.resolved_revision) ? { resolved_revision: freshness.resolved_revision } : {}),
      ...(freshness.at_least_revision === null || nonNegativeInt(freshness.at_least_revision)
        ? { at_least_revision: freshness.at_least_revision }
        : {}),
      ...(typeof freshness.stale === "boolean" ? { stale: freshness.stale } : {}),
    },
    partial: {
      ...(typeof partial.journal_exists === "boolean" ? { journal_exists: partial.journal_exists } : {}),
      ...(typeof partial.journal_degraded === "boolean" ? { journal_degraded: partial.journal_degraded } : {}),
      ...copyNonNegativeInts(partial, ["corrupt_lines", "write_failures"]),
    },
    ...(safeLabel(value.memory?.worker) ? { memory: {
      worker: value.memory.worker,
      ...(typeof value.memory.stale === "boolean" ? { stale: value.memory.stale } : {}),
    } } : {}),
  };
  if (Number.isSafeInteger(value.generated_at) && value.generated_at >= 0) {
    output.generated_at = isoTimestamp(value.generated_at);
  }
  return output;
}

export function projectContextStatus(value, sessionId) {
  if (!value || typeof value !== "object" || Array.isArray(value) || value.session_id !== sessionId) return null;
  const sourceJournal = value.journal && typeof value.journal === "object" && !Array.isArray(value.journal)
    ? value.journal
    : null;
  if (!sourceJournal) return null;
  const journal = {
    ...(nonNegativeInt(sourceJournal.schema_version) ? { schema_version: sourceJournal.schema_version } : {}),
    ...(typeof sourceJournal.exists === "boolean" ? { exists: sourceJournal.exists } : {}),
    ...copyNonNegativeInts(sourceJournal, [
      "revision", "base_revision", "observations", "bytes", "max_bytes", "compactions",
      "write_failures", "corrupt_lines",
    ]),
    ...(typeof sourceJournal.degraded === "boolean" ? { degraded: sourceJournal.degraded } : {}),
  };
  const memory = value.memory && typeof value.memory === "object" && !Array.isArray(value.memory)
    ? {
      ...(safeLabel(value.memory.worker) ? { worker: value.memory.worker } : {}),
      ...(typeof value.memory.stale === "boolean" ? { stale: value.memory.stale } : {}),
    }
    : {};
  return { session_id: sessionId, journal, memory };
}

export async function readDashboardTimeline(env, ownerId, hostId, sessionId, cursor, limit) {
  if (!isD1(env?.OBSERVATION_DB) || !ownerId) return { ok: false, error_code: "replica_unavailable" };
  const sourceResult = await readDashboardReplicaSource(env, ownerId, hostId, sessionId);
  if (!sourceResult.ok) return sourceResult;
  let after = null;
  if (cursor !== null) {
    after = await decodeDashboardTimelineCursor(cursor, ownerId, hostId, sessionId);
    if (after === null) return { ok: false, error_code: "invalid_cursor", status: 400 };
  }
  const sql = after === null
    ? [
      "SELECT cloud_seq, source_revision, kind, action, target_backend, content_kind,",
      "state_status, state_revision, observed_at FROM observations",
      "WHERE owner_id = ? AND host_id = ? AND session_id = ?",
      "ORDER BY cloud_seq DESC LIMIT ?",
    ].join(" ")
    : [
      "SELECT cloud_seq, source_revision, kind, action, target_backend, content_kind,",
      "state_status, state_revision, observed_at FROM observations",
      "WHERE owner_id = ? AND host_id = ? AND session_id = ? AND cloud_seq > ?",
      "ORDER BY cloud_seq ASC LIMIT ?",
    ].join(" ");
  try {
    const bindings = after === null
      ? [ownerId, hostId, sessionId, limit + 1]
      : [ownerId, hostId, sessionId, after, limit + 1];
    const result = await env.OBSERVATION_DB.prepare(sql).bind(...bindings).all();
    if (result?.success === false || !Array.isArray(result?.results) || result.results.length > limit + 1) {
      return { ok: false, error_code: "replica_unavailable" };
    }
    const hasMore = result.results.length > limit;
    const selected = result.results.slice(0, limit);
    if (after === null) selected.reverse();
    const events = selected.map(projectTimelineEvent);
    if (events.some((event) => event === null)) return { ok: false, error_code: "replica_unavailable" };
    const lastSequence = events.at(-1)?.cloud_seq;
    return {
      ok: true,
      source: sourceResult.source,
      events,
      after,
      next_cursor: nonNegativeInt(lastSequence)
        ? await encodeDashboardTimelineCursor(ownerId, hostId, sessionId, lastSequence)
        : null,
      has_more: after === null ? false : hasMore,
      has_older: after === null ? hasMore : false,
    };
  } catch {
    return { ok: false, error_code: "replica_unavailable" };
  }
}

export async function encodeDashboardTimelineCursor(ownerId, hostId, sessionId, after) {
  if (!nonNegativeInt(after)) throw new TypeError("invalid timeline cursor position");
  const scope = await digestHex(`${ownerId}\0${hostId}\0${sessionId}`);
  const check = await digestHex(`dashboard-timeline-v1\0${scope}\0${after}`);
  const encoded = btoa(JSON.stringify({ v: 1, scope, after, check }));
  return encoded.replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/g, "");
}

export async function decodeDashboardTimelineCursor(cursor, ownerId, hostId, sessionId) {
  if (typeof cursor !== "string" || cursor.length < 1 || cursor.length > 512 || !/^[A-Za-z0-9_-]+$/.test(cursor)) return null;
  try {
    const normalized = cursor.replaceAll("-", "+").replaceAll("_", "/");
    const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
    const payload = JSON.parse(atob(padded));
    const keys = Object.keys(payload ?? {}).sort();
    if (
      !payload
      || typeof payload !== "object"
      || Array.isArray(payload)
      || keys.join(",") !== "after,check,scope,v"
      || payload.v !== 1
      || !nonNegativeInt(payload.after)
      || typeof payload.scope !== "string"
      || typeof payload.check !== "string"
    ) return null;
    const scope = await digestHex(`${ownerId}\0${hostId}\0${sessionId}`);
    const check = await digestHex(`dashboard-timeline-v1\0${scope}\0${payload.after}`);
    if (payload.scope !== scope || payload.check !== check) return null;
    return payload.after;
  } catch {
    return null;
  }
}

function projectUnresolvedItem(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || !boundedString(value.task_id, 256)) return null;
  const output = {
    task_id: value.task_id,
    ...(safeLabel(value.status) ? { status: value.status } : {}),
    ...(CONTEXT_REASONS.has(value.reason) ? { reason: value.reason } : {}),
    refs: projectObservationRefs(value.refs),
  };
  return output;
}

function projectRelatedTask(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || !boundedString(value.task_id, 256)) return null;
  const result = { task_id: value.task_id };
  if (CONTEXT_BACKENDS.has(value.backend)) result.backend = value.backend;
  const instruction = value.instruction && typeof value.instruction === "object" && !Array.isArray(value.instruction)
    ? value.instruction
    : null;
  if (instruction) {
    result.instruction = {
      ...copyNonNegativeInts(instruction, ["revision"]),
      ...(isoTimestamp(instruction.observed_at) ? { observed_at: isoTimestamp(instruction.observed_at) } : {}),
      ...(boundedString(instruction.operation_id, 128) ? { operation_id: instruction.operation_id } : {}),
      ...(safeLabel(instruction.actor?.transport) ? { actor: { transport: instruction.actor.transport } } : {}),
    };
  }
  const state = value.state && typeof value.state === "object" && !Array.isArray(value.state)
    ? value.state
    : null;
  if (state) {
    result.state = {
      ...copyNonNegativeInts(state, ["revision"]),
      ...(isoTimestamp(state.observed_at) ? { observed_at: isoTimestamp(state.observed_at) } : {}),
      ...(safeLabel(state.status) ? { status: state.status } : {}),
      ...(boundedString(state.execution_id, 128) ? { execution_id: state.execution_id } : {}),
      ...(typeof state.reconciliation_required === "boolean"
        ? { reconciliation_required: state.reconciliation_required }
        : {}),
    };
  }
  if (isoTimestamp(value.last_observed_at)) result.last_observed_at = isoTimestamp(value.last_observed_at);
  result.refs = projectObservationRefs(value.refs);
  return result;
}

function projectObservationRefs(value) {
  if (!Array.isArray(value)) return [];
  return value.slice(0, 64).map((item) => {
    if (
      !item
      || typeof item !== "object"
      || Array.isArray(item)
      || !boundedString(item.observation_id, 64)
      || !nonNegativeInt(item.revision)
      || !OBSERVATION_KINDS.has(item.kind)
    ) return null;
    return { observation_id: item.observation_id, revision: item.revision, kind: item.kind };
  }).filter(Boolean);
}

function projectTimelineEvent(row) {
  if (
    !row
    || typeof row !== "object"
    || !nonNegativeInt(Number(row.cloud_seq))
    || !nonNegativeInt(Number(row.source_revision))
    || !OBSERVATION_KINDS.has(row.kind)
    || !validIsoDate(row.observed_at)
  ) return null;
  return {
    cloud_seq: Number(row.cloud_seq),
    source_revision: Number(row.source_revision),
    kind: row.kind,
    ...(safeLabel(row.action) ? { action: row.action } : {}),
    ...(safeLabel(row.target_backend) ? { target_backend: row.target_backend } : {}),
    ...(CONTENT_KINDS.has(row.content_kind) ? { content_kind: row.content_kind } : {}),
    ...(safeLabel(row.state_status) ? { state_status: row.state_status } : {}),
    ...(row.state_revision !== null && row.state_revision !== undefined
      && nonNegativeInt(Number(row.state_revision))
      ? { state_revision: Number(row.state_revision) }
      : {}),
    observed_at: row.observed_at,
  };
}

function copyNonNegativeInts(value, fields) {
  return Object.fromEntries(fields
    .filter((field) => nonNegativeInt(value?.[field]))
    .map((field) => [field, value[field]]));
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

async function digestHex(value) {
  const bytes = new TextEncoder().encode(value);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return [...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function isD1(value) {
  return value && typeof value.prepare === "function";
}
