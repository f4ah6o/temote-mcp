import { hosts, sessions, sessionDetail } from "../dashboard/service.js";
import { validateHostId, validateSessionId } from "../protocol.js";

export const MENTION_TOOL_NAME = "search_mentions";
export const MENTION_MAX_RESULTS = 32;
export const MENTION_MAX_QUERY_LENGTH = 128;

// Matches the installed @openai/mcp-extensions 0.1.0 createMentions tool.
export const MENTION_TOOL = Object.freeze({
  name: MENTION_TOOL_NAME,
  title: "Search Fabric mentions",
  description: "Search configured Hosts and live sessions by logical identity.",
  annotations: { readOnlyHint: true },
  inputSchema: {
    type: "object",
    properties: { query: { type: "string" } },
    required: ["query"],
    additionalProperties: false,
  },
  outputSchema: {
    type: "object",
    properties: { items: { type: "array", items: { type: "object" } } },
    required: ["items"],
  },
  _meta: { "openai/extensions": { "mentions/search": {} }, ui: { visibility: ["app"] } },
});

const SAFE_REPOSITORY = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$/;

function validIdentity(identity) {
  return identity && typeof identity.subject === "string" && identity.subject.length > 0;
}

export function validMentionQuery(args) {
  return args && typeof args === "object" && !Array.isArray(args)
    && Object.keys(args).length === 1 && typeof args.query === "string"
    && args.query.length <= MENTION_MAX_QUERY_LENGTH
    && !/[\x00-\x1f\x7f]/.test(args.query);
}

function link(uri, name, title) {
  return { type: "resource_link", uri, name, title };
}

// `read` is injectable for tests; production uses the same owner-scoped,
// bounded projections as the dashboard and MCP App. Never read raw host output.
export async function searchMentions(args, identity, env, read = { hosts, sessions, sessionDetail }) {
  if (!validIdentity(identity)) return { ok: false, code: "unauthorized" };
  if (!validMentionQuery(args)) return { ok: false, code: "invalid_arguments" };
  const query = args.query.toLocaleLowerCase("en-US");
  const inventory = await read.hosts(env);
  if (inventory.status !== 200
    || inventory.body?.data?.components?.membership?.status !== "confirmed"
    || inventory.body?.data?.components?.liveness?.status !== "confirmed"
    || !Array.isArray(inventory.body?.data?.hosts)) {
    return { ok: false, code: "mention_source_unavailable" };
  }
  const candidates = inventory.body.data.hosts;
  if (candidates.length > 256) return { ok: false, code: "mention_source_unavailable" };
  const items = [];
  const seenRepositories = new Set();
  let detailReads = 0;
  // Bound host RPC fanout. A query containing a Host ID searches that Host;
  // an empty or broad query samples at most eight Hosts for sessions.
  const sessionHosts = candidates.filter((host) => host?.availability === "online"
    && validateHostId(host.host_id))
    .sort((left, right) => Number(right.host_id.toLocaleLowerCase("en-US").includes(query))
      - Number(left.host_id.toLocaleLowerCase("en-US").includes(query)))
    .slice(0, 8);
  for (const host of candidates) {
    if (!validateHostId(host?.host_id)) return { ok: false, code: "mention_source_unavailable" };
    if (host.host_id.toLocaleLowerCase("en-US").includes(query)) {
      items.push(link(`temote-fabric://hosts/${encodeURIComponent(host.host_id)}`, host.host_id, `Host ${host.host_id}`));
      if (items.length >= MENTION_MAX_RESULTS) return { ok: true, items };
    }
  }
  for (const host of sessionHosts) {
    const result = await read.sessions(env, host.host_id);
    if (result.status !== 200 || result.body?.status !== "confirmed"
      || result.body?.freshness !== "live" || result.body?.data?.truncated
      || !Array.isArray(result.body?.data?.sessions)) {
      return { ok: false, code: "mention_source_unavailable" };
    }
    for (const session of result.body.data.sessions) {
      if (!validateSessionId(session?.session_id) || session.host_id !== host.host_id) {
        return { ok: false, code: "mention_source_unavailable" };
      }
      const label = `${host.host_id}/${session.session_id}`;
      if (label.toLocaleLowerCase("en-US").includes(query)) {
        items.push(link(`temote-fabric://hosts/${encodeURIComponent(host.host_id)}/sessions/${encodeURIComponent(session.session_id)}`,
          label, `Session ${label}`));
        if (items.length >= MENTION_MAX_RESULTS) return { ok: true, items };
      }
      // Repository labels are checked in the same authenticated Host/session
      // scope. Reject path separators, URLs, whitespace, and free-form names.
      if (detailReads >= 32) continue;
      detailReads += 1;
      const detail = await read.sessionDetail(env, host.host_id, session.session_id);
      if (detail.status !== 200 || detail.body?.status !== "confirmed"
        || detail.body?.freshness !== "live") continue;
      const repository = detail.body?.data?.session?.workspace?.repository;
      if (!SAFE_REPOSITORY.test(repository ?? "") || !repository.toLocaleLowerCase("en-US").includes(query)) continue;
      const key = `${label}:${repository}`;
      if (seenRepositories.has(key)) continue;
      seenRepositories.add(key);
      items.push(link(`temote-fabric://hosts/${encodeURIComponent(host.host_id)}/sessions/${encodeURIComponent(session.session_id)}/repositories/${encodeURIComponent(repository)}`,
        repository, `Repository ${repository} on ${label}`));
      if (items.length >= MENTION_MAX_RESULTS) return { ok: true, items };
    }
  }
  return { ok: true, items };
}
