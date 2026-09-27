import { validateHostId, validateSessionId } from "./protocol.js";

export function withoutHostRoutingArgument(rpc) {
  const routed = structuredClone(rpc);
  const args = routed?.params?.arguments;
  if (args && typeof args === "object" && !Array.isArray(args)) delete args.host_id;
  return routed;
}

export function compareSessionRoute(a, b) {
  const aHost = a.host_id ?? "";
  const bHost = b.host_id ?? "";
  return aHost.localeCompare(bHost) || String(a.session_id ?? "").localeCompare(String(b.session_id ?? ""));
}

export function agentRoute(value) {
  if (validateHostId(value?.host_id) && !Object.hasOwn(value ?? {}, "session_id")) {
    return { host_id: value.host_id };
  }
  if (validateSessionId(value?.session_id) && !Object.hasOwn(value ?? {}, "host_id")) {
    return { session_id: value.session_id };
  }
  return {};
}

export function agentRouteKey(value) {
  const route = agentRoute(value);
  if (route.host_id) return `host:${route.host_id}`;
  if (route.session_id) return `session:${route.session_id}`;
  return null;
}

export function sessionStub(env, sessionId) {
  const id = env.GATEWAY_SESSIONS.idFromName(sessionId);
  return env.GATEWAY_SESSIONS.get(id);
}

export function hostStub(env, hostId) {
  const id = env.GATEWAY_SESSIONS.idFromName(`host:${hostId}`);
  return env.GATEWAY_SESSIONS.get(id);
}

export function registryStub(env) {
  const id = env.GATEWAY_REGISTRY.idFromName("global");
  return env.GATEWAY_REGISTRY.get(id);
}
