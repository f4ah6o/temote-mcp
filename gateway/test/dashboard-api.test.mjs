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

function d1Fixture(rows = []) {
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
    OBSERVATION_DB: d1Fixture(d1Rows),
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
