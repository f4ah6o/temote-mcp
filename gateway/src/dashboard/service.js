import {
  DASHBOARD_MAX_SESSIONS,
  dashboardHostLive,
  dashboardMembership,
  dashboardOwnerId,
  dashboardRegistryLiveness,
  callDashboardHostTool,
  isoTimestamp,
  projectReplica,
  projectContextResolve,
  projectContextStatus,
  projectSessionInfo,
  projectSessionList,
  projectTaskList,
  readDashboardHostReplicas,
  readDashboardReplicaSource,
  readDashboardTimeline,
} from "./projection.js";
import { gatewayVersion, publicContractFingerprint } from "../protocol.js";

const REFRESH = Object.freeze({ foreground_ms: 5_000, background_ms: 30_000 });

export async function bootstrap(env) {
  try {
    const deployment = gatewayVersion(env);
    return serviceResult({
      status: "confirmed",
      authority: "fabric",
      freshness: "current",
      data: {
        service: "temote-fabric",
        deployment,
        version: deployment,
        contract_fingerprint: await publicContractFingerprint(),
        authenticated: true,
        generated_at: new Date().toISOString(),
        refresh: REFRESH,
      },
    });
  } catch {
    return componentFailure("bootstrap_unavailable", "fabric", 503);
  }
}

export async function hosts(env) {
  const membership = dashboardMembership(env);
  if (!membership.ok) {
    return serviceResult({
      status: "unavailable",
      authority: "fabric",
      freshness: "unavailable",
      error_code: membership.error_code,
      data: {
        components: {
          membership: component("unavailable", "fabric", "unavailable", membership.error_code),
          liveness: component("unavailable", "fabric", "unavailable", "membership_unavailable"),
          replica: component("unavailable", "fabric_replica", "unavailable", "membership_unavailable"),
        },
      },
    }, 503);
  }

  const ownerId = dashboardOwnerId(env);
  const [liveness, replicaRead] = await Promise.all([
    dashboardRegistryLiveness(env, membership.hostIds),
    readDashboardHostReplicas(env, ownerId, membership.hostIds),
  ]);
  const hasUnavailableProbe = liveness.ok && liveness.unavailable.size > 0;
  const livenessComponent = liveness.ok && !hasUnavailableProbe
    ? component("confirmed", "fabric", "live")
    : component("unavailable", "fabric", "unknown", liveness.error_code ?? "registry_probe_unavailable");
  const replicaComponent = replicaRead.ok
    ? component("confirmed", "fabric_replica", "stale")
    : component("unavailable", "fabric_replica", "unavailable", replicaRead.error_code);
  const hosts = membership.hostIds.map((hostId) => {
    const registryHost = liveness.ok ? liveness.byId.get(hostId) ?? null : null;
    const hostOnline = liveness.ok && liveness.online.has(hostId);
    const hostUnavailable = !liveness.ok || liveness.unavailable.has(hostId);
    const availability = hostOnline ? "online" : hostUnavailable ? "unknown" : "offline";
    const replicaRow = replicaRead.ok ? replicaRead.byHost.get(hostId) ?? null : null;
    const replica = replicaRead.ok
      ? projectReplica(replicaRow)
      : { status: "unavailable", authority: "fabric_replica", freshness: "unavailable" };
    const evidence = ["configured_membership"];
    if (registryHost) evidence.push("registry_entry");
    if (hostOnline) evidence.push("live_route");
    if (replicaRow) evidence.push("fabric_replica");
    const connectionHistory = !liveness.ok
      ? { status: "unavailable" }
      : registryHost
        ? {
          status: "confirmed",
          ...(isoTimestamp(registryHost.connected_at) ? { connected_at: isoTimestamp(registryHost.connected_at) } : {}),
          ...(isoTimestamp(registryHost.last_seen) ? { last_seen: isoTimestamp(registryHost.last_seen) } : {}),
        }
        : { status: "unknown" };
    const host = {
      host_id: hostId,
      availability,
      evidence,
      connection_history: connectionHistory,
      replica,
    };
    if (hostOnline && registryHost) {
      host.live = {
        ...(typeof registryHost.platform === "string" ? { platform: registryHost.platform } : {}),
        ...(typeof registryHost.runtime_version === "string" ? { runtime_version: registryHost.runtime_version } : {}),
        ...(Number.isSafeInteger(registryHost.agent_protocol) ? { agent_protocol: registryHost.agent_protocol } : {}),
        ...(Number.isSafeInteger(registryHost.control_protocol) ? { control_protocol: registryHost.control_protocol } : {}),
        ...(typeof registryHost.protocol_compatibility === "string"
          ? { protocol_compatibility: registryHost.protocol_compatibility }
          : {}),
        ...(Array.isArray(registryHost.capabilities) ? { capabilities: registryHost.capabilities.slice(0, 32) } : {}),
      };
    }
    return host;
  });
  const partial = !liveness.ok || hasUnavailableProbe || !replicaRead.ok;
  return serviceResult({
    status: partial ? "stale" : "confirmed",
    authority: "fabric",
    freshness: partial ? "stale" : "live",
    data: {
      components: {
        membership: component("confirmed", "fabric", "current"),
        liveness: livenessComponent,
        replica: replicaComponent,
      },
      hosts,
    },
  });
}

export async function sessions(env, hostId) {
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId });
  const source = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!source.ok) return componentFailure(source.error_code, "host_live", 503, { host_id: hostId });
  const sessionList = projectSessionList(source.value, hostId);
  if (!sessionList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId });
  return serviceResult({
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { host_id: hostId, sessions: sessionList.sessions, truncated: sessionList.truncated },
  });
}

export async function sessionDetail(env, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId, session_id: sessionId });
  const source = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!source.ok) return componentFailure(source.error_code, "host_live", 503, { host_id: hostId, session_id: sessionId });
  const sessionList = projectSessionList(source.value, hostId, {
    includeWorkspace: true,
    projectionLimit: DASHBOARD_MAX_SESSIONS,
  });
  if (!sessionList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId, session_id: sessionId });
  const sessionSource = sessionList.sessions.find((candidate) => candidate.session_id === sessionId);
  const session = sessionSource ? projectSessionInfo(sessionSource, hostId, sessionId) : null;
  if (!session) return failure("session_not_found", 404, {}, { host_id: hostId, session_id: sessionId });
  return serviceResult({
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { host_id: hostId, session_id: sessionId, session },
  });
}

export async function tasks(env, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId, session_id: sessionId });
  const owner = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!owner.ok) return componentFailure(owner.error_code, "host_live", 503, { host_id: hostId, session_id: sessionId });
  const ownerList = projectSessionList(owner.value, hostId, { projectionLimit: DASHBOARD_MAX_SESSIONS });
  if (!ownerList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId, session_id: sessionId });
  if (!ownerList.sessions.some((candidate) => candidate.session_id === sessionId)) {
    return failure("session_not_found", 404, {}, { host_id: hostId, session_id: sessionId });
  }
  const source = await callDashboardHostTool(env, hostId, "task_list", { session_id: sessionId, limit: 64 });
  if (!source.ok) return componentFailure(source.error_code, "host_live", 503, { host_id: hostId, session_id: sessionId });
  const taskList = projectTaskList(source.value);
  if (!taskList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId, session_id: sessionId });
  const partial = taskList.truncated || taskList.backends.some((backend) => (
    backend.status !== "confirmed" || backend.truncated === true
  ));
  return serviceResult({
    status: partial ? "stale" : "confirmed",
    authority: "host_live",
    freshness: partial ? "stale" : "live",
    data: {
      host_id: hostId,
      session_id: sessionId,
      backends: taskList.backends,
      total: taskList.total,
      limit: taskList.limit,
      truncated: taskList.truncated,
    },
  });
}

export async function context(env, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const membership = dashboardMembership(env);
  if (!membership.ok) return componentFailure("membership_unavailable", "unavailable", 503, { host_id: hostId, session_id: sessionId });
  if (!membership.hostIds.includes(hostId)) return failure("host_not_found", 404);
  const ownerId = dashboardOwnerId(env);
  const replicaRead = await readDashboardReplicaSource(env, ownerId, hostId, sessionId);
  const replica = replicaComponent(replicaRead);
  const live = await dashboardHostLive(env, hostId, membership);
  if (!live.ok) {
    const code = live.error_code === "host_offline" || live.error_code === "host_unknown"
      ? live.error_code
      : "host_unavailable";
    const unavailable = component("unavailable", "unavailable", "unavailable", code);
    return serviceResult({
      status: "stale",
      authority: "unavailable",
      freshness: "stale",
      error_code: code,
      data: {
        host_id: hostId,
        session_id: sessionId,
        context_resolve: unavailable,
        context_status: unavailable,
        replica,
      },
    }, live.status);
  }
  const sessionListResult = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!sessionListResult.ok) {
    return contextFailure(sessionListResult.error_code, hostId, sessionId, replica, 503);
  }
  const sessionList = projectSessionList(sessionListResult.value, hostId, {
    projectionLimit: DASHBOARD_MAX_SESSIONS,
  });
  if (!sessionList) return contextFailure("host_projection_unavailable", hostId, sessionId, replica, 503);
  if (!sessionList.sessions.some((session) => session.session_id === sessionId)) {
    return failure("session_not_found", 404, {}, { host_id: hostId, session_id: sessionId });
  }
  const [resolveResult, statusResult] = await Promise.all([
    callDashboardHostTool(env, hostId, "context_resolve", { session_id: sessionId, limit: 16 }),
    callDashboardHostTool(env, hostId, "context_status", { session_id: sessionId }),
  ]);
  const resolveData = resolveResult.ok ? projectContextResolve(resolveResult.value, sessionId) : null;
  const statusData = statusResult.ok ? projectContextStatus(statusResult.value, sessionId) : null;
  const resolveComponent = resolveData
    ? component("confirmed", "host_live", "live", undefined, resolveData)
    : component("unavailable", "host_live", "unavailable", resolveResult.error_code ?? "host_projection_unavailable");
  const statusComponent = statusData
    ? component("confirmed", "host_live", "live", undefined, statusData)
    : component("unavailable", "host_live", "unavailable", statusResult.error_code ?? "host_projection_unavailable");
  const stale = !resolveData
    || !statusData
    || resolveData.freshness?.stale === true
    || resolveData.partial?.journal_degraded === true
    || statusData.journal?.degraded === true
    || replica.status === "stale"
    || replica.status === "unavailable";
  return serviceResult({
    status: stale ? "stale" : "confirmed",
    authority: "host_live",
    freshness: stale ? "stale" : "live",
    data: {
      host_id: hostId,
      session_id: sessionId,
      context_resolve: resolveComponent,
      context_status: statusComponent,
      replica,
    },
  });
}

export async function timeline(env, url, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const membership = dashboardMembership(env);
  if (!membership.ok) return componentFailure("membership_unavailable", "unavailable", 503, { host_id: hostId, session_id: sessionId });
  if (!membership.hostIds.includes(hostId)) return failure("host_not_found", 404);
  const ownerId = dashboardOwnerId(env);
  if (!ownerId) return componentFailure("replica_unavailable", "fabric_replica", 503, { host_id: hostId, session_id: sessionId });
  const afterValues = url.searchParams.getAll("after");
  const limitValues = url.searchParams.getAll("limit");
  if (afterValues.length > 1 || limitValues.length > 1) return failure("invalid_query", 400);
  const rawLimit = limitValues[0];
  const limit = rawLimit === undefined ? 100 : (/^(?:[1-9]\d{0,2})$/.test(rawLimit) ? Number(rawLimit) : null);
  if (limit === null || limit < 1 || limit > 256) return failure("invalid_limit", 400);
  const afterCursor = afterValues[0] ?? null;
  const result = await readDashboardTimeline(env, ownerId, hostId, sessionId, afterCursor, limit);
  if (!result.ok) {
    const status = result.status === 400 ? 400 : 503;
    return componentFailure(result.error_code, "fabric_replica", status, { host_id: hostId, session_id: sessionId });
  }
  const source = result.source ? projectReplica(result.source) : {
    status: "unknown",
    authority: "fabric_replica",
    freshness: "unknown",
  };
  const stale = source.status !== "confirmed";
  return serviceResult({
    status: stale ? "stale" : "confirmed",
    authority: "fabric_replica",
    freshness: "stale",
    data: {
      host_id: hostId,
      session_id: sessionId,
      events: result.events,
      next_cursor: result.next_cursor,
      has_more: result.has_more,
      ...(result.has_older ? { has_older: true } : {}),
      source,
    },
  });
}

function contextFailure(errorCode, hostId, sessionId, replica, status) {
  const unavailable = component("unavailable", "host_live", "unavailable", errorCode);
  return serviceResult({
    status: "unavailable",
    authority: "unavailable",
    freshness: "unavailable",
    error_code: errorCode,
    data: {
      host_id: hostId,
      session_id: sessionId,
      context_resolve: unavailable,
      context_status: unavailable,
      replica,
    },
  }, status);
}

function replicaComponent(read) {
  if (!read?.ok) return component("unavailable", "fabric_replica", "unavailable", read?.error_code ?? "replica_unavailable");
  if (!read.source) return component("unknown", "fabric_replica", "unknown");
  const projection = projectReplica(read.source);
  return component(projection.status, projection.authority, projection.freshness, undefined, projection);
}

function hostUnavailable(result, data) {
  const status = result.status === 404 ? 404 : 503;
  const errorCode = result.error_code === "host_offline" || result.error_code === "host_unknown"
    ? result.error_code
    : result.error_code === "host_not_found"
      ? result.error_code
    : result.error_code === "membership_unavailable"
      ? result.error_code
      : "host_unavailable";
  return componentFailure(errorCode, "unavailable", status, data);
}

function componentFailure(errorCode, authority, status, data = {}, headers = {}) {
  return serviceResult({
    status: "unavailable",
    authority,
    freshness: "unavailable",
    error_code: errorCode,
    data,
  }, status, headers);
}

function component(status, authority, freshness, errorCode, data) {
  return {
    status,
    authority,
    freshness,
    ...(errorCode ? { error_code: errorCode } : {}),
    ...(data === undefined ? {} : { data }),
  };
}

function failure(errorCode, status = 400, headers = {}, data = {}) {
  return serviceResult({
    status: "unavailable",
    authority: "unavailable",
    freshness: "unavailable",
    error_code: errorCode,
    data,
  }, status, headers);
}

function validSessionId(value) {
  return typeof value === "string" && /^(?!\.{1,2}$)[A-Za-z0-9._-]{1,64}$/.test(value);
}

function serviceResult(body, status = 200, headers = {}) {
  return { body, status, headers };
}
