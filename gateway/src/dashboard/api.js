import { bootstrap, hosts, sessions, sessionDetail, tasks, context, timeline } from "./service.js";
import { validateHostId } from "../protocol.js";
import { jsonResponse } from "../http.js";

export async function handleDashboardApi(request, env) {
  const result = await routeDashboardApi(request, env);
  return jsonResponse(result.body, result.status, result.headers);
}

async function routeDashboardApi(request, env) {
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
    if (segments.length === 8 && segments[7] === "tasks") return tasks(env, hostId, sessionId);
    if (segments.length === 8 && segments[7] === "context") return context(env, hostId, sessionId);
    if (segments.length === 8 && segments[7] === "timeline") return timeline(env, url, hostId, sessionId);
  }
  return failure("not_found", 404);
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


function failure(errorCode, status, headers = {}) {
  return { body: { status: "unavailable", authority: "unavailable", freshness: "unavailable", error_code: errorCode, data: {} }, status, headers };
}
