import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";

import worker from "../src/index.js";

const TEXT_ENCODER = new TextEncoder();
const FIXTURE_TOKEN = "membership-token-fixture-do-not-render";
const RAW_MARKER = "raw-observation-fixture-do-not-render";

function base64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function makeAccessFixture() {
  const team = `dashboard-scope-${randomUUID()}.cloudflareaccess.com`;
  const audience = "dashboard-scoping-test-audience";
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5",
    modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  publicKey.kid = "dashboard-scoping-fixture-key";
  publicKey.alg = "RS256";
  publicKey.use = "sig";

  const claims = {
    iss: `https://${team}`,
    aud: audience,
    exp: Math.floor(Date.now() / 1000) + 3600,
    nbf: Math.floor(Date.now() / 1000) - 1,
    sub: "dashboard-scope-owner",
    email: "operator@example.test",
  };
  const header = base64url(TEXT_ENCODER.encode(JSON.stringify({
    alg: "RS256",
    kid: publicKey.kid,
    typ: "JWT",
  })));
  const payload = base64url(TEXT_ENCODER.encode(JSON.stringify(claims)));
  const signingInput = `${header}.${payload}`;
  const signature = await crypto.subtle.sign(
    { name: "RSASSA-PKCS1-v1_5" },
    keyPair.privateKey,
    TEXT_ENCODER.encode(signingInput),
  );

  return {
    team,
    audience,
    assertion: `${signingInput}.${base64url(signature)}`,
    publicKey,
  };
}

async function withJwks(fixture, action) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input) => {
    assert.equal(String(input), `https://${fixture.team}/cdn-cgi/access/certs`);
    return new Response(JSON.stringify({ keys: [fixture.publicKey] }), {
      headers: { "content-type": "application/json" },
    });
  };
  try {
    return await action();
  } finally {
    globalThis.fetch = originalFetch;
  }
}

function request(path, assertion) {
  return new Request(`https://fabric.example.test${path}`, {
    headers: { "cf-access-jwt-assertion": assertion },
  });
}

function dashboardEnv(fixture, options = {}) {
  const membership = options.membership === undefined
    ? { "host-a": FIXTURE_TOKEN, "host-b": "host-b-token-fixture" }
    : options.membership;
  const env = {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    CLIENT_TOKEN: "client-token-fixture-do-not-render",
    HOST_TOKENS_JSON: typeof membership === "string" ? membership : JSON.stringify(membership),
    OBSERVATION_OWNER_ID: "owner-a",
    GATEWAY_REGISTRY: options.registry.namespace,
    GATEWAY_SESSIONS: options.sessions.namespace,
    OBSERVATION_DB: options.d1,
  };
  if (options.membership === null) delete env.HOST_TOKENS_JSON;
  return env;
}

function registryHost(hostId, overrides = {}) {
  const now = Math.floor(Date.now() / 1000);
  return {
    host_id: hostId,
    instance_id: `instance-${hostId}`,
    generation: 3,
    expires_at: now + 60,
    connected_at: now - 30,
    last_seen: now - 1,
    platform: "linux",
    ...overrides,
  };
}

function replicaRow(ownerId, hostId, sessionId = "session-a", overrides = {}) {
  return {
    owner_id: ownerId,
    host_id: hostId,
    session_id: sessionId,
    source_base_revision: 0,
    source_head_revision: 7,
    acked_through_revision: 6,
    cloud_head_seq: 13,
    journal_degraded: 0,
    gap_count: 0,
    last_synced_at: "2026-09-29T00:00:00.000Z",
    ...overrides,
  };
}

function observationRow(ownerId, hostId, sessionId, cloudSeq, overrides = {}) {
  return {
    owner_id: ownerId,
    host_id: hostId,
    session_id: sessionId,
    cloud_seq: cloudSeq,
    source_revision: cloudSeq,
    kind: "operation_accepted",
    action: "task_start",
    target_backend: "codex",
    content_kind: "text",
    state_status: "accepted",
    state_revision: cloudSeq,
    observed_at: "2026-09-29T00:00:00.000Z",
    ingested_at: "2026-09-29T00:00:01.000Z",
    // These fields represent sensitive source-side data that is deliberately
    // absent from the dashboard's timeline SELECT list and response projection.
    content_preview: RAW_MARKER,
    evidence_refs: JSON.stringify([{ kind: "stdout", ref: RAW_MARKER }]),
    prompt: RAW_MARKER,
    stdout: RAW_MARKER,
    argv: ["--token", FIXTURE_TOKEN],
    environment: { TEMOTE_MCP_TOKEN: FIXTURE_TOKEN },
    credential: FIXTURE_TOKEN,
    ...overrides,
  };
}

class FakeD1 {
  constructor({ rows = [], observations = [], failAll = false, failFirst = false } = {}) {
    this.rows = rows;
    this.observations = observations;
    this.failAll = failAll;
    this.failFirst = failFirst;
    this.calls = [];
  }

  prepare(sql) {
    const call = { sql, binds: null, method: null };
    this.calls.push(call);
    return {
      bind: (...binds) => {
        call.binds = binds;
        return {
          all: async () => {
            call.method = "all";
            if (this.failAll) throw new Error("D1 fixture unavailable");
            if (/\bFROM\s+observations\b/i.test(sql)) {
              const [ownerId, hostId, sessionId] = binds;
              const scoped = this.observations.filter((row) => row.owner_id === ownerId
                && row.host_id === hostId && row.session_id === sessionId);
              const afterMatch = /cloud_seq\s*>\s*\?/i.test(sql);
              const after = afterMatch ? Number(binds[3]) : null;
              const candidates = scoped.filter((row) => after === null || row.cloud_seq > after);
              const descending = /ORDER\s+BY\s+cloud_seq\s+DESC/i.test(sql);
              candidates.sort((left, right) => descending
                ? right.cloud_seq - left.cloud_seq
                : left.cloud_seq - right.cloud_seq);
              const limit = Number(binds.at(-1));
              const limited = Number.isSafeInteger(limit) && limit > 0
                ? candidates.slice(0, limit)
                : candidates;
              // Keep raw fixture fields here on purpose. The dashboard query and
              // projection must ensure they never cross the response boundary.
              return { results: structuredClone(limited) };
            }
            const ownerId = binds[0];
            const hostIds = binds.slice(1);
            const results = this.rows
              .filter((row) => row.owner_id === ownerId && hostIds.includes(row.host_id))
              .map(({ owner_id: _owner, ...row }) => structuredClone(row));
            return { results };
          },
          first: async () => {
            call.method = "first";
            if (this.failFirst) throw new Error("D1 fixture unavailable");
            const [ownerId, hostId, sessionId] = binds;
            const row = this.rows.find((candidate) => candidate.owner_id === ownerId
              && candidate.host_id === hostId && candidate.session_id === sessionId);
            if (!row) return null;
            const { owner_id: _owner, ...selected } = row;
            return structuredClone(selected);
          },
        };
      },
    };
  }
}

function fakeBindings({ registryRows = [], registryFailure = false, status = {}, toolValues = {}, d1 } = {}) {
  const calls = {
    registry: 0,
    status: [],
    dispatch: [],
  };
  const registryStub = {
    fetch: async (input) => {
      calls.registry += 1;
      assert.equal(String(input), "https://registry.internal/hosts");
      if (registryFailure) throw new Error("registry fixture unavailable");
      return new Response(JSON.stringify(registryRows), {
        headers: { "content-type": "application/json" },
      });
    },
  };
  const registryNamespace = {
    idFromName: (name) => {
      assert.equal(name, "global");
      return name;
    },
    get: (id) => {
      assert.equal(id, "global");
      return registryStub;
    },
  };
  const sessionsNamespace = {
    idFromName: (name) => name,
    get: (id) => ({
      fetch: async (input, init = {}) => {
        const url = new URL(input);
        const hostId = id.startsWith("host:") ? id.slice("host:".length) : id;
        if (url.pathname === "/status") {
          calls.status.push(hostId);
          const configuredStatus = Object.hasOwn(status, hostId) ? status[hostId] : 200;
          if (configuredStatus === "throw") throw new Error("host probe fixture unavailable");
          return new Response(null, { status: configuredStatus });
        }
        if (url.pathname === "/dispatch") {
          const envelope = JSON.parse(init.body);
          const rpc = envelope.request;
          const name = rpc.params?.name;
          const args = rpc.params?.arguments ?? {};
          calls.dispatch.push({ hostId, name, arguments: args });
          const defaultValues = {
            session_list: [],
            session_info: { session_id: args.session_id, status: "active" },
          };
          const value = Object.hasOwn(toolValues, name) ? toolValues[name] : defaultValues[name] ?? [];
          return new Response(JSON.stringify({
            jsonrpc: "2.0",
            id: rpc.id,
            result: { content: [{ type: "text", text: JSON.stringify(value) }] },
          }), { headers: { "content-type": "application/json" } });
        }
        throw new Error(`unexpected host fixture path ${url.pathname}`);
      },
    }),
  };
  return {
    calls,
    registry: { namespace: registryNamespace },
    sessions: { namespace: sessionsNamespace },
    d1,
  };
}

async function dashboardJson(fixture, env, path) {
  const response = await worker.fetch(request(path, fixture.assertion), env);
  return { response, body: await response.json() };
}

test("configured membership keeps pruned hosts visible and ignores stale foreign replica rows", async () => {
  const fixture = await makeAccessFixture();
  const d1 = new FakeD1({ rows: [
    replicaRow("owner-a", "host-a", "session-a", { raw_body: RAW_MARKER, credential: FIXTURE_TOKEN }),
    replicaRow("owner-b", "host-a"),
    replicaRow("owner-a", "removed-host"),
  ] });
  const adapters = fakeBindings({
    registryRows: [registryHost("removed-host")],
    d1,
  });
  const env = dashboardEnv(fixture, adapters);

  await withJwks(fixture, async () => {
    const { response, body } = await dashboardJson(fixture, env, "/dash/api/v1/hosts");
    assert.equal(response.status, 200);
    assert.equal(body.status, "confirmed");
    assert.equal(body.data.components.membership.status, "confirmed");
    assert.deepEqual(body.data.hosts.map((host) => host.host_id), ["host-a", "host-b"]);
    for (const [index, host] of body.data.hosts.entries()) {
      assert.equal(host.availability, "offline");
      assert.deepEqual(host.evidence, index === 0
        ? ["configured_membership", "fabric_replica"]
        : ["configured_membership"]);
      assert.deepEqual(host.connection_history, { status: "unknown" });
    }
    assert.equal(body.data.hosts[0].replica.source_head_revision, 7);
    assert.equal(JSON.stringify(body).includes(RAW_MARKER), false);
    assert.equal(JSON.stringify(body).includes(FIXTURE_TOKEN), false);
  });

  assert.equal(adapters.calls.registry, 1);
  assert.deepEqual(adapters.calls.status, []);
  assert.deepEqual(d1.calls.map((call) => call.binds), [["owner-a", "host-a", "host-b"]]);
  assert.match(d1.calls[0].sql, /owner_id\s*=\s*\?/);
  assert.match(d1.calls[0].sql, /host_id\s+IN\s*\(/);
  assert.equal(d1.calls[0].binds.includes(FIXTURE_TOKEN), false);
});

test("invalid or missing membership is unavailable and does not become an empty inventory", async () => {
  const fixture = await makeAccessFixture();
  for (const membership of [null, "{broken", JSON.stringify({ "host-a": "" })]) {
    const d1 = new FakeD1();
    const adapters = fakeBindings({ registryRows: [registryHost("host-a")], d1 });
    const env = dashboardEnv(fixture, { ...adapters, membership });
    await withJwks(fixture, async () => {
      const { response, body } = await dashboardJson(fixture, env, "/dash/api/v1/hosts");
      assert.equal(response.status, 503);
      assert.equal(body.status, "unavailable");
      assert.equal(body.error_code, "membership_unavailable");
      assert.equal(body.data.components.membership.status, "unavailable");
      assert.equal(Object.hasOwn(body.data, "hosts"), false);
    });
    assert.equal(adapters.calls.registry, 0);
    assert.equal(adapters.calls.status.length, 0);
    assert.equal(d1.calls.length, 0);
  }
});

test("registry read or completeness failures keep configured inventory unknown", async () => {
  const fixture = await makeAccessFixture();
  const invalidRows = [registryHost("host-a", { last_seen: "not-a-timestamp" })];
  for (const options of [
    { registryFailure: true },
    { registryRows: invalidRows },
  ]) {
    const d1 = new FakeD1({ rows: [replicaRow("owner-a", "host-a")] });
    const adapters = fakeBindings({ ...options, d1 });
    const env = dashboardEnv(fixture, adapters);
    await withJwks(fixture, async () => {
      const { response, body } = await dashboardJson(fixture, env, "/dash/api/v1/hosts");
      assert.equal(response.status, 200);
      assert.equal(body.status, "stale");
      assert.equal(body.data.components.liveness.status, "unavailable");
      assert.equal(body.data.hosts.length, 2);
      assert.equal(body.data.hosts[0].availability, "unknown");
      assert.deepEqual(body.data.hosts[0].connection_history, { status: "unavailable" });
      assert.equal(body.data.hosts[0].replica.source_head_revision, 7);
    });
    assert.equal(adapters.calls.registry, 1);
    assert.deepEqual(adapters.calls.status, []);
  }
});

test("a failed host liveness probe is unknown rather than offline", async () => {
  const fixture = await makeAccessFixture();
  const d1 = new FakeD1({ rows: [replicaRow("owner-a", "host-a")] });
  const adapters = fakeBindings({
    registryRows: [registryHost("host-a")],
    status: { "host-a": "throw" },
    d1,
  });
  const env = dashboardEnv(fixture, adapters);

  await withJwks(fixture, async () => {
    const { response, body } = await dashboardJson(fixture, env, "/dash/api/v1/hosts");
    assert.equal(response.status, 200);
    assert.equal(body.status, "stale");
    assert.equal(body.data.components.liveness.status, "unavailable");
    assert.equal(body.data.hosts[0].availability, "unknown");
    assert.equal(body.data.hosts[0].connection_history.status, "confirmed");
    assert.equal(body.data.hosts[0].replica.source_head_revision, 7);
  });

  assert.deepEqual(adapters.calls.status, ["host-a"]);
});

test("D1 failure does not downgrade a host with confirmed live route", async () => {
  const fixture = await makeAccessFixture();
  const d1 = new FakeD1({ failAll: true });
  const adapters = fakeBindings({ registryRows: [registryHost("host-a")], d1 });
  const env = dashboardEnv(fixture, adapters);

  await withJwks(fixture, async () => {
    const { response, body } = await dashboardJson(fixture, env, "/dash/api/v1/hosts");
    assert.equal(response.status, 200);
    assert.equal(body.status, "stale");
    assert.equal(body.data.components.liveness.status, "confirmed");
    assert.equal(body.data.components.replica.status, "unavailable");
    assert.equal(body.data.hosts[0].host_id, "host-a");
    assert.equal(body.data.hosts[0].availability, "online");
    assert.equal(body.data.hosts[0].replica.status, "unavailable");
    assert.ok(body.data.hosts[0].evidence.includes("live_route"));
    assert.equal(body.data.hosts[1].availability, "offline");
  });

  assert.deepEqual(adapters.calls.status, ["host-a"]);
  assert.equal(d1.calls.length, 1);
});

test("host-scoped routes reject nonmembers before registry, host, or D1 reads", async () => {
  const fixture = await makeAccessFixture();
  const d1 = new FakeD1({ rows: [replicaRow("owner-a", "outside-host")] });
  const adapters = fakeBindings({ registryRows: [registryHost("outside-host")], d1 });
  const env = dashboardEnv(fixture, { ...adapters, membership: { "host-a": FIXTURE_TOKEN } });
  const paths = [
    "/dash/api/v1/hosts/outside-host/sessions",
    "/dash/api/v1/hosts/outside-host/sessions/session-a",
    "/dash/api/v1/hosts/outside-host/sessions/session-a/tasks",
    "/dash/api/v1/hosts/outside-host/sessions/session-a/context",
    "/dash/api/v1/hosts/outside-host/sessions/session-a/timeline",
  ];

  await withJwks(fixture, async () => {
    for (const path of paths) {
      const { response, body } = await dashboardJson(fixture, env, path);
      assert.equal(response.status, 404, path);
      assert.equal(body.status, "unavailable", path);
    }
  });

  assert.equal(adapters.calls.registry, 0);
  assert.deepEqual(adapters.calls.status, []);
  assert.deepEqual(adapters.calls.dispatch, []);
  assert.equal(d1.calls.length, 0);
});

test("context live projections and replica metadata stay session-scoped and omit raw host data", async () => {
  const fixture = await makeAccessFixture();
  const sourceRows = [
    replicaRow("owner-a", "host-a", "session-a", { raw_body: RAW_MARKER, credential: FIXTURE_TOKEN }),
    replicaRow("owner-b", "host-a", "session-a"),
    replicaRow("owner-a", "host-b", "session-a"),
    replicaRow("owner-a", "host-a", "other-session"),
  ];
  const d1 = new FakeD1({ rows: sourceRows });
  const resolve = {
    session_id: "session-a",
    context_schema_version: 1,
    workspace: { repository: "repo-a", branch: "private-branch", task: RAW_MARKER },
    current_summary: {
      observations: 2,
      journal_revision: 7,
      tasks_total: 1,
      tasks_active: 1,
      backends: ["codex", "unknown-backend"],
      last_observed_at: "2026-09-29T00:00:00.000Z",
      private_detail: RAW_MARKER,
    },
    unresolved: [{
      task_id: "task-a",
      status: "active",
      reason: "latest observed state needs attention",
      refs: [{ observation_id: "obs-a", revision: 2, kind: "execution_state", body: RAW_MARKER }],
      prompt: RAW_MARKER,
    }],
    recent_related_tasks: [{
      task_id: "task-a",
      backend: "codex",
      instruction: {
        revision: 1,
        operation_id: "operation-a",
        observed_at: "2026-09-29T00:00:00.000Z",
        actor: { transport: "mcp", token: RAW_MARKER },
        prompt: RAW_MARKER,
      },
      state: {
        revision: 2,
        status: "done",
        execution_id: "execution-a",
        output: RAW_MARKER,
      },
      raw: RAW_MARKER,
    }],
    refs: [{ observation_id: "obs-a", revision: 2, kind: "execution_state", body: RAW_MARKER }],
    freshness: { resolved_revision: 7, at_least_revision: null, stale: false, detail: RAW_MARKER },
    partial: { journal_exists: true, journal_degraded: false, corrupt_lines: 0, write_failures: 0, raw: RAW_MARKER },
    memory: { worker: "memory-v1", stale: false, credential: FIXTURE_TOKEN },
    generated_at: 1_790_000_000,
    raw_observation: RAW_MARKER,
  };
  const contextStatus = {
    session_id: "session-a",
    journal: {
      schema_version: 1,
      exists: true,
      revision: 7,
      base_revision: 0,
      observations: 2,
      bytes: 512,
      max_bytes: 4096,
      compactions: 1,
      write_failures: 0,
      corrupt_lines: 0,
      degraded: false,
      raw: RAW_MARKER,
    },
    memory: { worker: "memory-v1", stale: false, raw: RAW_MARKER },
    stdout: RAW_MARKER,
  };
  const adapters = fakeBindings({
    registryRows: [registryHost("host-a")],
    d1,
    toolValues: {
      session_list: [{ session_id: "session-a", status: "active" }],
      context_resolve: resolve,
      context_status: contextStatus,
    },
  });
  const env = dashboardEnv(fixture, adapters);

  await withJwks(fixture, async () => {
    const { response, body } = await dashboardJson(
      fixture,
      env,
      "/dash/api/v1/hosts/host-a/sessions/session-a/context",
    );
    assert.equal(response.status, 200);
    assert.equal(body.status, "confirmed");
    assert.equal(body.data.context_resolve.status, "confirmed");
    assert.equal(body.data.context_status.status, "confirmed");
    assert.equal(body.data.context_resolve.data.session_id, "session-a");
    assert.equal(body.data.context_resolve.data.current_summary.journal_revision, 7);
    assert.deepEqual(body.data.context_resolve.data.current_summary.backends, ["codex"]);
    assert.equal(body.data.context_status.data.journal.revision, 7);
    assert.equal(body.data.replica.status, "confirmed");
    assert.equal(body.data.replica.data.source_head_revision, 7);
    assert.equal(JSON.stringify(body).includes(RAW_MARKER), false);
    assert.equal(JSON.stringify(body).includes(FIXTURE_TOKEN), false);
  });

  assert.deepEqual(adapters.calls.dispatch.map((call) => call.name), [
    "session_list",
    "context_resolve",
    "context_status",
  ]);
  assert.deepEqual(adapters.calls.dispatch.map((call) => call.hostId), ["host-a", "host-a", "host-a"]);
  assert.deepEqual(adapters.calls.status, ["host-a"]);
  assert.equal(adapters.calls.registry, 1);
  assert.equal(d1.calls.length, 1);
  assert.equal(d1.calls[0].method, "first");
  assert.match(d1.calls[0].sql, /owner_id\s*=\s*\?\s+AND\s+host_id\s*=\s*\?\s+AND\s+session_id\s*=\s*\?/);
  assert.deepEqual(d1.calls[0].binds, ["owner-a", "host-a", "session-a"]);
});

test("timeline uses exact owner-host-session scope, bounded allow-listed events, and scope-checked cursors", async () => {
  const fixture = await makeAccessFixture();
  const d1 = new FakeD1({
    rows: [
      replicaRow("owner-a", "host-a", "session-a", { cloud_head_seq: 3 }),
      replicaRow("owner-b", "host-a", "session-a", { cloud_head_seq: 99 }),
      replicaRow("owner-a", "host-b", "session-a", { cloud_head_seq: 98 }),
      replicaRow("owner-a", "host-a", "other-session", { cloud_head_seq: 97 }),
    ],
    observations: [
      observationRow("owner-a", "host-a", "session-a", 1),
      observationRow("owner-a", "host-a", "session-a", 2),
      observationRow("owner-b", "host-a", "session-a", 99),
      observationRow("owner-a", "host-b", "session-a", 98),
      observationRow("owner-a", "host-a", "other-session", 97),
    ],
  });
  const adapters = fakeBindings({ d1 });
  const env = dashboardEnv(fixture, adapters);

  let cursor;
  await withJwks(fixture, async () => {
    const first = await dashboardJson(
      fixture,
      env,
      "/dash/api/v1/hosts/host-a/sessions/session-a/timeline?limit=1",
    );
    assert.equal(first.response.status, 200);
    assert.equal(first.body.data.events.length, 1);
    assert.equal(first.body.data.events[0].cloud_seq, 2);
    assert.equal(first.body.data.events[0].kind, "operation_accepted");
    assert.equal(first.body.data.events[0].content_kind, "text");
    assert.equal(first.body.data.has_older, true);
    assert.equal(first.body.data.source.cloud_head_seq, 3);
    assert.equal(JSON.stringify(first.body).includes(RAW_MARKER), false);
    assert.equal(JSON.stringify(first.body).includes(FIXTURE_TOKEN), false);
    cursor = first.body.data.next_cursor;
    assert.equal(typeof cursor, "string");

    const timelineQuery = d1.calls.find((call) => call.method === "all");
    assert.ok(timelineQuery);
    assert.match(timelineQuery.sql, /FROM\s+observations\s+WHERE\s+owner_id\s*=\s*\?\s+AND\s+host_id\s*=\s*\?\s+AND\s+session_id\s*=\s*\?/);
    assert.match(timelineQuery.sql, /ORDER BY cloud_seq DESC LIMIT \?/);
    assert.doesNotMatch(timelineQuery.sql, /content_preview|content_digest|content_ref|evidence_refs|prompt|stdout|stderr|argv|environment|credential|token/i);
    assert.deepEqual(timelineQuery.binds, ["owner-a", "host-a", "session-a", 2]);
    assert.deepEqual(d1.calls.map((call) => call.binds), [
      ["owner-a", "host-a", "session-a"],
      ["owner-a", "host-a", "session-a", 2],
    ]);

    d1.observations.push(observationRow("owner-a", "host-a", "session-a", 3));
    const incremental = await dashboardJson(
      fixture,
      env,
      `/dash/api/v1/hosts/host-a/sessions/session-a/timeline?after=${encodeURIComponent(cursor)}&limit=2`,
    );
    assert.equal(incremental.response.status, 200);
    assert.deepEqual(incremental.body.data.events.map((event) => event.cloud_seq), [3]);
    assert.equal(incremental.body.data.has_more, false);
    assert.equal(JSON.stringify(incremental.body).includes(RAW_MARKER), false);

    const tamperedPayload = JSON.parse(Buffer.from(cursor, "base64url").toString("utf8"));
    tamperedPayload.after += 1;
    const tamperedCursor = Buffer.from(JSON.stringify(tamperedPayload)).toString("base64url");
    const tampered = await dashboardJson(
      fixture,
      env,
      `/dash/api/v1/hosts/host-a/sessions/session-a/timeline?after=${encodeURIComponent(tamperedCursor)}`,
    );
    assert.equal(tampered.response.status, 400);
    assert.equal(tampered.body.error_code, "invalid_cursor");

    for (const path of [
      `/dash/api/v1/hosts/host-b/sessions/session-a/timeline?after=${encodeURIComponent(cursor)}`,
      `/dash/api/v1/hosts/host-a/sessions/other-session/timeline?after=${encodeURIComponent(cursor)}`,
    ]) {
      const crossScope = await dashboardJson(fixture, env, path);
      assert.equal(crossScope.response.status, 400, path);
      assert.equal(crossScope.body.error_code, "invalid_cursor", path);
      assert.equal(JSON.stringify(crossScope.body).includes(RAW_MARKER), false);
    }
  });

  assert.equal(adapters.calls.registry, 0, "replicated timeline should remain available without host liveness");
  assert.deepEqual(adapters.calls.status, []);
  const observationQueries = d1.calls.filter((call) => /FROM\s+observations\b/i.test(call.sql));
  assert.equal(observationQueries.length, 2, "invalid cursors must be rejected before reading observations");
  assert.deepEqual(d1.calls.map((call) => call.binds), [
    ["owner-a", "host-a", "session-a"],
    ["owner-a", "host-a", "session-a", 2],
    ["owner-a", "host-a", "session-a"],
    ["owner-a", "host-a", "session-a", 2, 3],
    ["owner-a", "host-a", "session-a"],
    ["owner-a", "host-b", "session-a"],
    ["owner-a", "host-a", "other-session"],
  ]);
});
