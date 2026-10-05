import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";

import worker from "../src/index.js";
import { projectPendingInteraction } from "../src/dashboard/projection.js";

const ENCODER = new TextEncoder();

function base64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function makeAccessFixture() {
  const team = `dashboard-${randomUUID()}.cloudflareaccess.com`;
  const audience = `dashboard-aud-${randomUUID()}`;
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5",
    modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const jwk = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  jwk.kid = `dashboard-${randomUUID()}`;
  jwk.alg = "RS256";
  jwk.use = "sig";
  const claims = {
    iss: `https://${team}`,
    aud: audience,
    exp: Math.floor(Date.now() / 1000) + 3600,
    nbf: Math.floor(Date.now() / 1000) - 1,
    sub: "dashboard-user",
    email: "operator@example.test",
  };
  const input = `${base64url(ENCODER.encode(JSON.stringify({ alg: "RS256", kid: jwk.kid, typ: "JWT" })))}.${base64url(ENCODER.encode(JSON.stringify(claims)))}`;
  const signature = await crypto.subtle.sign(
    { name: "RSASSA-PKCS1-v1_5" },
    keyPair.privateKey,
    ENCODER.encode(input),
  );
  return { team, audience, jwk, assertion: `${input}.${base64url(signature)}` };
}

function sessionView(overrides = {}) {
  return {
    session_id: "project-a",
    status: "active",
    started_at: 1_790_000_000,
    stopped_at: null,
    permission_mode: "agent",
    yolo: false,
    cwd: "/private/work/project-a",
    pid: 54321,
    last_error: "private session detail",
    grants: { listen_ports: [443] },
    workspace: {
      workspace_type: "managed_worktree",
      repository: "temote-mcp",
      branch: "feat/dashboard",
      task: "project-a",
      cwd: "/private/work/project-a",
      repository_root: "/private/repository",
      workspace_root: "/private/work/project-a",
    },
    ...overrides,
  };
}

function registryHost(hostId, overrides = {}) {
  return {
    host_id: hostId,
    instance_id: `${hostId}-instance`,
    generation: 1,
    platform: "linux",
    connected_at: 1_790_000_000_000,
    last_seen: 1_790_000_010_000,
    expires_at: Date.now() + 60_000,
    agent_protocol: 1,
    runtime_version: "1.2.3",
    control_protocol: 2,
    protocol_compatibility: "compatible",
    capabilities: ["codex", "task_list"],
    named_roots: ["src"],
    ...overrides,
  };
}

function d1Fixture(rows = [], { success = true } = {}) {
  return {
    prepare(sql) {
      return {
        bind(...bindings) {
          return {
            async all() {
              assert.match(sql, /WHERE owner_id = \?/);
              const owner = bindings[0];
              const hosts = new Set(bindings.slice(1));
              return {
                success,
                results: rows.filter((row) => row.owner_id === owner && hosts.has(row.host_id))
                  .map((row) => ({
                    host_id: row.host_id,
                    last_synced_at: row.last_synced_at,
                    source_head_revision: row.source_head_revision,
                    acked_through_revision: row.acked_through_revision,
                    cloud_head_seq: row.cloud_head_seq,
                    journal_degraded: row.journal_degraded,
                    gap_count: row.gap_count,
                  })),
              };
            },
            async first() {
              assert.match(sql, /owner_id = \? AND host_id = \? AND session_id = \?/);
              return null;
            },
          };
        },
      };
    },
  };
}

function makeEnv(fixture, {
  membership = { "mac-main": "host-token", "offline-host": "offline-token" },
  registry = [registryHost("mac-main"), registryHost("outside-host")],
  onlineHosts = new Set(["mac-main"]),
  d1Rows = [{
    owner_id: "owner-one",
    host_id: "mac-main",
    last_synced_at: "2026-09-29T01:00:00.000Z",
    source_head_revision: 10,
    acked_through_revision: 9,
    cloud_head_seq: 7,
    journal_degraded: 0,
    gap_count: 1,
  }],
  sessions = [sessionView()],
  taskList = null,
  d1Success = true,
  contextResolve = null,
  contextStatus = null,
} = {}) {
  const calls = [];
  const probeCalls = [];
  const values = new Map([
    ["session_list", sessions],
    ["task_list", taskList ?? {
      tasks: [{
        backend: "codex",
        task_id: "task-one",
        status: "completed",
        revision: 4,
        last_updated_at: 1_790_000_100,
        pending_interaction: {
          state: "none",
          count: 0,
          types: [],
          summary_revision: 3,
          observed_at: Math.floor(Date.now() / 1000) - 1,
          producer_kind: "runtime_owner",
          producer_epoch: 8,
          expires_at: Math.floor(Date.now() / 1000) + 29,
          truncated: false,
          private_detail: "must not leave host",
        },
        report: { output: "must not leave host" },
      }],
      backends: {
        codex: { status: "ok", total: 2, skipped: 1 },
        devin_acp: { status: "ok", total: 0, skipped: 0 },
      },
      total: 2,
      limit: 64,
      truncated: false,
    }],
  ]);
  if (contextResolve !== null) values.set("context_resolve", contextResolve);
  if (contextStatus !== null) values.set("context_status", contextStatus);
  const registryStub = {
    async fetch(request) {
      assert.equal(new URL(typeof request === "string" ? request : request.url).pathname, "/hosts");
      return new Response(JSON.stringify(registry), { headers: { "content-type": "application/json" } });
    },
  };
  const sessionsNamespace = {
    idFromName(name) { return name; },
    get(id) {
      return {
        async fetch(request, init = {}) {
          const url = new URL(typeof request === "string" ? request : request.url);
          const hostId = id.slice("host:".length);
          if (url.pathname === "/status") {
            probeCalls.push(hostId);
            return onlineHosts.has(hostId)
              ? new Response(JSON.stringify({ status: "registered" }))
              : new Response(JSON.stringify({ error: "host_offline" }), { status: 404 });
          }
          assert.equal(url.pathname, "/dispatch");
          const body = typeof request === "string" ? JSON.parse(init.body) : await request.json();
          const rpc = body.request;
          const name = rpc.params.name;
          calls.push({ hostId, name, args: rpc.params.arguments });
          assert.ok(["session_list", "task_list", "context_resolve", "context_status"].includes(name), `unexpected dispatched tool ${name}`);
          const value = values.get(name);
          if (value === undefined) return new Response(JSON.stringify({ error: "unavailable" }), { status: 503 });
          return new Response(JSON.stringify({
            jsonrpc: "2.0",
            id: rpc.id,
            result: { content: [{ type: "text", text: JSON.stringify(value) }] },
          }), { headers: { "content-type": "application/json" } });
        },
      };
    },
  };
  const env = {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    CLIENT_TOKEN: "shared-client-token",
    HOST_TOKENS_JSON: typeof membership === "string" ? membership : JSON.stringify(membership),
    OBSERVATION_OWNER_ID: "owner-one",
    GATEWAY_DEPLOYMENT: { id: "deployment-test-9" },
    GATEWAY_REGISTRY: { idFromName() { return "registry"; }, get() { return registryStub; } },
    GATEWAY_SESSIONS: sessionsNamespace,
    OBSERVATION_DB: d1Fixture(d1Rows, { success: d1Success }),
  };
  return { env, calls, probeCalls };
}

function request(path, fixture) {
  return new Request(`https://fabric.example.test${path}`, {
    headers: { "cf-access-jwt-assertion": fixture.assertion },
  });
}

async function withJwks(fixture, action) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input) => {
    assert.equal(String(input), `https://${fixture.team}/cdn-cgi/access/certs`);
    return new Response(JSON.stringify({ keys: [fixture.jwk] }), { headers: { "content-type": "application/json" } });
  };
  try {
    return await action();
  } finally {
    globalThis.fetch = originalFetch;
  }
}

test("bootstrap reports deployment and contract without credentials", async () => {
  const fixture = await makeAccessFixture();
  const { env } = makeEnv(fixture);
  const response = await withJwks(fixture, () => worker.fetch(request("/dash/api/v1/bootstrap", fixture), env));
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.status, "confirmed");
  assert.equal(body.authority, "fabric");
  assert.equal(body.data.deployment, "deployment-test-9");
  assert.equal(body.data.version, "deployment-test-9");
  assert.equal(body.data.authenticated, true);
  assert.match(body.data.contract_fingerprint, /^[0-9a-f]{64}$/);
  assert.deepEqual(body.data.refresh, { foreground_ms: 5_000, background_ms: 30_000 });
  assert.equal(JSON.stringify(body).includes("shared-client-token"), false);
  assert.equal(JSON.stringify(body).includes(fixture.assertion), false);
});

test("host inventory is limited to strict configured membership and labels unknown history", async () => {
  const fixture = await makeAccessFixture();
  const { env, probeCalls } = makeEnv(fixture);
  const response = await withJwks(fixture, () => worker.fetch(request("/dash/api/v1/hosts", fixture), env));
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.status, "confirmed");
  assert.deepEqual(body.data.hosts.map((host) => host.host_id), ["mac-main", "offline-host"]);
  assert.deepEqual(probeCalls, ["mac-main"]);
  const [online, offline] = body.data.hosts;
  assert.equal(online.availability, "online");
  assert.deepEqual(online.evidence, ["configured_membership", "registry_entry", "live_route", "fabric_replica"]);
  assert.equal(online.connection_history.status, "confirmed");
  assert.equal(online.replica.status, "stale");
  assert.equal(online.replica.gap_count, 1);
  assert.equal(offline.availability, "offline");
  assert.equal(offline.connection_history.status, "unknown");
  assert.equal(offline.replica.status, "unknown");
  assert.equal(JSON.stringify(body).includes("host-token"), false);
  assert.equal(JSON.stringify(body).includes("outside-host"), false);
});

test("a failed D1 all-result cannot masquerade as complete replica data", async () => {
  const fixture = await makeAccessFixture();
  const { env } = makeEnv(fixture, {
    membership: { "mac-main": "host-token" },
    registry: [registryHost("mac-main")],
    d1Success: false,
  });

  await withJwks(fixture, async () => {
    const hostsResponse = await worker.fetch(request("/dash/api/v1/hosts", fixture), env);
    const hosts = await hostsResponse.json();
    assert.equal(hosts.data.hosts[0].availability, "online");
    assert.equal(hosts.data.components.replica.status, "unavailable");

    const timelineResponse = await worker.fetch(
      request("/dash/api/v1/hosts/mac-main/sessions/project-a/timeline", fixture),
      env,
    );
    assert.equal(timelineResponse.status, 503);
    assert.equal((await timelineResponse.json()).error_code, "replica_unavailable");
  });
});

test("session list and detail return only allow-listed metadata through read-only session_list", async () => {
  const fixture = await makeAccessFixture();
  const { env, calls } = makeEnv(fixture, {
    sessions: [sessionView(), sessionView({ session_id: "another", id: "another", status: "stopped" })],
  });
  const listResponse = await withJwks(fixture, () => worker.fetch(request("/dash/api/v1/hosts/mac-main/sessions", fixture), env));
  const list = await listResponse.json();
  assert.equal(listResponse.status, 200);
  assert.equal(list.status, "confirmed");
  assert.equal(list.data.host_id, "mac-main");
  assert.equal(list.data.sessions.length, 2);
  assert.equal(list.data.sessions[0].session_id, "project-a");
  assert.equal(list.data.sessions[0].workspace, undefined);
  assert.equal(JSON.stringify(list).includes("/private/work"), false);
  assert.equal(JSON.stringify(list).includes("last_error"), false);
  assert.equal(JSON.stringify(list).includes("pid"), false);

  const detailResponse = await withJwks(fixture, () => worker.fetch(
    request("/dash/api/v1/hosts/mac-main/sessions/project-a", fixture),
    env,
  ));
  const detail = await detailResponse.json();
  assert.equal(detailResponse.status, 200);
  assert.equal(detail.data.session.session_id, "project-a");
  assert.deepEqual(detail.data.session.workspace, {
    workspace_type: "managed_worktree",
    repository: "temote-mcp",
    branch: "feat/dashboard",
    task: "project-a",
  });
  assert.equal(JSON.stringify(detail).includes("/private"), false);
  assert.equal(JSON.stringify(detail).includes("private session detail"), false);
  assert.deepEqual(calls.map((call) => call.name), ["session_list", "session_list"]);
});

test("task list preserves partial backend data and strips result detail", async () => {
  const fixture = await makeAccessFixture();
  const { env, calls } = makeEnv(fixture);
  const response = await withJwks(fixture, () => worker.fetch(
    request("/dash/api/v1/hosts/mac-main/sessions/project-a/tasks", fixture),
    env,
  ));
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.status, "stale");
  assert.equal(body.data.total, 2);
  const codex = body.data.backends.find((backend) => backend.backend === "codex");
  assert.equal(codex.status, "confirmed");
  assert.equal(codex.tasks.length, 1);
  assert.equal(codex.tasks[0].pending_interaction.state, "none");
  assert.equal(codex.tasks[0].report, undefined);
  assert.equal(codex.tasks[0].pending_interaction.private_detail, undefined);
  assert.equal(codex.skipped, 1);
  assert.equal(codex.truncated, true);
  assert.equal(JSON.stringify(body).includes("must not leave host"), false);
  assert.deepEqual(calls.map((call) => call.name), ["session_list", "task_list"]);
});

test("context envelope is stale when resolver freshness is stale or a projection is unavailable", async () => {
  const fixture = await makeAccessFixture();
  const contextStatus = {
    session_id: "project-a",
    journal: { exists: true, degraded: false, revision: 4 },
    memory: { worker: "not_implemented", stale: true },
  };
  const options = {
    membership: { "mac-main": "host-token" },
    registry: [registryHost("mac-main")],
    d1Rows: [],
    contextResolve: {
      session_id: "project-a",
      current_summary: {},
      unresolved: [],
      recent_related_tasks: [],
      refs: [],
      freshness: { stale: true, resolved_revision: 4, at_least_revision: 5 },
      partial: { journal_degraded: false },
    },
    contextStatus,
  };
  const { env: staleEnv } = makeEnv(fixture, options);
  await withJwks(fixture, async () => {
    const staleResponse = await worker.fetch(
      request("/dash/api/v1/hosts/mac-main/sessions/project-a/context", fixture),
      staleEnv,
    );
    const stale = await staleResponse.json();
    assert.equal(stale.status, "stale");
    assert.equal(stale.freshness, "stale");
    assert.equal(stale.data.context_resolve.status, "confirmed");

    const { env: unavailableEnv } = makeEnv(fixture, {
      ...options,
      contextResolve: { session_id: "other-session" },
    });
    const unavailableResponse = await worker.fetch(
      request("/dash/api/v1/hosts/mac-main/sessions/project-a/context", fixture),
      unavailableEnv,
    );
    const unavailable = await unavailableResponse.json();
    assert.equal(unavailable.status, "stale");
    assert.equal(unavailable.freshness, "stale");
    assert.equal(unavailable.data.context_resolve.status, "unavailable");
  });
});

test("malformed, duplicate, oversized, or cross-host session lists fail closed on direct routes", async () => {
  const fixture = await makeAccessFixture();
  const invalidCases = [
    {
      path: "/dash/api/v1/hosts/mac-main/sessions",
      sessions: [sessionView({ session_id: "../outside" })],
    },
    {
      path: "/dash/api/v1/hosts/mac-main/sessions/project-a",
      sessions: [sessionView(), sessionView()],
    },
    {
      path: "/dash/api/v1/hosts/mac-main/sessions",
      sessions: Array.from({ length: 1025 }, (_value, index) => ({
        session_id: `session-${index}`,
        status: "active",
      })),
    },
    {
      path: "/dash/api/v1/hosts/mac-main/sessions/project-a/tasks",
      sessions: [sessionView({ host_id: "outside-host" })],
    },
  ];
  await withJwks(fixture, async () => {
    for (const invalidCase of invalidCases) {
      const { env, calls } = makeEnv(fixture, { sessions: invalidCase.sessions });
      const response = await worker.fetch(request(invalidCase.path, fixture), env);
      assert.equal(response.status, 503);
      assert.equal((await response.json()).error_code, "host_projection_unavailable");
      assert.deepEqual(calls.map((call) => call.name), ["session_list"]);
    }
  });
});

test("detail and task routes can validate sessions beyond the display page cap", async () => {
  const fixture = await makeAccessFixture();
  const sessions = Array.from({ length: 300 }, (_value, index) => ({
    session_id: `session-${index}`,
    status: "active",
  }));
  const target = "session-299";
  const { env, calls } = makeEnv(fixture, { sessions });
  await withJwks(fixture, async () => {
    const listResponse = await worker.fetch(request("/dash/api/v1/hosts/mac-main/sessions", fixture), env);
    const list = await listResponse.json();
    assert.equal(list.data.sessions.length, 256);
    assert.equal(list.data.truncated, true);

    const detailResponse = await worker.fetch(
      request(`/dash/api/v1/hosts/mac-main/sessions/${target}`, fixture),
      env,
    );
    assert.equal(detailResponse.status, 200);
    assert.equal((await detailResponse.json()).data.session.session_id, target);

    const tasksResponse = await worker.fetch(
      request(`/dash/api/v1/hosts/mac-main/sessions/${target}/tasks`, fixture),
      env,
    );
    assert.equal(tasksResponse.status, 200);
    assert.equal((await tasksResponse.json()).data.session_id, target);
  });
  assert.deepEqual(calls.map((call) => call.name), ["session_list", "session_list", "session_list", "task_list"]);
});

test("malformed membership, registry, and direct nonmember host requests fail explicitly", async () => {
  const fixture = await makeAccessFixture();
  const malformedMembership = makeEnv(fixture, { membership: "{not-json" });
  const membershipResponse = await withJwks(fixture, () => worker.fetch(
    request("/dash/api/v1/hosts", fixture),
    malformedMembership.env,
  ));
  const membership = await membershipResponse.json();
  assert.equal(membershipResponse.status, 503);
  assert.equal(membership.error_code, "membership_unavailable");
  assert.equal(membership.data.hosts, undefined);

  const duplicateRegistry = makeEnv(fixture, {
    registry: [registryHost("mac-main"), registryHost("mac-main", { generation: 2 })],
  });
  const registryResponse = await withJwks(fixture, () => worker.fetch(
    request("/dash/api/v1/hosts", fixture),
    duplicateRegistry.env,
  ));
  const registryBody = await registryResponse.json();
  assert.equal(registryResponse.status, 200);
  assert.equal(registryBody.data.hosts[0].availability, "unknown");
  assert.equal(registryBody.data.components.liveness.status, "unavailable");

  const directResponse = await withJwks(fixture, () => worker.fetch(
    request("/dash/api/v1/hosts/outside-host/sessions", fixture),
    duplicateRegistry.env,
  ));
  assert.equal(directResponse.status, 404);
  assert.equal((await directResponse.json()).error_code, "host_not_found");
});

test("context resolve treats missing containers and malformed items as unavailable, never confirmed-empty", async () => {
  const fixture = await makeAccessFixture();
  const contextStatus = {
    session_id: "project-a",
    journal: { exists: true, degraded: false, revision: 4 },
    memory: { worker: "not_implemented", stale: true },
  };
  const base = {
    session_id: "project-a",
    context_schema_version: 1,
    workspace: { repository: "temote-mcp" },
    current_summary: { observations: 2, journal_revision: 4, tasks_total: 1, tasks_active: 1, tasks_attention: 1, backends: ["codex"], last_observed_at: null },
    unresolved: [{
      task_id: "task-one",
      status: "running",
      reason: "latest observed state needs attention",
      refs: [{ observation_id: "obs-1", revision: 1, kind: "instruction" }],
    }],
    recent_related_tasks: [{
      task_id: "task-one",
      backend: "codex",
      refs: [{ observation_id: "obs-1", revision: 1, kind: "instruction" }],
    }],
    refs: [{ observation_id: "obs-1", revision: 1, kind: "instruction" }],
    freshness: { resolved_revision: 4, at_least_revision: null, stale: false },
    partial: { journal_exists: true, journal_degraded: false, corrupt_lines: 0, write_failures: 0 },
    memory: { worker: "not_implemented", stale: true },
    generated_at: 1_790_000_200,
  };
  await withJwks(fixture, async () => {
    const { env: okEnv } = makeEnv(fixture, { contextResolve: base, contextStatus, d1Rows: [] });
    const okResponse = await worker.fetch(
      request("/dash/api/v1/hosts/mac-main/sessions/project-a/context", fixture),
      okEnv,
    );
    const ok = await okResponse.json();
    assert.equal(ok.data.context_resolve.status, "confirmed");
    assert.equal(ok.data.context_resolve.data.unresolved.length, 1);
    assert.equal(ok.data.context_resolve.data.unresolved[0].task_id, "task-one");

    const malformed = [
      { unresolved: undefined },
      { unresolved: "not-an-array" },
      { unresolved: [null] },
      { unresolved: [{ status: "running", reason: "latest observed state needs attention" }] },
      { unresolved: [{ task_id: "task-one", refs: "broken" }] },
      { unresolved: [{ task_id: "task-one", refs: [{ observation_id: "obs-1", revision: 1, kind: "instruction" }, null] }] },
      { current_summary: undefined },
      { current_summary: [] },
      { freshness: undefined },
      { freshness: "stale" },
      { freshness: { resolved_revision: 4 } },
      { partial: undefined },
      { partial: [] },
      { partial: { journal_exists: true } },
      { recent_related_tasks: undefined },
      { recent_related_tasks: {} },
      { recent_related_tasks: [{ backend: "codex", refs: [] }] },
      { recent_related_tasks: [{ task_id: "task-one", refs: [{ observation_id: "obs-1", kind: "instruction" }] }] },
      { refs: undefined },
      { refs: "broken" },
      { refs: [{ observation_id: "obs-1", revision: 1, kind: "instruction" }, { observation_id: "obs-bad" }] },
      { refs: [{ observation_id: "obs-1", revision: 1, kind: "not-a-kind" }] },
    ];
    for (const broken of malformed) {
      const { env } = makeEnv(fixture, {
        contextResolve: { ...base, ...broken },
        contextStatus,
        d1Rows: [],
      });
      const response = await worker.fetch(
        request("/dash/api/v1/hosts/mac-main/sessions/project-a/context", fixture),
        env,
      );
      const body = await response.json();
      assert.equal(response.status, 200, JSON.stringify(broken));
      assert.equal(body.data.context_resolve.status, "unavailable", JSON.stringify(broken));
      assert.equal(body.data.context_resolve.error_code, "host_projection_unavailable", JSON.stringify(broken));
      assert.equal(body.data.context_resolve.data, undefined, JSON.stringify(broken));
      assert.equal(body.status, "stale", JSON.stringify(broken));
      assert.equal(JSON.stringify(body).includes('"unresolved":[]'), false, JSON.stringify(broken));
    }
  });
});

test("task list fails closed on malformed rows instead of reporting a backend as empty", async () => {
  const fixture = await makeAccessFixture();
  const baseTaskList = () => ({
    tasks: [{
      backend: "codex",
      task_id: "task-one",
      status: "completed",
      revision: 4,
      last_updated_at: 1_790_000_100,
      pending_interaction: { state: "unsupported" },
      report: { output: "must not leave host" },
    }],
    backends: {
      codex: { status: "ok", total: 1, skipped: 0 },
      devin_acp: { status: "ok", total: 0, skipped: 0 },
    },
    total: 1,
    limit: 64,
    truncated: false,
  });

  const backendUnavailableCases = [
    { tasks: [{ backend: "codex", status: "completed" }] },
    { tasks: [{ backend: "codex", task_id: "x".repeat(300) }] },
    { tasks: [{ backend: "codex", task_id: 42 }] },
    { tasks: [{ backend: "codex", task_id: "task-one" }], backends: { codex: { status: "ok", skipped: 0 }, devin_acp: { status: "ok", total: 0, skipped: 0 } }, total: 0 },
    { tasks: [{ backend: "codex", task_id: "task-one" }], backends: { codex: { status: "ok", total: 0, skipped: 0 }, devin_acp: { status: "ok", total: 0, skipped: 0 } }, total: 0 },
    { backends: { codex: { status: "ok", total: 1 }, devin_acp: { status: "ok", total: 0, skipped: 0 } } },
  ];
  await withJwks(fixture, async () => {
    for (const broken of backendUnavailableCases) {
      const taskList = { ...baseTaskList(), ...broken };
      const { env } = makeEnv(fixture, { taskList });
      const response = await worker.fetch(
        request("/dash/api/v1/hosts/mac-main/sessions/project-a/tasks", fixture),
        env,
      );
      const body = await response.json();
      assert.equal(response.status, 200, JSON.stringify(broken));
      assert.equal(body.status, "stale", JSON.stringify(broken));
      const codex = body.data.backends.find((backend) => backend.backend === "codex");
      assert.equal(codex.status, "unavailable", JSON.stringify(broken));
      assert.equal(codex.tasks, undefined, JSON.stringify(broken));
      const devinAcp = body.data.backends.find((backend) => backend.backend === "devin_acp");
      assert.equal(devinAcp.status, "confirmed", JSON.stringify(broken));
      assert.deepEqual(devinAcp.tasks, [], JSON.stringify(broken));
      assert.equal(JSON.stringify(body).includes("must not leave host"), false);
    }

    const mixed = baseTaskList();
    mixed.tasks = [
      { backend: "codex", task_id: 42 },
      { backend: "devin_acp", task_id: "task-two", status: "running", pending_interaction: { state: "unsupported" } },
    ];
    mixed.backends.devin_acp = { status: "ok", total: 1, skipped: 0 };
    mixed.total = 2;
    const { env: mixedEnv } = makeEnv(fixture, { taskList: mixed });
    const mixedResponse = await worker.fetch(
      request("/dash/api/v1/hosts/mac-main/sessions/project-a/tasks", fixture),
      mixedEnv,
    );
    const mixedBody = await mixedResponse.json();
    assert.equal(mixedResponse.status, 200);
    assert.equal(mixedBody.status, "stale");
    const mixedCodex = mixedBody.data.backends.find((backend) => backend.backend === "codex");
    assert.equal(mixedCodex.status, "unavailable");
    const mixedDevin = mixedBody.data.backends.find((backend) => backend.backend === "devin_acp");
    assert.equal(mixedDevin.status, "confirmed");
    assert.equal(mixedDevin.tasks.length, 1);
    assert.equal(mixedDevin.tasks[0].task_id, "task-two");

    const structuralCases = [
      { tasks: [{ backend: "unknown-backend", task_id: "task-one" }] },
      { tasks: [{ task_id: "task-one" }] },
      { tasks: [null] },
      { tasks: ["broken"] },
      { tasks: undefined },
      { backends: "broken" },
      { backends: undefined },
      { total: 7 },
      { total: undefined },
      { backends: { codex: { status: "ok", total: "1", skipped: 0 }, devin_acp: { status: "ok", total: 0, skipped: 0 } } },
      { limit: undefined },
      { truncated: undefined },
      { truncated: true },
    ];
    for (const broken of structuralCases) {
      const { env } = makeEnv(fixture, { taskList: { ...baseTaskList(), ...broken } });
      const response = await worker.fetch(
        request("/dash/api/v1/hosts/mac-main/sessions/project-a/tasks", fixture),
        env,
      );
      assert.equal(response.status, 503, JSON.stringify(broken));
      assert.equal((await response.json()).error_code, "host_projection_unavailable", JSON.stringify(broken));
    }
  });
});

test("pending summaries require a coherent fresh producer-owned projection", () => {
  const now = 1_800_000_000;
  const base = {
    state: "none",
    count: 0,
    types: [],
    summary_revision: 1,
    observed_at: now,
    producer_kind: "runtime_owner",
    producer_epoch: 1,
    expires_at: now + 30,
    truncated: false,
  };
  assert.equal(projectPendingInteraction(base, now).state, "none");
  const noneWithoutCount = { ...base };
  delete noneWithoutCount.count;
  assert.equal(projectPendingInteraction(noneWithoutCount, now).state, "none");
  const pendingWithoutCount = { ...base, state: "pending", types: ["permission"] };
  delete pendingWithoutCount.count;
  assert.equal(projectPendingInteraction(pendingWithoutCount, now).state, "pending");
  const unavailableWithoutCount = { ...base, state: "unavailable" };
  delete unavailableWithoutCount.count;
  assert.equal(projectPendingInteraction(unavailableWithoutCount, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, summary_revision: 0 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, producer_epoch: 0 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, expires_at: now + 31 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, state: "none", count: 1 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({
    state: "unavailable",
    summary_revision: 2,
    observed_at: now - 30,
    producer_kind: "runtime_owner",
    producer_epoch: 2,
    expires_at: now,
  }, now).state, "unavailable");
});
