import {
  dashboardHostLive,
  dashboardMembership,
  dashboardOwnerId,
  dashboardRegistryLiveness,
  callDashboardHostTool,
  isoTimestamp,
  projectReplica,
  projectSessionInfo,
  projectSessionList,
  projectTaskList,
  readDashboardHostReplicas,
} from "./projection.js";
import { gatewayVersion, publicContractFingerprint, validateHostId } from "../protocol.js";
import { jsonResponse } from "../http.js";

const REFRESH = Object.freeze({ foreground_ms: 5_000, background_ms: 30_000 });

export async function handleDashboardApi(request, env) {
  const url = new URL(request.url);
  if (request.method !== "GET") {
    return failure("method_not_allowed", 405, { allow: "GET" });
  }
  const segments = decodePathSegments(url.pathname);
  if (!segments || segments[0] !== "dash" || segments[1] !== "api" || segments[2] !== "v1") {
    return failure("not_found", 404);
  }
  if (segments.length === 4 && segments[3] === "bootstrap") return bootstrap(env);
  if (segments.length === 4 && segments[3] === "hosts") return hosts(env);
  if (segments[3] !== "hosts" || !validateHostId(segments[4])) return failure("not_found", 404);
  const hostId = segments[4];
  if (segments.length === 6 && segments[5] === "sessions") return sessions(env, hostId);
  if (segments.length >= 7 && segments[5] === "sessions") {
    const sessionId = segments[6];
    if (!sessionId || segments.length > 8) return failure("not_found", 404);
    if (segments.length === 7) return sessionDetail(env, hostId, sessionId);
    if (segments[7] === "tasks" && segments.length === 8) return tasks(env, hostId, sessionId);
  }
  return failure("not_found", 404);
}

async function bootstrap(env) {
  try {
    const deployment = gatewayVersion(env);
    return jsonResponse({
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

async function hosts(env) {
  const membership = dashboardMembership(env);
  if (!membership.ok) {
    return jsonResponse({
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
  return jsonResponse({
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

async function sessions(env, hostId) {
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId });
  const source = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!source.ok) return componentFailure(source.error_code, "host_live", 503, { host_id: hostId });
  const sessionList = projectSessionList(source.value, hostId);
  if (!sessionList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId });
  return jsonResponse({
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { host_id: hostId, sessions: sessionList.sessions, truncated: sessionList.truncated },
  });
}

async function sessionDetail(env, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId, session_id: sessionId });
  const source = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!source.ok) return componentFailure(source.error_code, "host_live", 503, { host_id: hostId, session_id: sessionId });
  const sessionList = projectSessionList(source.value, hostId, { includeWorkspace: true });
  if (!sessionList) return componentFailure("host_projection_unavailable", "host_live", 503, { host_id: hostId, session_id: sessionId });
  const sessionSource = sessionList.sessions.find((candidate) => candidate.session_id === sessionId);
  const session = sessionSource ? projectSessionInfo(sessionSource, hostId, sessionId) : null;
  if (!session) return failure("session_not_found", 404, {}, { host_id: hostId, session_id: sessionId });
  return jsonResponse({
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { host_id: hostId, session_id: sessionId, session },
  });
}

async function tasks(env, hostId, sessionId) {
  if (!validSessionId(sessionId)) return failure("not_found", 404);
  const liveness = await dashboardHostLive(env, hostId);
  if (!liveness.ok) return hostUnavailable(liveness, { host_id: hostId, session_id: sessionId });
  const owner = await callDashboardHostTool(env, hostId, "session_list", {});
  if (!owner.ok) return componentFailure(owner.error_code, "host_live", 503, { host_id: hostId, session_id: sessionId });
  const ownerList = projectSessionList(owner.value, hostId);
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
  return jsonResponse({
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
  return jsonResponse({
    status: "unavailable",
    authority,
    freshness: "unavailable",
    error_code: errorCode,
    data,
  }, status, headers);
}

function component(status, authority, freshness, errorCode) {
  return {
    status,
    authority,
    freshness,
    ...(errorCode ? { error_code: errorCode } : {}),
  };
}

function failure(errorCode, status = 400, headers = {}, data = {}) {
  return jsonResponse({
    status: "unavailable",
    authority: "unavailable",
    freshness: "unavailable",
    error_code: errorCode,
    data,
  }, status, headers);
}

function decodePathSegments(pathname) {
  const raw = pathname.split("/").slice(1);
  if (raw.at(-1) === "") raw.pop();
  try {
    return raw.map((segment) => decodeURIComponent(segment));
  } catch {
    return null;
  }
}

function validSessionId(value) {
  return typeof value === "string" && /^(?!\.{1,2}$)[A-Za-z0-9._-]{1,64}$/.test(value);
}
