// Loopback-only browser fixture. This intentionally runs outside Worker auth and
// must never be used as a production dashboard server.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const gatewayRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const assetsRoot = resolve(gatewayRoot, "assets/dash");
const port = Number(process.env.DASHBOARD_FIXTURE_PORT ?? 4173);
const maxControlBody = 4_096;
const allowedControlFields = new Set([
  "hostsUnavailable",
  "membershipMissing",
  "replicaUnavailable",
  "livenessUnavailable",
  "hostOffline",
  "tasksUnavailable",
  "codexUnavailable",
  "tasksSkipped",
  "tasksTruncated",
  "contextUnavailable",
  "timelineUnavailable",
  "pendingState",
  "taskRevision",
  "summaryRevision",
  "producerEpoch",
]);

const state = {
  hostsUnavailable: false,
  membershipMissing: false,
  replicaUnavailable: false,
  livenessUnavailable: false,
  hostOffline: false,
  tasksUnavailable: false,
  codexUnavailable: false,
  tasksSkipped: 0,
  tasksTruncated: false,
  contextUnavailable: false,
  timelineUnavailable: false,
  pendingState: "pending",
  taskRevision: "17",
  summaryRevision: "3",
  producerEpoch: "1",
};

const hostId = "fabric-local";
const sessionId = "session-demo-01";
const now = () => Math.floor(Date.now() / 1_000);

function response(status, value, extraHeaders = {}) {
  return new Response(JSON.stringify(value), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
      ...extraHeaders,
    },
  });
}

function envelope(status, authority, freshness, data, extra = {}) {
  return { status, authority, freshness, ...(data === undefined ? {} : { data }), ...extra };
}

function unavailable(errorCode) {
  return envelope("unavailable", "unavailable", "unavailable", undefined, { error_code: errorCode });
}

function component(status, authority, freshness, data, errorCode) {
  return envelope(status, authority, freshness, data, errorCode ? { error_code: errorCode } : {});
}

function hostProjection() {
  const availability = state.hostOffline ? "offline" : state.livenessUnavailable ? "unknown" : "online";
  return {
    host_id: hostId,
    availability,
    evidence: ["configured_membership", ...(availability === "online" ? ["live_route"] : [])],
    connection_history: {
      status: "confirmed",
      connected_at: now() - 3_600,
      last_seen: now() - 3,
    },
    replica: state.replicaUnavailable
      ? { status: "unavailable" }
      : {
        status: "confirmed",
        last_synced_at: now() - 22,
        source_head_revision: "42",
        acked_through_revision: "42",
        cloud_head_seq: "42",
        journal_degraded: false,
        gap_count: 0,
      },
    live: {
      platform: "linux",
      runtime_version: "0.0.0-fixture",
      control_protocol: 2,
      agent_protocol: 2,
      protocol_compatibility: "compatible",
      session_availability: "available",
      capabilities: ["sessions", "tasks", "observation_context"],
    },
  };
}

function hostEnvelope() {
  const liveness = state.livenessUnavailable
    ? component("unavailable", "unavailable", "unavailable", undefined, "registry_unavailable")
    : component("confirmed", "fabric", "current", { available: !state.hostOffline });
  const replica = state.replicaUnavailable
    ? component("unavailable", "unavailable", "unavailable", undefined, "replica_unavailable")
    : component("confirmed", "fabric_replica", "current", { count: 1 });
  return component(state.replicaUnavailable ? "stale" : "confirmed", "fabric", state.replicaUnavailable ? "stale" : "current", {
    components: {
      membership: component("confirmed", "fabric", "current", { count: state.membershipMissing ? 0 : 1 }),
      liveness,
      replica,
    },
    hosts: state.membershipMissing ? [] : [hostProjection()],
  });
}

function sessionsEnvelope() {
  return component("confirmed", "host_live", "live", {
    host_id: hostId,
    sessions: [{
      host_id: hostId,
      session_id: sessionId,
      status: "active",
      started_at: now() - 7_200,
      permission_mode: "agent",
      yolo: false,
    }],
  });
}

function sessionEnvelope() {
  return component("confirmed", "host_live", "live", {
    host_id: hostId,
    session_id: sessionId,
    session: {
      host_id: hostId,
      session_id: sessionId,
      status: "active",
      started_at: now() - 7_200,
      permission_mode: "agent",
      yolo: false,
      workspace: {
        workspace_type: "managed_worktree",
        repository: '<img src=x onerror="globalThis.fixtureXss=1">',
        branch: "feat/fabric-web-dashboard",
        task: "Render a safe dashboard projection",
      },
    },
  });
}

function taskProjection() {
  const observed = now();
  const pending = state.pendingState;
  const summary = {
    state: pending,
    summary_revision: state.summaryRevision,
    observed_at: observed,
    producer_kind: "runtime_owner",
    producer_epoch: state.producerEpoch,
    expires_at: observed + 30,
    ...(pending === "pending" ? { count: 1, types: ["approval"] } : {}),
  };
  return {
    backend: "codex",
    task_id: "task-fixture-01",
    status: "running",
    revision: state.taskRevision,
    last_updated_at: now() - 24,
    pending_interaction: summary,
  };
}

function tasksEnvelope() {
  if (state.tasksUnavailable) return response(503, unavailable("host_offline"));
  const codexBackend = {
    backend: "codex",
    status: state.codexUnavailable ? "unavailable" : "confirmed",
    ...(state.codexUnavailable
      ? { error_code: "task_store_unavailable" }
      : { tasks: [taskProjection()], total: 1 + state.tasksSkipped, skipped: state.tasksSkipped, truncated: state.tasksTruncated }),
  };
  const backends = [codexBackend, {
    backend: "opencode",
    status: "confirmed",
    tasks: [],
  }];
  const partial = state.codexUnavailable || state.tasksSkipped > 0 || state.tasksTruncated;
  return response(200, component(partial ? "stale" : "confirmed", "host_live", partial ? "stale" : "live", {
    host_id: hostId,
    session_id: sessionId,
    backends,
  }));
}

function contextEnvelope() {
  if (state.contextUnavailable) {
    return response(200, component("confirmed", "fabric", "current", {
      host_id: hostId,
      session_id: sessionId,
      context_resolve: component("unavailable", "unavailable", "unavailable", undefined, "context_unavailable"),
      context_status: component("confirmed", "host_live", "live", {
        session_id: sessionId,
        journal: { revision: "42", observations: 6, degraded: false, write_failures: 0, corrupt_lines: 0 },
        memory: { worker: "not_implemented", stale: true },
      }),
      replica: component("confirmed", "fabric_replica", "current", {
        last_synced_at: now() - 22,
        source_head_revision: "42",
        acked_through_revision: "42",
        cloud_head_seq: "42",
        journal_degraded: false,
        gap_count: 0,
      }),
    }));
  }
  return response(200, component("confirmed", "fabric", "current", {
    host_id: hostId,
    session_id: sessionId,
    context_resolve: component("confirmed", "host_live", "live", {
      context_schema_version: 1,
      session_id: sessionId,
      workspace: { repository: "temote-mcp" },
      current_summary: {
        observations: 6,
        journal_revision: "42",
        instructions: 1,
        tasks_total: 1,
        tasks_active: 1,
        tasks_terminal: 0,
        tasks_attention: 1,
        backends: ["codex"],
        last_observed_at: now() - 12,
      },
      unresolved: [{
        task_id: "task-fixture-01",
        status: "running",
        reason: "Latest observed state needs attention.",
        refs: [{ observation_id: "obs-fixture-06", revision: "42", kind: "execution_state" }],
      }],
      recent_related_tasks: [{
        task_id: "task-fixture-01",
        backend: "codex",
        instruction: { revision: "36", observed_at: now() - 2_500, actor: { transport: "mcp" } },
        state: { revision: "42", observed_at: now() - 12, status: "running", reconciliation_required: false },
        last_observed_at: now() - 12,
        refs: [{ observation_id: "obs-fixture-06", revision: "42", kind: "execution_state" }],
      }],
      refs: [{ observation_id: "obs-fixture-06", revision: "42", kind: "execution_state" }],
      freshness: { resolved_revision: "42", stale: false },
      partial: { journal_exists: true, journal_degraded: false, corrupt_lines: 0, write_failures: 0 },
      memory: { worker: "not_implemented", stale: true },
    }),
    context_status: component("confirmed", "host_live", "live", {
      session_id: sessionId,
      journal: { schema_version: 1, exists: true, revision: "42", observations: 6, bytes: 4096, max_bytes: 1_048_576, compactions: 0, write_failures: 0, corrupt_lines: 0, degraded: false },
      memory: { worker: "not_implemented", stale: true },
    }),
    replica: component("confirmed", "fabric_replica", "current", {
      last_synced_at: now() - 22,
      source_head_revision: "42",
      acked_through_revision: "42",
      cloud_head_seq: "42",
      journal_degraded: false,
      gap_count: 0,
    }),
  }));
}

function timelineEnvelope() {
  if (state.timelineUnavailable) return response(503, unavailable("replica_unavailable"));
  return response(200, component("confirmed", "fabric_replica", "current", {
    host_id: hostId,
    session_id: sessionId,
    events: [{
      cloud_seq: "42",
      source_revision: "42",
      kind: "execution_state",
      action: "task_get",
      target_backend: "codex",
      content_kind: "none",
      state_status: "running",
      state_revision: "17",
      observed_at: now() - 12,
    }],
    next_cursor: String(now()),
    has_more: false,
    source: { last_synced_at: now() - 22, journal_degraded: false, gap_count: 0 },
  }));
}

async function readControlBody(request) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > maxControlBody) throw new Error("fixture control body too large");
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

async function handle(request) {
  const url = new URL(request.url, "http://127.0.0.1");
  if (url.pathname === "/__fixture/state") {
    if (request.method === "GET") return response(200, state);
    if (request.method === "POST") {
      let next;
      try {
        next = await readControlBody(request);
      } catch {
        return response(400, { error: "invalid_fixture_state" });
      }
      if (!next || typeof next !== "object" || Array.isArray(next)
        || Object.keys(next).some((key) => !allowedControlFields.has(key))) {
        return response(400, { error: "invalid_fixture_state" });
      }
      for (const [key, value] of Object.entries(next)) {
        if (key.endsWith("Unavailable") || key === "hostOffline") {
          if (typeof value !== "boolean") return response(400, { error: "invalid_fixture_state" });
        } else if (key === "membershipMissing") {
          if (typeof value !== "boolean") return response(400, { error: "invalid_fixture_state" });
        } else if (key === "pendingState") {
          if (!["none", "pending", "unknown", "unsupported", "unavailable"].includes(value)) return response(400, { error: "invalid_fixture_state" });
        } else if (key === "tasksSkipped") {
          if (!Number.isSafeInteger(value) || value < 0 || value > 64) return response(400, { error: "invalid_fixture_state" });
        } else if (key === "tasksTruncated") {
          if (typeof value !== "boolean") return response(400, { error: "invalid_fixture_state" });
        } else if (typeof value !== "string" || value.length > 32) {
          return response(400, { error: "invalid_fixture_state" });
        }
        state[key] = value;
      }
      return response(200, state);
    }
    return response(405, { error: "method_not_allowed" }, { allow: "GET, POST" });
  }

  if (url.pathname === "/dash" || url.pathname === "/") {
    return new Response(null, { status: 308, headers: { location: "/dash/", "cache-control": "no-store" } });
  }
  if (url.pathname === "/dash/") {
    return new Response(await readFile(resolve(assetsRoot, "index.html")), { headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } });
  }
  if (url.pathname === "/dash/app.js") {
    return new Response(await readFile(resolve(assetsRoot, "app.js")), { headers: { "content-type": "text/javascript; charset=utf-8", "cache-control": "no-store" } });
  }
  if (url.pathname === "/dash/styles.css") {
    return new Response(await readFile(resolve(assetsRoot, "styles.css")), { headers: { "content-type": "text/css; charset=utf-8", "cache-control": "no-store" } });
  }
  if (request.method !== "GET" && url.pathname.startsWith("/dash/api/v1/")) {
    return response(405, unavailable("method_not_allowed"), { allow: "GET" });
  }
  if (request.method === "GET" && url.pathname === "/dash/api/v1/bootstrap") {
    return response(200, component("confirmed", "fabric", "current", {
      service: "temote-fabric",
      deployment: "fixture-local",
      version: "fixture-local",
      contract_fingerprint: "e620f7cce792a7d50c4a8dbd9ad017bf4a2fa195c568a6b984aa8f911f1de734",
      authenticated: true,
      generated_at: new Date().toISOString(),
      refresh: { foreground_ms: 5_000, background_ms: 30_000 },
    }));
  }
  if (request.method === "GET" && url.pathname === "/dash/api/v1/hosts") {
    return state.hostsUnavailable ? response(503, unavailable("inventory_unavailable")) : response(200, hostEnvelope());
  }

  const hostPath = `/dash/api/v1/hosts/${hostId}`;
  const selectedSessionPath = `${hostPath}/sessions/${sessionId}`;
  if (url.pathname === `${hostPath}/sessions`) {
    if (state.hostOffline || state.livenessUnavailable) return response(503, unavailable(state.hostOffline ? "host_offline" : "host_unknown"));
    return response(200, sessionsEnvelope());
  }
  if (url.pathname === selectedSessionPath) {
    if (state.hostOffline || state.livenessUnavailable) return response(503, unavailable(state.hostOffline ? "host_offline" : "host_unknown"));
    return response(200, sessionEnvelope());
  }
  if (url.pathname === `${selectedSessionPath}/tasks`) {
    if (state.hostOffline || state.livenessUnavailable) return response(503, unavailable(state.hostOffline ? "host_offline" : "host_unknown"));
    return tasksEnvelope();
  }
  if (url.pathname === `${selectedSessionPath}/context`) {
    if (state.hostOffline || state.livenessUnavailable) return response(503, unavailable(state.hostOffline ? "host_offline" : "host_unknown"));
    return contextEnvelope();
  }
  if (url.pathname === `${selectedSessionPath}/timeline`) return timelineEnvelope();
  if (url.pathname.startsWith("/dash/")) {
    return new Response("Not found", { status: 404, headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" } });
  }
  return new Response("Not found", { status: 404, headers: { "content-type": "text/plain; charset=utf-8" } });
}

const server = createServer(async (request, outgoing) => {
  try {
    const result = await handle(request);
    outgoing.writeHead(result.status, Object.fromEntries(result.headers.entries()));
    outgoing.end(Buffer.from(await result.arrayBuffer()));
  } catch {
    outgoing.writeHead(500, { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" });
    outgoing.end("Fixture server error");
  }
});

const invokedDirectly = process.argv[1]
  && pathToFileURL(resolve(process.argv[1])).href === import.meta.url;
if (invokedDirectly && !process.env.NODE_TEST_CONTEXT) {
  server.listen(port, "127.0.0.1", () => {
    process.stdout.write(`Dashboard fixture listening at http://127.0.0.1:${port}/dash/\n`);
    process.stdout.write("Loopback only; test fixture without Fabric or Access authentication.\n");
    process.stdout.write("Change scenarios with POST /__fixture/state and refresh the dashboard.\n");
  });
}

export { handle, server, state };
