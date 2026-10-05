import { bootstrap, hosts, sessions, sessionDetail, tasks } from "../dashboard/service.js";
import { rpcError, validateHostId, validateSessionId } from "../protocol.js";
import appHtml from "./generated.js";

export const FABRIC_APP_URI = "ui://temote-fabric/overview";
export const FABRIC_APP_MIME = "text/html;profile=mcp-app";
export const FABRIC_RESOURCES_CAPABILITY = { listChanged: false, subscribe: false };

const RESOURCE_META = {
  ui: {
    csp: { connectDomains: [], resourceDomains: [], frameDomains: [], baseUriDomains: [] },
    prefersBorder: false,
  },
  "openai/ui": { availableDisplayModes: ["inline", "fullscreen"], preferredDisplayMode: "fullscreen" },
};

export function fabricResources() {
  return { resources: [{
    uri: FABRIC_APP_URI,
    name: "temote_fabric_overview",
    title: "Temote Fabric",
    description: "Read-only Fabric hosts, sessions, and retained task state.",
    mimeType: FABRIC_APP_MIME,
    _meta: RESOURCE_META,
  }] };
}

export function readFabricResource(params) {
  if (!params || params.uri !== FABRIC_APP_URI
    || Object.keys(params).some((key) => !["uri", "_meta"].includes(key))) {
    return { error: { code: -32602, message: "unknown or invalid Fabric UI resource" } };
  }
  return { result: { contents: [{
    uri: FABRIC_APP_URI, mimeType: FABRIC_APP_MIME, text: appHtml, _meta: RESOURCE_META,
  }] } };
}

const TOOLS = new Set(["fabric_overview", "fabric_session_list", "fabric_session_read"]);

export function isFabricTool(name) {
  return TOOLS.has(name);
}

function validArgs(name, args) {
  const keys = name === "fabric_overview" ? []
    : name === "fabric_session_list" ? ["host_id"] : ["host_id", "session_id"];
  return args && typeof args === "object" && !Array.isArray(args)
    && Object.keys(args).length === keys.length
    && Object.keys(args).every((key) => keys.includes(key))
    && (name === "fabric_overview" || validateHostId(args.host_id))
    && (name !== "fabric_session_read" || validateSessionId(args.session_id));
}

export async function callFabricTool(id, name, args, env) {
  if (!validArgs(name, args)) return rpcError(id, -32602, "invalid Fabric tool arguments");
  let view;
  try {
    if (name === "fabric_overview") {
      const [service, inventory] = await Promise.all([bootstrap(env), hosts(env)]);
      view = { service: service.body, inventory: inventory.body };
    } else if (name === "fabric_session_list") {
      view = { sessions: (await sessions(env, args.host_id)).body };
    } else {
      const session = await sessionDetail(env, args.host_id, args.session_id);
      // Do not query task stores until the selected session is confirmed to
      // belong to this host. Preserve a successful detail if the task read fails.
      view = { session: session.body };
      if (session.status === 200) view.tasks = (await tasks(env, args.host_id, args.session_id)).body;
    }
  } catch {
    // Provider/SQL exceptions never cross the MCP boundary.
    return rpcError(id, -32603, "fabric_read_unavailable");
  }
  const structuredContent = { kind: name, generated_at: new Date().toISOString(), ...view };
  return { jsonrpc: "2.0", id, result: {
    structuredContent,
    // Also useful to clients without MCP Apps support. These are the same
    // bounded, redacted projections, rather than raw host tool responses.
    content: [{ type: "text", text: summarizeView(structuredContent) }],
  } };
}

function summarizeView(view) {
  const lines = [`Temote Fabric · ${view.generated_at}`];
  const describe = (label, envelope) => {
    lines.push(`${label}: ${envelope?.status ?? "unavailable"}${envelope?.error_code ? ` (${envelope.error_code})` : ""}`);
  };
  if (view.kind === "fabric_overview") {
    describe("Service", view.service);
    describe("Inventory", view.inventory);
    for (const host of view.inventory?.data?.hosts ?? []) {
      lines.push(`${host.host_id}: ${host.availability}; replica ${host.replica?.freshness ?? "unknown"}; last synchronized ${host.replica?.last_synced_at ?? "unknown"}`);
    }
  } else if (view.kind === "fabric_session_list") {
    describe("Sessions", view.sessions);
    for (const session of view.sessions?.data?.sessions ?? []) lines.push(`${session.session_id}: ${session.status}; ${session.permission_mode ?? "unknown mode"}`);
    if (view.sessions?.data?.truncated) lines.push("Session list truncated.");
  } else {
    describe("Session", view.session);
    const session = view.session?.data?.session;
    if (session) lines.push(`${session.session_id}: ${session.status}; repository ${session.workspace?.repository ?? "unknown"}; branch ${session.workspace?.branch ?? "unknown"}`);
    describe("Retained tasks", view.tasks);
    for (const backend of view.tasks?.data?.backends ?? []) {
      lines.push(`${backend.backend}: ${backend.status}${backend.truncated ? "; truncated" : ""}`);
      for (const task of backend.tasks ?? []) lines.push(`${task.task_id}: ${task.status}; saved ${task.last_updated_at ?? "at an unknown time"}`);
    }
    lines.push("Task records are saved state; viewing does not reconcile backends.");
  }
  return lines.join("\n");
}
