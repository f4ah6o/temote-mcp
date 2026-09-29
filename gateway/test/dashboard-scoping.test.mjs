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

class FakeD1 {
  constructor({ rows = [], failAll = false, failFirst = false } = {}) {
    this.rows = rows;
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

function fakeBindings({ registryRows = [], registryFailure = false, status = {}, d1 } = {}) {
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
          calls.dispatch.push({ hostId, name: rpc.params?.name, arguments: rpc.params?.arguments });
          return new Response(JSON.stringify({
            jsonrpc: "2.0",
            id: rpc.id,
            result: { content: [{ type: "text", text: JSON.stringify([]) }] },
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
