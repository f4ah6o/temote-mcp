import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import { DatabaseSync } from "node:sqlite";
import worker from "../src/index.js";
import { EVENT_CATALOG, matches, validArguments } from "../src/events/catalog.js";
import { callbackUrl, canonicalJson, constantTimeEqual, grantedTtl, subscriptionId, validSecret } from "../src/events/validation.js";
import { recordTransition, repositoryReady } from "../src/events/repository.js";
import { deliveryDisposition, retryDelay, sweepEventOutbox } from "../src/events/outbox.js";
import { currentSession, eventsEligible, principalKey, principalStillAllowed, sessionIdentity } from "../src/events/service.js";

const secret = `whsec_${btoa("a".repeat(32))}`;
const meta = { "io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {} };

function database() {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec("PRAGMA foreign_keys = ON");
  sqlite.exec(fs.readFileSync(new URL("../migrations/0005_mcp_events.sql", import.meta.url), "utf8"));
  const wrap = (sql, params = []) => ({
    bind(...next) { return wrap(sql, next); },
    first() { return sqlite.prepare(sql).get(...params) ?? null; },
    all() { return { results: sqlite.prepare(sql).all(...params) }; },
    run() { const info = sqlite.prepare(sql).run(...params); return { meta: { changes: Number(info.changes) } }; },
  });
  return {
    prepare: (sql) => wrap(sql),
    async batch(statements) {
      sqlite.exec("BEGIN");
      try { const results = statements.map((statement) => statement.run()); sqlite.exec("COMMIT"); return results; }
      catch (error) { sqlite.exec("ROLLBACK"); throw error; }
    },
    sqlite,
  };
}

function fixture(db, session = {}, onInfo = () => {}) {
  const baseline = { started_at: 1700000000, process_id: 200, pid: 200, restart_count: 0,
    permission_mode: "agent", status: "active", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] };
  const host = { fetch: async (url, init) => {
    if (new URL(url).pathname === "/events/identity") return Response.json({ status: "registered", host_id: "host-a", instance_id: "host-instance-a", generation: 1 });
    const request = JSON.parse(init.body).request;
    assert.equal(request.params.name, "session_info");
    onInfo();
    return Response.json({ jsonrpc: "2.0", id: request.id, result: { content: [{ type: "text", text: JSON.stringify({ host_id: "host-a", session_id: "session-a", ...baseline, ...session }) }] } });
  } };
  return {
    OBSERVATION_DB: db,
    GATEWAY_SESSIONS: { idFromName: (name) => name, get: () => host },
    GATEWAY_DEPLOYMENT: { id: "test" },
    CLIENT_TOKEN: "client-token",
    EVENT_SENDER_URL: "https://sender.example.com/events/send",
    EVENT_SENDER_BEARER: "b".repeat(40),
    EVENT_SENDER_ACCESS_CLIENT_ID: "test-id",
    EVENT_SENDER_ACCESS_CLIENT_SECRET: "test-secret",
  };
}

function request(method, params = {}, modern = true) {
  const rpc = { jsonrpc: "2.0", id: "event-test", method, params: { ...params, ...(modern ? { _meta: meta } : {}) } };
  return new Request("https://fabric.test/mcp", { method: "POST", headers: {
    authorization: "Bearer client-token", "content-type": "application/json",
    ...(modern ? { "mcp-protocol-version": "2026-07-28", "mcp-method": method } : {}),
  }, body: JSON.stringify(rpc) });
}

const args = { host_id: "host-a", session_id: "session-a" };
const delivery = { mode: "webhook", url: "https://callback.example.com/events", secret };

test("memory-only and pre-migration deployments skip the Events sweep safely", async () => {
  let reads = 0;
  const missingSchema = {
    prepare() { reads += 1; throw new Error("no event tables"); },
    batch() { throw new Error("Events must not mutate unavailable schema"); },
  };
  await sweepEventOutbox({ OBSERVATION_DB: missingSchema });
  assert.equal(reads, 0);
  await sweepEventOutbox(fixture(missingSchema));
  assert.equal(reads, 1);
});

test("catalog, canonical identity, secret, URL, TTL and filters are strict", async () => {
  assert.deepEqual(EVENT_CATALOG.map((event) => event.name), ["job.state.changed", "session.state.changed"]);
  assert.ok(EVENT_CATALOG.every((event) => event.delivery.join() === "webhook" && event.inputSchema.additionalProperties === false));
  assert.equal(canonicalJson({ b: 2, a: { z: 1, y: 2 } }), canonicalJson({ a: { y: 2, z: 1 }, b: 2 }));
  assert.equal(await subscriptionId("p", "https://c", "job.state.changed", { b: 2, a: 1 }),
    await subscriptionId("p", "https://c", "job.state.changed", { a: 1, b: 2 }));
  assert.equal(validSecret(secret), true);
  assert.equal(validSecret(`whsec_${btoa("a".repeat(23))}`), false);
  assert.equal(validSecret("whsec_%%%"), false);
  assert.equal(callbackUrl("http://callback.example.com"), null);
  assert.equal(callbackUrl("https://localhost/"), null);
  assert.equal(callbackUrl("https://user@callback.example.com/"), null);
  assert.equal(callbackUrl("https://callback.example.com/events"), "https://callback.example.com/events");
  assert.equal(grantedTtl(null, true), 86_400_000);
  assert.equal(grantedTtl(9 * 86_400_000, true), 7 * 86_400_000);
  assert.equal(grantedTtl(0, true), null);
  assert.equal(constantTimeEqual("abc", "abc"), true);
  assert.equal(constantTimeEqual("abc", "abcd"), false);
  assert.equal(validArguments("job.state.changed", { ...args, job_id: "job-1" }), true);
  assert.equal(validArguments("job.state.changed", { ...args, extra: 1 }), false);
  assert.equal(eventsEligible({ subject: "access-user" }), false);
  assert.equal(matches({ name: "job.state.changed", host_id: "host-a", session_id: "session-a", instance_key: "i", job_id: "job-1" },
    { name: "job.state.changed", host_id: "host-a", session_id: "session-a", instance_key: "i", data: { job_id: "job-2" } }), false);
});

test("modern discovery advertises only with durable configured sender; legacy stays unchanged", async () => {
  const db = database(); const env = fixture(db);
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response(null, { status: 204 });
  try {
    assert.equal(await repositoryReady(env), true);
    let response = await (await worker.fetch(request("server/discover"), env)).json();
    assert.deepEqual(response.result.capabilities.events, {});
    response = await (await worker.fetch(request("events/list"), env)).json();
    assert.equal(response.result.events.length, 2);
    assert.equal(response.result.nextCursor, null);
    response = await (await worker.fetch(request("events/list", {}, false), env)).json();
    assert.equal(response.error.code, -32601);
    delete env.EVENT_SENDER_ACCESS_CLIENT_SECRET;
    response = await (await worker.fetch(request("server/discover"), env)).json();
    assert.equal(response.result.capabilities.events, undefined);
  } finally { globalThis.fetch = originalFetch; }
});

test("verified subscribe refreshes durably, rotates secret and unsubscribes idempotently", async () => {
  const db = database(); const env = fixture(db);
  const originalFetch = globalThis.fetch;
  let verifications = 0;
  globalThis.fetch = async (_url, init) => {
    assert.equal(init.redirect, "error");
    if (init.method === "GET") return new Response(null, { status: 204 });
    const envelope = JSON.parse(init.body);
    assert.equal(envelope.kind, "verification");
    verifications += 1;
    return Response.json({ status: 200, verified: true });
  };
  try {
    const params = { name: "job.state.changed", arguments: { ...args, job_id: "job-1" }, delivery, cursor: null, ttlMs: null };
    const first = (await (await worker.fetch(request("events/subscribe", params), env)).json()).result;
    assert.ok(first.refreshBefore && first.cursor === null);
    const otherSecret = `whsec_${btoa("b".repeat(32))}`;
    const restarted = fixture(db);
    const refreshed = (await (await worker.fetch(request("events/subscribe", {
      ...params, arguments: { job_id: "job-1", session_id: "session-a", host_id: "host-a" }, delivery: { ...delivery, secret: otherSecret },
    }), restarted)).json()).result;
    assert.equal(first.id, refreshed.id);
    const stored = db.sqlite.prepare("SELECT * FROM event_subscriptions").all();
    assert.equal(stored.length, 1);
    assert.equal(stored[0].secret, otherSecret);
    assert.equal(stored[0].previous_secret, secret);
    assert.equal(stored[0].rotate_until - stored[0].updated_at, 300_000);
    assert.equal(verifications, 2);
    const unsubscribe = { name: params.name, arguments: params.arguments, delivery: { mode: "webhook", url: delivery.url } };
    for (let count = 0; count < 2; count += 1) {
      assert.deepEqual((await (await worker.fetch(request("events/unsubscribe", unsubscribe), env)).json()).result.resultType, "complete");
    }
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_subscriptions").get().n, 0);
  } finally { globalThis.fetch = originalFetch; }
});

test("callback failures return categorized errors without persisting secrets", async () => {
  const db = database(); const env = fixture(db);
  const originalFetch = globalThis.fetch;
  const params = { name: "session.state.changed", arguments: args, delivery };
  try {
    globalThis.fetch = async (_url, init) => init.method === "GET"
      ? new Response(null, { status: 204 })
      : Response.json({ error: "invalid_address" }, { status: 400 });
    const invalid = await (await worker.fetch(request("events/subscribe", params), env)).json();
    assert.equal(invalid.error.code, -32015);
    assert.equal(invalid.error.data.reason, "invalid_address");
    assert.equal(JSON.stringify(invalid).includes(secret), false);
    globalThis.fetch = async (_url, init) => {
      if (init.method === "GET") return new Response(null, { status: 204 });
      throw new DOMException("timeout", "TimeoutError");
    };
    const timeout = await (await worker.fetch(request("events/subscribe", params), env)).json();
    assert.equal(timeout.error.data.reason, "timeout");
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_subscriptions").get().n, 0);
  } finally { globalThis.fetch = originalFetch; }
});

test("transition outbox is durable, filtered, deduplicated, fenced and bounded", async () => {
  const db = database(); const env = fixture(db);
  const now = Date.now();
  const instanceKey = sessionIdentity({ host_id: "host-a", session_id: "session-a", started_at: 1700000000, process_id: 200,
    restart_count: 0, permission_mode: "agent", status: "active", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] }, "host-a", "session-a");
  const base = ["sub_1", await principalKey({ subject: "client-token", email: "-" }, env), delivery.url, "job.state.changed", canonicalJson({ ...args, job_id: "job-1" }), "host-a", "session-a", "job-1", instanceKey, secret, now, now + 60_000, now];
  db.sqlite.prepare(`INSERT INTO event_subscriptions (id,principal,callback_url,name,arguments_json,host_id,session_id,job_id,instance_key,secret,verified_at,expires_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`).run(...base);
  const transition = { name: "job.state.changed", host_id: "host-a", session_id: "session-a", instance_key: instanceKey,
    data: { host_id: "host-a", session_id: "session-a", job_id: "job-2", state: "running", timestamp: new Date(now).toISOString() } };
  await recordTransition(db, transition, now);
  assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 0);
  transition.data.job_id = "job-1";
  await recordTransition(db, transition, now);
  await recordTransition(db, transition, now);
  assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 1);
  const row = db.sqlite.prepare("SELECT * FROM event_outbox").get();
  assert.equal(JSON.parse(row.event_body).eventId, row.event_id);
  assert.equal(JSON.parse(row.event_body).cursor, null);
  assert.equal(new TextEncoder().encode(row.event_body).byteLength <= 262_144, true);
  transition.data.state = "completed";
  await recordTransition(db, transition, now + 1);
  transition.data.state = "running";
  await recordTransition(db, transition, now + 2);
  assert.equal(db.sqlite.prepare("SELECT state FROM event_projections WHERE job_id = 'job-1'").get().state, "completed");
  assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 2);
  const offline = fixture(db, { started_at: 1700000001, process_id: 201, pid: 201, status: "active",
    cwd: "/workspace/project", permitted_directories: ["/workspace/project"] });
  await sweepEventOutbox(offline, now + 2);
  assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 0);
  assert.equal(deliveryDisposition(200), "accepted");
  assert.equal(deliveryDisposition(302), "terminal_rejected");
  assert.equal(deliveryDisposition(410), "terminal_rejected");
  assert.equal(deliveryDisposition(413), "terminal_rejected");
  assert.equal(deliveryDisposition(503), "retry");
  assert.equal(retryDelay(10) <= 3_600_000, true);
});

test("retained process and scope fence stop, replacement, degraded and token rotation", async () => {
  const db = database();
  const active = await currentSession(fixture(db), "host-a", "session-a");
  assert.ok(active?.instance_key);
  const stopped = await currentSession(fixture(db, { started_at: 1700000000, process_id: 200, pid: null,
    status: "stopped", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] }), "host-a", "session-a");
  assert.equal(stopped.instance_key, active.instance_key);
  const replaced = await currentSession(fixture(db, { started_at: 1700000001, process_id: 201, pid: 201,
    status: "active", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] }), "host-a", "session-a");
  assert.notEqual(replaced.instance_key, active.instance_key);
  const otherScope = await currentSession(fixture(db, { started_at: 1700000000, process_id: 200, pid: 200,
    status: "active", cwd: "/workspace/other", permitted_directories: ["/workspace/other"] }), "host-a", "session-a");
  assert.notEqual(otherScope.instance_key, active.instance_key);
  const windowsScope = await currentSession(fixture(db, { cwd: "C:\\Users\\workspace", permitted_directories: ["C:\\Users\\workspace"] }), "host-a", "session-a");
  assert.ok(windowsScope?.instance_key);
  assert.equal(await currentSession(fixture(db, { started_at: 1700000000, process_id: 200, pid: null,
    status: "unknown", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] }), "host-a", "session-a"), null);
  assert.equal(await currentSession(fixture(db, { started_at: 1700000000, process_id: undefined, pid: null,
    status: "stopped", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] }), "host-a", "session-a"), null);
  const env = fixture(db);
  const principal = await principalKey({ subject: "client-token", email: "-" }, env);
  assert.equal(principal.includes("client-token"), true);
  assert.notEqual(JSON.parse(principal)[1], env.CLIENT_TOKEN);
  assert.equal(await principalStillAllowed(principal, env), true);
  env.CLIENT_TOKEN = "new-token";
  assert.equal(await principalStillAllowed(principal, env), false);
});

test("authenticated host push fences host generation and complete session instance", async () => {
  const db = database(); const env = fixture(db, { started_at: 1700000000, process_id: 200, pid: null,
    status: "stopped", cwd: "/workspace/project", permitted_directories: ["/workspace/project"] });
  env.HOST_TOKENS_JSON = JSON.stringify({ "host-a": "host-token" });
  const transition = { name: "session.state.changed", session_id: "session-a", state: "stopped",
    timestamp: new Date().toISOString(), generation: 1, host_instance_id: "host-instance-a",
    session_started_at: 1700000000, session_process_id: 200, session_restart_count: 0,
    session_permission_mode: "agent", session_cwd: "/workspace/project",
    session_permitted_directories: ["/workspace/project"] };
  const push = (payload, token = "host-token") => worker.fetch(new Request("https://fabric.test/v1/hosts/host-a/events/transition", {
    method: "POST", headers: { authorization: `Bearer ${token}`, "x-temote-host-id": "host-a", "content-type": "application/json" },
    body: JSON.stringify(payload),
  }), env);
  assert.equal((await push(transition, "wrong")).status, 401);
  assert.equal((await push({ ...transition, generation: 2 })).status, 409);
  assert.equal((await push({ ...transition, host_instance_id: "replacement" })).status, 409);
  assert.equal((await push({ ...transition, session_cwd: "/workspace/other" })).status, 409);
  assert.equal((await push({ ...transition, state: "active" })).status, 200);
  assert.equal(db.sqlite.prepare("SELECT state FROM event_projections").get().state, "active");
  assert.equal((await push(transition)).status, 200);
  assert.equal(db.sqlite.prepare("SELECT state FROM event_projections").get().state, "stopped");
  assert.equal((await push({ ...transition, state: "active" })).status, 200);
  assert.equal(db.sqlite.prepare("SELECT state FROM event_projections").get().state, "stopped");
});

test("sender outage stays pending past eight sweeps, then recovery delivers once", async () => {
  const db = database(); const env = fixture(db); const now = Date.now();
  const instance = (await currentSession(env, "host-a", "session-a")).instance_key;
  const principal = await principalKey({ subject: "client-token", email: "-" }, env);
  db.sqlite.prepare(`INSERT INTO event_subscriptions (id,principal,callback_url,name,arguments_json,host_id,session_id,job_id,instance_key,secret,verified_at,expires_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`)
    .run("sub_1", principal, delivery.url, "job.state.changed", canonicalJson(args), "host-a", "session-a", null, instance, secret, now, now + 86_400_000, now);
  await recordTransition(db, { name: "job.state.changed", host_id: "host-a", session_id: "session-a", instance_key: instance,
    data: { host_id: "host-a", session_id: "session-a", job_id: "job-1", state: "running", timestamp: new Date(now).toISOString() } }, now);
  const originalFetch = globalThis.fetch;
  let deliveries = 0;
  globalThis.fetch = async () => { throw new Error("sender offline"); };
  try {
    for (let attempt = 0; attempt < 10; attempt += 1) await sweepEventOutbox(env, now + attempt * 60_000);
    assert.equal(db.sqlite.prepare("SELECT attempt_count FROM event_outbox").get().attempt_count, 0);
    globalThis.fetch = async (_url, init) => { deliveries += 1; assert.equal(JSON.parse(init.body).eventBody.length > 0, true); return Response.json({ status: 200, verified: false }); };
    await sweepEventOutbox(fixture(db), now + 10 * 60_000);
    assert.equal(deliveries, 1);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 0);
  } finally { globalThis.fetch = originalFetch; }
});

test("leased delivery rechecks unsubscribe, rotation and credential revocation", async () => {
  const db = database(); const now = Date.now();
  const initial = fixture(db);
  const instance = (await currentSession(initial, "host-a", "session-a")).instance_key;
  const principal = await principalKey({ subject: "client-token", email: "-" }, initial);
  const seed = (id, jobId) => {
    db.sqlite.prepare(`INSERT INTO event_subscriptions (id,principal,callback_url,name,arguments_json,host_id,session_id,job_id,instance_key,secret,verified_at,expires_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`)
      .run(id, principal, delivery.url, "job.state.changed", canonicalJson({ ...args, job_id: jobId }), "host-a", "session-a", jobId, instance, secret, now, now + 86_400_000, now);
    db.sqlite.prepare(`INSERT INTO event_outbox (delivery_id,subscription_id,event_id,event_body,host_id,session_id,instance_key,next_attempt_at,expires_at,created_at) VALUES (?,?,?,?,?,?,?,?,?,?)`)
      .run(`${id}:evt`, id, `evt_${"a".repeat(64)}`, JSON.stringify({ eventId: `evt_${"a".repeat(64)}`, name: "job.state.changed", timestamp: new Date(now).toISOString(),
        data: { host_id: "host-a", session_id: "session-a", job_id: jobId, previous_state: null, state: "running", timestamp: new Date(now).toISOString() }, cursor: null }),
      "host-a", "session-a", instance, now, now + 86_400_000, now);
  };
  seed("sub_unsubscribe", "job-a");
  let once = false;
  const unsubscribeEnv = fixture(db, undefined, () => {
    if (once) return; once = true;
    db.sqlite.prepare("DELETE FROM event_subscriptions WHERE id = ?").run("sub_unsubscribe");
  });
  const originalFetch = globalThis.fetch;
  let sent = 0;
  globalThis.fetch = async (_url, init) => { sent += 1; return Response.json({ status: 200, verified: false }); };
  try {
    await sweepEventOutbox(unsubscribeEnv, now);
    assert.equal(sent, 0);
    seed("sub_rotate", "job-b");
    const replacementSecret = `whsec_${btoa("b".repeat(32))}`;
    once = false;
    const rotateEnv = fixture(db, undefined, () => {
      if (once) return; once = true;
      db.sqlite.prepare("UPDATE event_subscriptions SET previous_secret = secret, secret = ?, rotate_until = ? WHERE id = ?")
        .run(replacementSecret, now + 300_000, "sub_rotate");
    });
    globalThis.fetch = async (_url, init) => {
      sent += 1;
      const envelope = JSON.parse(init.body);
      assert.equal(envelope.secret, replacementSecret);
      assert.equal(envelope.previousSecret, secret);
      return Response.json({ status: 200, verified: false });
    };
    await sweepEventOutbox(rotateEnv, now);
    assert.equal(sent, 1);
    seed("sub_rebound", "job-rebound");
    once = false;
    const rebound = fixture(db, undefined, () => {
      if (once) return; once = true;
      db.sqlite.prepare("UPDATE event_subscriptions SET instance_key = ? WHERE id = ?")
        .run("replacement-instance", "sub_rebound");
    });
    await sweepEventOutbox(rebound, now);
    assert.equal(sent, 1);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox WHERE subscription_id = 'sub_rebound'").get().n, 0);
    assert.equal(db.sqlite.prepare("SELECT instance_key FROM event_subscriptions WHERE id = 'sub_rebound'").get().instance_key, "replacement-instance");
    seed("sub_revoke", "job-c");
    const revoked = fixture(db); revoked.CLIENT_TOKEN = "rotated-token";
    await sweepEventOutbox(revoked, now);
    assert.equal(sent, 1);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_subscriptions WHERE id = 'sub_revoke'").get().n, 0);
  } finally { globalThis.fetch = originalFetch; }
});

test("offline Host holds entries; 410 and 413 terminate; expiry retires rotation", async () => {
  const db = database(); const online = fixture(db); const now = Date.now();
  const instance = (await currentSession(online, "host-a", "session-a")).instance_key;
  const principal = await principalKey({ subject: "client-token", email: "-" }, online);
  for (let index = 0; index < 2; index += 1) {
    const subId = `sub_terminal_${index}`;
    db.sqlite.prepare(`INSERT INTO event_subscriptions (id,principal,callback_url,name,arguments_json,host_id,session_id,job_id,instance_key,secret,previous_secret,rotate_until,verified_at,expires_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`)
      .run(subId, principal, delivery.url, "job.state.changed", canonicalJson(args), "host-a", "session-a", null,
        instance, secret, secret, now + 30_000, now, now + 120_000, now);
    const eventId = `evt_${String(index).repeat(64)}`;
    db.sqlite.prepare(`INSERT INTO event_outbox (delivery_id,subscription_id,event_id,event_body,host_id,session_id,instance_key,next_attempt_at,expires_at,created_at) VALUES (?,?,?,?,?,?,?,?,?,?)`)
      .run(`${subId}:evt`, subId, eventId, JSON.stringify({ eventId, name: "job.state.changed", timestamp: new Date(now).toISOString(),
        data: { host_id: "host-a", session_id: "session-a", job_id: `job-${index}`, previous_state: null, state: "running", timestamp: new Date(now).toISOString() }, cursor: null }),
      "host-a", "session-a", instance, now, now + 120_000, now);
  }
  const offline = fixture(db);
  offline.GATEWAY_SESSIONS = { idFromName: (name) => name, get: () => ({ fetch: async () => new Response(null, { status: 404 }) }) };
  await sweepEventOutbox(offline, now);
  assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox WHERE attempt_count = 0").get().n, 2);
  const originalFetch = globalThis.fetch;
  let status = 410;
  globalThis.fetch = async (_url, init) => {
    assert.equal(JSON.parse(init.body).previousSecret, undefined);
    const current = status; status = 413;
    return Response.json({ status: current, verified: false });
  };
  try {
    await sweepEventOutbox(online, now + 60_000);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_outbox").get().n, 0);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_subscriptions WHERE previous_secret IS NOT NULL").get().n, 0);
    await sweepEventOutbox(online, now + 120_000);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM event_subscriptions").get().n, 0);
  } finally { globalThis.fetch = originalFetch; }
});
