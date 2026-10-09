import assert from "node:assert/strict";
import fs from "node:fs";
import { DatabaseSync } from "node:sqlite";
import test from "node:test";

import worker from "../src/index.js";
import { authorizeBrowserHost, handleEnrollmentRequest, sweepHostGrantOutbox } from "../src/enrollment.js";
import { GatewaySession, listGatewaySessions } from "../src/routing-runtime.js";

const encoder = new TextEncoder();

function base64url(bytes) { return Buffer.from(bytes).toString("base64url"); }

async function accessFixture(subject = "owner-1") {
  const team = `browser-${crypto.randomUUID()}.cloudflareaccess.com`;
  const audience = "browser-enrollment-test";
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5", modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  Object.assign(publicKey, { kid: "browser-test-key", alg: "RS256", use: "sig" });
  const claims = {
    iss: `https://${team}`, aud: audience,
    exp: Math.floor(Date.now() / 1000) + 3600,
    nbf: Math.floor(Date.now() / 1000) - 1,
    sub: subject, email: "operator@example.test",
  };
  const header = base64url(encoder.encode(JSON.stringify({ alg: "RS256", kid: publicKey.kid })));
  const payload = base64url(encoder.encode(JSON.stringify(claims)));
  const input = `${header}.${payload}`;
  const signature = await crypto.subtle.sign({ name: "RSASSA-PKCS1-v1_5" }, keyPair.privateKey, encoder.encode(input));
  return { team, audience, assertion: `${input}.${base64url(signature)}`, publicKey };
}

function database() {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec(fs.readFileSync(new URL("../migrations/0006_browser_host_enrollment.sql", import.meta.url), "utf8"));
  const wrap = (sql, params = []) => ({
    bind(...next) { return wrap(sql, next); },
    first() { return sqlite.prepare(sql).get(...params) ?? null; },
    all() { return { results: sqlite.prepare(sql).all(...params) }; },
    run() { const result = sqlite.prepare(sql).run(...params); return { meta: { changes: Number(result.changes) } }; },
  });
  return {
    prepare: (sql) => wrap(sql),
    async batch(statements) {
      sqlite.exec("BEGIN");
      try {
        const results = statements.map((statement) => statement.run());
        sqlite.exec("COMMIT");
        return results;
      } catch (error) {
        sqlite.exec("ROLLBACK");
        throw error;
      }
    },
    sqlite,
  };
}

function envFor(fixture, db, extras = {}) {
  return {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    HOST_TOKENS_JSON: "{}",
    OBSERVATION_DB: db,
    CLIENT_TOKEN: "shared-client-token",
    ...extras,
  };
}

function accessRequest(fixture, path, { method = "GET", body, hostGrant, bearer = "oauth:fixture-access-token", authorization, hostId } = {}) {
  const headers = new Headers({ "cf-access-jwt-assertion": fixture.assertion });
  const authValue = authorization === undefined ? `Bearer ${bearer}` : authorization;
  if (authValue !== null) headers.set("authorization", authValue);
  if (body !== undefined) headers.set("content-type", "application/json");
  if (hostId) headers.set("x-temote-host-id", hostId);
  if (hostGrant) headers.set("x-temote-fabric-host-grant", hostGrant);
  return new Request(`https://fabric.example.test${path}`, {
    method, headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

async function enrollFixture(fixture, env, hostId = "host-a") {
  const grant = `host-grant-${crypto.randomUUID()}-${crypto.randomUUID()}`;
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(grant)))]
    .map((value) => value.toString(16).padStart(2, "0")).join("");
  const operation = ids();
  let response = await worker.fetch(accessRequest(fixture, `/v1/enrollments/${hostId}`, {
    method: "POST", body: {
      attempt_id: operation.attempt, grant_id: operation.grant, operation_id: operation.reserve,
      grant_digest: digest, root_names: ["src"], expected_generation: 0,
    },
  }), env);
  assert.equal(response.status, 200);
  response = await worker.fetch(accessRequest(fixture, `/v1/enrollments/${hostId}/activate`, {
    method: "POST", body: {
      attempt_id: operation.attempt, reservation_operation_id: operation.reserve,
      expected_generation: 1, activation_operation_id: operation.activate,
    }, hostGrant: grant,
  }), env);
  assert.equal(response.status, 200);
  const active = await response.json();
  return { grant, digest, operation, active };
}

async function withJwks(fixture, action) {
  const original = globalThis.fetch;
  globalThis.fetch = async (input) => {
    assert.equal(String(input), `https://${fixture.team}/cdn-cgi/access/certs`);
    return Response.json({ keys: [fixture.publicKey] });
  };
  try { return await action(); } finally { globalThis.fetch = original; }
}

function ids() {
  return {
    attempt: crypto.randomUUID(),
    grant: crypto.randomUUID(),
    reserve: crypto.randomUUID(),
    activate: crypto.randomUUID(),
    revoke: crypto.randomUUID(),
  };
}

test("Access identity is verified before the CLI can reserve a Host id", async () => {
  const fixture = await accessFixture();
  const db = database();
  const env = envFor(fixture, db);
  await withJwks(fixture, async () => {
    const identity = await worker.fetch(accessRequest(fixture, "/v1/enrollment-identity"), env);
    const value = await identity.json();
    assert.equal(identity.status, 200);
    assert.match(value.owner_key, /^[0-9a-f]{64}$/);
    assert.equal(value.email, "operator@example.test");

    const clientToken = await worker.fetch(accessRequest(fixture, "/v1/enrollment-identity", { bearer: "shared-client-token" }), env);
    assert.equal(clientToken.status, 200, "a valid Access assertion remains the identity proof regardless of bearer text");
  });
});

test("reservation fails closed on legacy inventory defects and collisions", async () => {
  const fixture = await accessFixture();
  const db = database();
  const request = (hostId) => accessRequest(fixture, `/v1/enrollments/${hostId}`, {
    method: "POST",
    body: { attempt_id: crypto.randomUUID(), grant_id: crypto.randomUUID(), operation_id: crypto.randomUUID(),
      grant_digest: "a".repeat(64), root_names: ["src"], expected_generation: 0 },
  });
  await withJwks(fixture, async () => {
    let response = await worker.fetch(request("host-a"), envFor(fixture, db, { HOST_TOKENS_JSON: undefined }));
    assert.equal(response.status, 503);
    response = await worker.fetch(request("host-a"), envFor(fixture, db, { HOST_TOKENS_JSON: "{broken" }));
    assert.equal(response.status, 503);
    response = await worker.fetch(request("legacy-host"), envFor(fixture, db, {
      HOST_TOKENS_JSON: JSON.stringify({ "legacy-host": "static-token" }),
    }));
    assert.equal(response.status, 409);
    assert.equal(db.sqlite.prepare("SELECT COUNT(*) AS n FROM fabric_host_grants").get().n, 0);
  });
});

test("activation is idempotent, transport context contains only root names, and revoke commits before fencing", async () => {
  const fixture = await accessFixture();
  const db = database();
  const grant = `host-grant-${crypto.randomUUID()}-${crypto.randomUUID()}`;
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(grant)))]
    .map((value) => value.toString(16).padStart(2, "0")).join("");
  const operation = ids();
  let capturedConnect;
  const fenceGenerations = [];
  const sessionStub = {
    async fetch(url, init) {
      const path = new URL(url).pathname;
      if (path === "/fence") { fenceGenerations.push(JSON.parse(init.body).generation); return new Response(null, { status: 204 }); }
      if (path === "/connect") {
        capturedConnect = JSON.parse(init.body);
        return Response.json({ generation: 1, host_id: "host-a" });
      }
      return new Response(null, { status: 204 });
    },
  };
  const env = envFor(fixture, db, {
    GATEWAY_SESSIONS: { idFromName: (name) => name, get: () => sessionStub },
  });

  await withJwks(fixture, async () => {
    let response = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-a", {
      method: "POST", body: {
        attempt_id: operation.attempt, grant_id: operation.grant, operation_id: operation.reserve,
        grant_digest: digest, root_names: ["src"], expected_generation: 0,
      },
    }), env);
    assert.equal(response.status, 200);
    assert.equal((await response.json()).status, "pending");

    const activationRequest = () => accessRequest(fixture, "/v1/enrollments/host-a/activate", {
      method: "POST", body: { attempt_id: operation.attempt, reservation_operation_id: operation.reserve,
        expected_generation: 1, activation_operation_id: operation.activate },
      hostGrant: grant,
    });
    response = await worker.fetch(activationRequest(), env);
    assert.equal(response.status, 200);
    const active = await response.json();
    assert.equal(active.status, "active");
    assert.equal(active.generation, 1);
    assert.ok(active.expires_at > 1_000_000_000_000,
      "Worker active expiry uses Unix epoch milliseconds");
    assert.deepEqual(active.root_names, ["src"]);
    assert.equal(JSON.stringify(active).includes(digest), false);
    response = await worker.fetch(activationRequest(), env);
    assert.equal((await response.json()).generation, 1);

    const connect = await worker.fetch(accessRequest(fixture, "/v1/hosts/connect", {
      method: "POST", hostId: "host-a", hostGrant: grant,
      body: {
        host_id: "host-a", instance_id: "instance-a", agent_protocol: 1, runtime_version: "test",
        control_protocol: 2, capabilities: ["mcp"], named_roots: ["src"], platform: "macos",
      },
    }), env);
    assert.equal(connect.status, 200);
    assert.deepEqual(capturedConnect._fabric_auth, {
      mode: "browser", owner_key: active.owner_key, grant_id: operation.grant,
      grant_generation: 1, approved_roots: ["src"],
    });
    assert.equal(JSON.stringify(capturedConnect).includes(grant), false);
    assert.equal(JSON.stringify(capturedConnect).includes(digest), false);

    const rootOperation = crypto.randomUUID();
    response = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-a/roots", {
      method: "PUT", body: { operation_id: rootOperation, expected_generation: 1, root_names: ["src"] },
    }), env);
    assert.equal(response.status, 200);
    assert.equal((await response.json()).generation, 2);
    assert.equal(db.sqlite.prepare("SELECT generation FROM fabric_host_grant_outbox WHERE operation_id = ?").get(rootOperation).generation, 2);

    const revokeRequest = () => accessRequest(fixture, "/v1/enrollments/host-a", {
      method: "DELETE", body: { operation_id: operation.revoke, expected_generation: 2 },
    });
    response = await worker.fetch(revokeRequest(), env);
    const revoked = await response.json();
    assert.equal(revoked.status, "revoked");
    assert.equal(revoked.generation, 3);
    response = await worker.fetch(revokeRequest(), env);
    assert.equal((await response.json()).generation, 3);

    const replayedActiveAttempt = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-a", {
      method: "POST", body: {
        attempt_id: operation.attempt, grant_id: operation.grant, operation_id: operation.reserve,
        grant_digest: digest, root_names: ["src"], expected_generation: 3,
      },
    }), env);
    assert.equal(replayedActiveAttempt.status, 409,
      "a terminal active attempt cannot be reused even at the new tombstone generation");

    assert.equal(await authorizeBrowserHost(accessRequest(fixture, "/", { hostGrant: grant }), env, "host-a"), null);
    env.HOST_TOKENS_JSON = JSON.stringify({ "host-a": "static-token" });
    const legacyAttempt = new Request("https://fabric.example.test/v1/hosts/connect", {
      method: "POST", headers: {
        authorization: "Bearer static-token", "x-temote-host-id": "host-a", "content-type": "application/json",
      }, body: JSON.stringify({ host_id: "host-a" }),
    });
    const legacyResponse = await worker.fetch(legacyAttempt, env);
    assert.equal(legacyResponse.status, 401);

    await sweepHostGrantOutbox(env);
    assert.deepEqual(fenceGenerations.sort(), [2, 3]);
    assert.equal(db.sqlite.prepare("SELECT generation FROM fabric_host_grant_outbox WHERE operation_id = ?").get(operation.revoke).generation, 3);
    assert.equal(db.sqlite.prepare("SELECT state FROM fabric_host_grant_outbox WHERE operation_id = ?").get(operation.revoke).state, "complete");
  });
});

test("reservation, activation, and cancellation retries bind exact ids and generations", async () => {
  const fixture = await accessFixture();
  const db = database();
  const env = envFor(fixture, db);
  const grant = `host-grant-${crypto.randomUUID()}-${crypto.randomUUID()}`;
  const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(grant)))]
    .map((value) => value.toString(16).padStart(2, "0")).join("");
  const first = ids();
  const reserve = (overrides = {}) => accessRequest(fixture, "/v1/enrollments/host-cancel", {
    method: "POST", body: {
      attempt_id: first.attempt, grant_id: first.grant, operation_id: first.reserve,
      grant_digest: digest, root_names: ["src"], expected_generation: 0, ...overrides,
    },
  });

  await withJwks(fixture, async () => {
    const identityResponse = await worker.fetch(accessRequest(fixture, "/v1/enrollment-identity"), env);
    const ownerKey = (await identityResponse.json()).owner_key;
    let response = await worker.fetch(reserve(), env);
    assert.equal(response.status, 200);
    const mutationReply = await response.json();
    assert.equal(mutationReply.owner_key, ownerKey);
    assert.equal(mutationReply.generation, 1);
    assert.equal(mutationReply.grant_generation, undefined);
    assert.equal(mutationReply.operation_id, first.reserve);
    assert.equal(mutationReply.operation_generation, 1);
    assert.equal(mutationReply.expires_at, undefined);
    assert.ok(mutationReply.pending_expires_at > 1_000_000_000_000,
      "Worker pending expiry uses Unix epoch milliseconds");
    const row = db.sqlite.prepare("SELECT pending_expires_at,generation FROM fabric_host_grants WHERE host_id = ?").get("host-cancel");
    assert.equal(row.generation, 1);
    assert.ok(row.pending_expires_at - Date.now() <= 5 * 60 * 1000);
    assert.ok(row.pending_expires_at - Date.now() > 4 * 60 * 1000);

    response = await worker.fetch(reserve(), env);
    assert.equal(response.status, 200, "an exact pending reservation retry is idempotent");
    const ownedStatus = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-cancel"), env);
    const owned = await ownedStatus.json();
    assert.equal(owned.status, "pending");
    assert.equal(owned.generation, 1);
    assert.equal(owned.operation_id, first.reserve);
    assert.equal(owned.operation_generation, 1);
    assert.equal(owned.attempt_id, first.attempt);
    assert.equal(owned.reservation_operation_id, first.reserve);
    assert.equal(JSON.stringify(owned).includes(digest), false, "owner recovery never reveals grant digest");
    response = await worker.fetch(reserve({ grant_id: crypto.randomUUID() }), env);
    assert.equal(response.status, 409, "same attempt with a different grant id conflicts");
    response = await worker.fetch(reserve({ root_names: ["other"] }), env);
    assert.equal(response.status, 409, "same attempt with different roots conflicts");
    response = await worker.fetch(reserve({ expected_generation: 3 }), env);
    assert.equal(response.status, 409, "fresh reservation requires generation zero");

    db.sqlite.prepare("UPDATE fabric_host_grants SET pending_expires_at = ? WHERE host_id = ?")
      .run(Date.now() - 1, "host-cancel");
    response = await worker.fetch(reserve(), env);
    assert.equal(response.status, 200, "an exact lost-response retry can reacquire an expired pending reservation");
    const reacquired = db.sqlite.prepare("SELECT pending_expires_at,generation FROM fabric_host_grants WHERE host_id = ?").get("host-cancel");
    assert.equal(reacquired.generation, 1, "reacquisition preserves the reservation generation");
    assert.ok(reacquired.pending_expires_at > Date.now());

    const cancelOperation = crypto.randomUUID();
    const cancel = (operationId) => accessRequest(fixture, "/v1/enrollments/host-cancel/pending", {
      method: "DELETE", body: { attempt_id: first.attempt, reservation_operation_id: first.reserve,
        expected_generation: 1, operation_id: operationId },
    });
    response = await worker.fetch(cancel(cancelOperation), env);
    assert.equal(response.status, 200, "a separate cancellation operation id cancels the reservation");
    const cancellationReply = await response.json();
    assert.equal(cancellationReply.operation_id, cancelOperation);
    assert.equal(cancellationReply.operation_generation, 2);
    response = await worker.fetch(cancel(cancelOperation), env);
    assert.equal(response.status, 200, "the exact cancellation replay is idempotent");
    response = await worker.fetch(cancel(crypto.randomUUID()), env);
    assert.equal(response.status, 409, "a different cancellation id cannot rewrite the terminal operation");
    const cancelledStatus = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-cancel"), env);
    const cancelled = await cancelledStatus.json();
    assert.equal(cancelled.owner_key, ownerKey);
    assert.equal(cancelled.status, "revoked");
    assert.equal(cancelled.generation, 2);
    assert.equal(cancelled.cancel_operation_id, cancelOperation);
    assert.equal(cancelled.operation_id, cancelOperation);
    assert.equal(cancelled.operation_generation, 2);
    assert.equal(cancelled.attempt_id, first.attempt);
    assert.equal(cancelled.reservation_operation_id, first.reserve);
    response = await worker.fetch(reserve(), env);
    assert.equal(response.status, 409, "a delayed reserve cannot resurrect a cancelled attempt");
    response = await worker.fetch(reserve({ expected_generation: 2 }), env);
    assert.equal(response.status, 409, "the current generation cannot make a cancelled attempt reusable");

    const next = ids();
    response = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-cancel", {
      method: "POST", body: { attempt_id: next.attempt, grant_id: next.grant, operation_id: next.reserve,
        grant_digest: digest, root_names: ["src"], expected_generation: 2 },
    }), env);
    assert.equal(response.status, 200, "a fresh attempt may reserve the next committed generation");
    assert.equal((await response.json()).generation, 3);

    const activation = (expectedGeneration) => accessRequest(fixture, "/v1/enrollments/host-cancel/activate", {
      method: "POST", body: { attempt_id: next.attempt, reservation_operation_id: next.reserve,
        expected_generation: expectedGeneration, activation_operation_id: next.activate }, hostGrant: grant,
    });
    response = await worker.fetch(activation(3), env);
    assert.equal(response.status, 200);
    response = await worker.fetch(activation(3), env);
    assert.equal(response.status, 200, "the exact activation replay is idempotent");
    response = await worker.fetch(activation(2), env);
    assert.equal(response.status, 409, "activation is bound to its reserved generation");
    const activeRow = db.sqlite.prepare("SELECT expires_at,generation FROM fabric_host_grants WHERE host_id = ?").get("host-cancel");
    assert.ok(activeRow.expires_at - Date.now() > 89 * 24 * 60 * 60 * 1000);
  });
});

test("fencing outbox retries use the committed generation and survive delivery failure", async () => {
  const fixture = await accessFixture();
  const db = database();
  const fenceGenerations = [];
  let failOnce = true;
  const env = envFor(fixture, db, {
    GATEWAY_REGISTRY: { idFromName: (name) => name, get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
    GATEWAY_SESSIONS: { idFromName: (name) => name, get: () => ({
    async fetch(url, init) {
      assert.equal(new URL(url).pathname, "/fence");
      fenceGenerations.push(JSON.parse(init.body).generation);
      if (failOnce) { failOnce = false; return new Response(null, { status: 503 }); }
      return new Response(null, { status: 204 });
    },
    }) },
  });
  await withJwks(fixture, async () => {
    const enrollment = await enrollFixture(fixture, env);
    const op = crypto.randomUUID();
    let response = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-a/roots", {
      method: "PUT", body: { operation_id: op, expected_generation: 1, root_names: ["src"] },
    }), env);
    assert.equal(response.status, 200);
    assert.equal(db.sqlite.prepare("SELECT generation FROM fabric_host_grant_outbox WHERE operation_id = ?").get(op).generation, 2);
    await sweepHostGrantOutbox(env);
    let outbox = db.sqlite.prepare("SELECT state,attempts,generation FROM fabric_host_grant_outbox WHERE operation_id = ?").get(op);
    assert.equal(outbox.state, "pending");
    assert.equal(outbox.attempts, 1);
    assert.equal(outbox.generation, 2);
    await sweepHostGrantOutbox(env);
    outbox = db.sqlite.prepare("SELECT state,attempts,generation FROM fabric_host_grant_outbox WHERE operation_id = ?").get(op);
    assert.equal(outbox.state, "complete");
    assert.equal(outbox.attempts, 2);
    assert.equal(outbox.generation, 2);
    assert.deepEqual(fenceGenerations, [2, 2]);
    assert.equal(enrollment.active.generation, 1);
  });
});

test("owner can tombstone an expired active grant and retry the same revoke operation", async () => {
  const fixture = await accessFixture();
  const db = database();
  const env = envFor(fixture, db);
  await withJwks(fixture, async () => {
    await enrollFixture(fixture, env);
    db.sqlite.prepare("UPDATE fabric_host_grants SET expires_at = ? WHERE host_id = ?")
      .run(Date.now() - 1, "host-a");
    const operationId = crypto.randomUUID();
    const revoke = () => accessRequest(fixture, "/v1/enrollments/host-a", {
      method: "DELETE", body: { operation_id: operationId, expected_generation: 1 },
    });
    let response = await worker.fetch(revoke(), env);
    assert.equal(response.status, 200, "expiry removes authority but does not prevent owner cleanup");
    let result = await response.json();
    assert.equal(result.status, "revoked");
    assert.equal(result.generation, 2);
    response = await worker.fetch(revoke(), env);
    assert.equal(response.status, 200, "lost-response retry observes the same tombstone");
    result = await response.json();
    assert.equal(result.generation, 2, "same operation does not bump generation twice");
    response = await worker.fetch(accessRequest(fixture, "/v1/enrollments/host-a", {
      method: "DELETE", body: { operation_id: crypto.randomUUID(), expected_generation: 1 },
    }), env);
    assert.equal(response.status, 409, "another operation cannot rewrite the tombstone");
  });
});

test("browser MCP and dashboard deny cloud data surfaces and client metadata cannot expand authority", async () => {
  const fixture = await accessFixture();
  const env = envFor(fixture, database());
  await withJwks(fixture, async () => {
    const request = (name) => new Request("https://fabric.example.test/mcp", {
      method: "POST",
      headers: { authorization: "Bearer oauth:fixture-access-token", "cf-access-jwt-assertion": fixture.assertion, "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: {
        name, arguments: { session_id: "session-a", host_id: "host-a" },
        _meta: { owner_key: "f".repeat(64), approved_roots: ["forged"] },
      } }),
    });
    for (const name of ["context_resolve", "context_status", "fabric_overview", "fabric_session_list", "fabric_session_read", "search_mentions", "fabric_interaction_read"]) {
      const response = await worker.fetch(request(name), env);
      assert.equal((await response.json()).error.code, -32601, name);
    }
    const dashboard = await worker.fetch(accessRequest(fixture, "/dash/api/bootstrap"), env);
    assert.equal(dashboard.status, 403);

    const tools = await worker.fetch(new Request("https://fabric.example.test/mcp", {
      method: "POST", headers: {
        authorization: "Bearer oauth:fixture-access-token", "cf-access-jwt-assertion": fixture.assertion, "content-type": "application/json",
      }, body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} }),
    }), env);
    const names = (await tools.json()).result.tools.map((tool) => tool.name);
    for (const denied of ["context_resolve", "context_status", "fabric_overview", "fabric_session_list", "fabric_session_read"]) assert.equal(names.includes(denied), false);
    assert.equal(names.includes("host_list"), true);
    for (const allowed of ["codex_status", "codex_task_start", "codex_task_get", "codex_task_control",
      "evidence_read", "task_list", "poll_job", "job_list", "stop_job"]) assert.equal(names.includes(allowed), true);
    for (const denied of ["opencode_task_start", "devin_cloud_task_start", "repository_clone_bare",
      "search_mentions", "fabric_interaction_read"]) assert.equal(names.includes(denied), false);

  });
});

test("every verified Access carrier stays owner-scoped and CLIENT_TOKEN cannot discover or dispatch to reserved Hosts", async () => {
  const fixture = await accessFixture();
  const db = database();
  const dispatchCalls = [];
  const host = {
    host_id: "host-a", instance_id: "instance-a", generation: 1, auth_mode: "browser",
    expires_at: Date.now() + 90_000, session_availability: "ready",
  };
  const hostStub = {
    async fetch(url, init = {}) {
      const path = new URL(url).pathname;
      if (path === "/status") return Response.json({ status: "registered" });
      if (path === "/dispatch") {
        const body = JSON.parse(init.body);
        if (body.request?.params?.name === "session_list") {
          return Response.json({ jsonrpc: "2.0", id: body.request.id, result: { content: [{ type: "text", text: JSON.stringify([{
            session_id: "session-a", root_name: "src", logical_path: "src/repo", status: "active",
            permission_mode: "agent", yolo: false, session_instance: "00000000-0000-4000-8000-000000000001",
          }]) }] } });
        }
        dispatchCalls.push(body);
        return Response.json({ jsonrpc: "2.0", id: body.request.id,
          result: { content: [{ type: "text", text: "routed" }] } });
      }
      return Response.json({ status: "registered" });
    },
  };
  const env = envFor(fixture, db, {
    GATEWAY_SESSIONS: { idFromName: (name) => name, get: () => hostStub },
    GATEWAY_REGISTRY: { idFromName: (name) => name, get: () => ({ fetch: async (url) => (
      new URL(url).pathname === "/hosts" ? Response.json([host]) : Response.json([])
    ) }) },
  });
  await withJwks(fixture, async () => {
    const enrollment = await enrollFixture(fixture, env);
    Object.assign(host, {
      owner_key: enrollment.active.owner_key, grant_id: enrollment.operation.grant,
      grant_generation: 1, approved_roots: ["src"],
    });

    const publicInventory = await worker.fetch(accessRequest(fixture, "/mcp", {
      method: "POST", authorization: null,
      body: { jsonrpc: "2.0", id: 3, method: "tools/call", params: {
        name: "session_list", arguments: { host_id: "host-a" },
      } },
    }), env);
    const publicSessions = JSON.parse((await publicInventory.json()).result.content[0].text);
    assert.equal(publicSessions[0].session_id, "session-a");
    assert.equal(publicSessions[0].root_name, "src");
    assert.equal(Object.hasOwn(publicSessions[0], "session_instance"), false,
      "the Worker retains the instance proof internally instead of exposing it to the MCP caller");
    assert.equal(Object.hasOwn(publicSessions[0], "instance_id"), false);

    const carriers = [null, "Bearer arbitrary-opaque", "Bearer oauth:prefix-like-only", "Bearer oauth:fixture-access-token"];
    for (const authorization of carriers) {
      const mcp = async (rpc) => worker.fetch(accessRequest(fixture, "/mcp", {
        method: "POST", authorization,
        body: { jsonrpc: "2.0", id: 1, ...rpc },
      }), env);
      let response = await mcp({ method: "tools/call", params: { name: "host_list", arguments: {} } });
      assert.equal(response.status, 200);
      const hostList = await response.json();
      assert.match(hostList.result.content[0].text, /host-a/, `carrier ${authorization ?? "<absent>"} uses owner-scoped discovery`);

      for (const name of ["context_resolve", "context_status", "fabric_overview", "fabric_session_list",
        "fabric_session_read", "search_mentions", "fabric_interaction_read"]) {
        response = await mcp({ method: "tools/call", params: { name, arguments: { host_id: "host-a" } } });
        assert.equal((await response.json()).error.code, -32601, `${name} denied for ${authorization ?? "<absent>"}`);
      }
      response = await mcp({ method: "resources/list", params: {} });
      assert.equal((await response.json()).error.code, -32601);

      const dashboard = await worker.fetch(accessRequest(fixture, "/dash/api/bootstrap", { authorization }), env);
      assert.equal(dashboard.status, 403, `dashboard is denied for ${authorization ?? "<absent>"}`);
      const observationReplica = await worker.fetch(accessRequest(fixture, "/v1/hosts/host-a/observations/sync", {
        method: "POST", authorization, hostId: "host-a", body: { records: [] },
      }), env);
      assert.equal(observationReplica.status, 401, `observation replica remains unavailable for ${authorization ?? "<absent>"}`);
    }

    const tokenHeaders = { authorization: "Bearer shared-client-token", "content-type": "application/json" };
    const clientCall = (name, argumentsValue) => worker.fetch(new Request("https://fabric.example.test/mcp", {
      method: "POST", headers: tokenHeaders,
      body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name, arguments: argumentsValue } }),
    }), env);
    let response = await clientCall("host_list", {});
    assert.doesNotMatch((await response.json()).result.content[0].text, /host-a/, "CLIENT_TOKEN discovery excludes D1-reserved browser ids");
    response = await clientCall("host_info", { host_id: "host-a" });
    assert.equal((await response.json()).error.code, -32004, "direct legacy lookup cannot see a browser id");
    response = await clientCall("session_list", {});
    assert.doesNotMatch((await response.json()).result.content[0].text, /host-a/, "legacy session discovery excludes browser Hosts");
    response = await clientCall("session_start", { host_id: "host-a", path: "src/private" });
    assert.equal((await response.json()).error.code, -32004, "CLIENT_TOKEN cannot route session_start to a browser Host");
    response = await clientCall("session_info", { session_id: "browser-private-session" });
    assert.equal((await response.json()).error.code, -32004, "unqualified legacy routing does not resolve browser sessions");
    assert.equal(dispatchCalls.length, 0, "no legacy call reached the browser Host DO");

    response = await worker.fetch(accessRequest(fixture, "/mcp", {
      method: "POST", authorization: null,
      body: { jsonrpc: "2.0", id: 3, method: "tools/call", params: {
        name: "session_start", arguments: { host_id: "host-a", path: "other/repository" },
      } },
    }), env);
    assert.equal((await response.json()).error.code, -32601, "owner grant cannot start a root outside its approved set");
    response = await worker.fetch(accessRequest(fixture, "/mcp", {
      method: "POST", authorization: "Bearer opaque", body: {
        jsonrpc: "2.0", id: 4, method: "tools/call", params: {
          name: "session_start", arguments: { host_id: "host-a", path: "src/repository" },
        },
      },
    }), env);
    const startResult = await response.json();
    assert.equal(startResult.result?.content?.[0]?.text, "routed", JSON.stringify(startResult));
    assert.equal(dispatchCalls.length, 1);
    assert.deepEqual(dispatchCalls[0]._fabric_auth.approved_roots, ["src"]);

    const taskOperation = crypto.randomUUID();
    response = await worker.fetch(accessRequest(fixture, "/mcp", {
      method: "POST", authorization: null,
      body: { jsonrpc: "2.0", id: 5, method: "tools/call", params: {
        name: "codex_task_start",
        arguments: { host_id: "host-a", session_id: "session-a", operation_id: taskOperation,
          task: "Inspect the scoped repository", model: "gpt-6", effort: "high" },
      } },
    }), env);
    const taskResult = await response.json();
    assert.equal(taskResult.result?.content?.[0]?.text, "routed", JSON.stringify(taskResult));
    assert.equal(dispatchCalls.length, 2, "a valid typed Codex task reaches the authorized Host");
    assert.equal(dispatchCalls[1].request.params.name, "codex_task_start");
    assert.equal(dispatchCalls[1].request.params.arguments.host_id, undefined,
      "the Worker removes Host routing metadata before dispatch");
    assert.equal(dispatchCalls[1].request.params.arguments.operation_id, taskOperation);
    assert.deepEqual(dispatchCalls[1]._fabric_session, {
      session_id: "session-a", session_instance: "00000000-0000-4000-8000-000000000001",
    }, "the Worker binds the routed tool to the current Supervisor session instance");

    response = await worker.fetch(accessRequest(fixture, "/mcp", {
      method: "POST", authorization: null,
      body: { jsonrpc: "2.0", id: 6, method: "tools/call", params: {
        name: "codex_task_start",
        arguments: { host_id: "host-a", session_id: "session-a", task: "must be rejected", model: "gpt-6", effort: "high" },
      } },
    }), env);
    assert.equal((await response.json()).error.code, -32601, "task start requires a caller operation id");
    assert.equal(dispatchCalls.length, 2, "invalid task calls are rejected before Host dispatch");
  });
});

test("GatewaySession queues only the exact browser typed-task contract under the current grant snapshot", async () => {
  const fixture = await accessFixture();
  const db = database();
  const env = envFor(fixture, db, {
    GATEWAY_REGISTRY: { idFromName: (name) => name, get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) },
  });
  await withJwks(fixture, async () => {
    const enrollment = await enrollFixture(fixture, env);
    const auth = {
      mode: "browser",
      owner_key: enrollment.active.owner_key,
      grant_id: enrollment.operation.grant,
      grant_generation: 1,
      approved_roots: ["src"],
    };
    const storageValues = new Map();
    const session = new GatewaySession({
      storage: {
        async get(key) { return storageValues.get(key); },
        async put(key, value) {
          if (key && typeof key === "object" && !Array.isArray(key)) {
            for (const [name, item] of Object.entries(key)) storageValues.set(name, item);
          } else storageValues.set(key, value);
        },
        async delete(key) { storageValues.delete(key); },
      },
    }, env);
    const identity = {
      host_id: "host-a", instance_id: "instance-a", platform: "macos", agent_protocol: 1,
      runtime_version: "test", control_protocol: 2,
      capabilities: ["session_lifecycle", "session_tools", "named_roots"], named_roots: ["src"],
      _fabric_auth: auth,
    };
    const connected = await session.fetch(new Request("https://host.internal/connect", {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(identity),
    }));
    assert.equal(connected.status, 200, await connected.text());

    const request = (name, argumentsValue) => ({
      jsonrpc: "2.0", id: crypto.randomUUID(), method: "tools/call",
      params: { name, arguments: argumentsValue },
    });
    const invalid = await session.fetch(new Request("https://host.internal/dispatch", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ request: request("codex_task_start", {
        session_id: "session-a", task: "missing operation", model: "gpt-6", effort: "high",
      }), _fabric_auth: auth }),
    }));
    assert.equal(invalid.status, 403);
    assert.equal(session.queue.length, 0, "invalid start is rejected before the Host can see it");

    const rpc = request("codex_task_start", {
      session_id: "session-a", operation_id: crypto.randomUUID(),
      task: "Inspect this repository", model: "gpt-6", effort: "high",
    });
    const sessionBinding = {
      session_id: "session-a",
      session_instance: "00000000-0000-4000-8000-000000000001",
    };
    const missingBinding = await session.fetch(new Request("https://host.internal/dispatch", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ request: rpc, _fabric_auth: auth }),
    }));
    assert.equal(missingBinding.status, 403, "browser session calls require a Worker-captured live instance");
    assert.equal(session.queue.length, 0, "unbound requests never reach the Link");
    const pending = session.fetch(new Request("https://host.internal/dispatch", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ request: rpc, _fabric_auth: auth, _fabric_session: sessionBinding }),
    }));
    const polled = await session.fetch(new Request("https://host.internal/poll", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ ...identity, generation: 1 }),
    }));
    const envelope = await polled.json();
    assert.equal(envelope.request.params.name, "codex_task_start");
    assert.equal(envelope._fabric_auth.grant_generation, 1);
    assert.deepEqual(envelope._fabric_session, sessionBinding);
    assert.equal(envelope.request.params.arguments.operation_id, rpc.params.arguments.operation_id);
    const wrongBindingResponse = await session.fetch(new Request("https://host.internal/respond", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({
        ...identity, generation: 1, request_id: envelope.request_id,
        _fabric_auth: auth,
        _fabric_session: { ...sessionBinding, session_instance: "00000000-0000-4000-8000-000000000002" },
        response: { jsonrpc: "2.0", id: rpc.id, result: { content: [] } },
      }),
    }));
    assert.equal(wrongBindingResponse.status, 409, "a response cannot change its captured session instance");
    const response = await session.fetch(new Request("https://host.internal/respond", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({
        ...identity, generation: 1, request_id: envelope.request_id,
        _fabric_auth: auth, _fabric_session: sessionBinding,
        response: { jsonrpc: "2.0", id: rpc.id, result: { content: [] } },
      }),
    }));
    assert.equal(response.status, 204);
    assert.equal((await pending).status, 200);
  });
});

test("browser Host session discovery discards both direct and aggregate results after scope changes", async () => {
  for (const aggregate of [false, true]) {
    for (const mutation of ["roots", "revoke", "expire", "reenroll"]) {
    const fixture = await accessFixture();
    const db = database();
    const env = envFor(fixture, db);
    let ownerKey;
    await withJwks(fixture, async () => {
      const enrollment = await enrollFixture(fixture, env);
      ownerKey = enrollment.active.owner_key;
      const host = {
        host_id: "host-a", instance_id: "instance-a", generation: 1, auth_mode: "browser",
        owner_key: ownerKey, grant_id: enrollment.operation.grant, grant_generation: 1,
        approved_roots: ["src"], expires_at: Date.now() + 90_000,
      };
      env.GATEWAY_REGISTRY = { idFromName: (name) => name, get: () => ({ fetch: async () => Response.json([host]) }) };
      env.GATEWAY_SESSIONS = { idFromName: (name) => name, get: () => ({
        async fetch(url) {
          if (new URL(url).pathname !== "/dispatch") return Response.json({ status: "registered" });
          if (mutation === "roots") {
            db.sqlite.prepare("UPDATE fabric_host_grants SET roots_json = ?, generation = generation + 1 WHERE host_id = ?")
              .run(JSON.stringify(["other"]), "host-a");
          } else if (mutation === "revoke") {
            db.sqlite.prepare("UPDATE fabric_host_grants SET status = 'revoked', generation = generation + 1 WHERE host_id = ?")
              .run("host-a");
          } else if (mutation === "expire") {
            db.sqlite.prepare("UPDATE fabric_host_grants SET expires_at = ? WHERE host_id = ?")
              .run(Date.now() - 1, "host-a");
          } else {
            db.sqlite.prepare("UPDATE fabric_host_grants SET grant_id = ?, generation = generation + 1 WHERE host_id = ?")
              .run(crypto.randomUUID(), "host-a");
          }
          return Response.json({ jsonrpc: "2.0", id: 1, result: { content: [{ type: "text",
            text: JSON.stringify([{ session_id: "private-session", root_name: "src", logical_path: "src/private",
              status: "active", instance_id: "instance-a" }]) }] } });
        },
      }) };
      const listed = await listGatewaySessions(env, aggregate ? null : "host-a", ownerKey);
      assert.equal(listed.ok, false, `${aggregate ? "aggregate" : "direct"} discovery fails after ${mutation}`);
      assert.equal(listed.value?.some((session) => session.root_name === "src") ?? false, false);
    });
    }
  }
});

test("GatewaySession dispatch checks the owner envelope, committed D1 generation, and named root", async () => {
  const fixture = await accessFixture();
  const db = database();
  const env = envFor(fixture, db);
  await withJwks(fixture, async () => {
    const enrollment = await enrollFixture(fixture, env);
    const auth = { mode: "browser", owner_key: enrollment.active.owner_key, grant_id: enrollment.operation.grant,
      grant_generation: 1, approved_roots: ["src"] };
    const host = { host_id: "host-a", instance_id: "instance-a", generation: 1, auth_mode: "browser",
      owner_key: auth.owner_key, grant_id: auth.grant_id, grant_generation: auth.grant_generation,
      approved_roots: ["src"], expires_at: Date.now() + 90_000 };
    let storedHost = host;
    const state = { storage: {
      async get(key) { return key === "host" ? storedHost : undefined; },
      async put(key, value) { if (key === "host") storedHost = value; },
      async delete(key) { if (key === "host") storedHost = undefined; },
    } };
    env.GATEWAY_REGISTRY = { idFromName: (name) => name, get: () => ({ fetch: async () => new Response(null, { status: 204 }) }) };
    const session = new GatewaySession(state, env, { rpcTimeoutMs: 25, requestId: () => crypto.randomUUID() });
    const dispatch = (fabricAuth, path) => session.fetch(new Request("https://do.internal/dispatch", {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({
        _fabric_auth: fabricAuth,
        request: { jsonrpc: "2.0", id: 1, method: "tools/call", params: {
          name: "session_start", arguments: { path },
        } },
      }),
    }));
    assert.equal((await dispatch(undefined, "src/repo")).status, 409, "missing owner envelope is rejected");
    assert.equal((await dispatch({ ...auth, owner_key: "f".repeat(64) }, "src/repo")).status, 409, "wrong owner is rejected");
    assert.equal((await dispatch({ ...auth, grant_generation: 2 }, "src/repo")).status, 409, "wrong generation is rejected");
    assert.equal((await dispatch(auth, "src/../other")).status, 403, "path traversal is rejected");
    assert.equal((await dispatch(auth, "outside/repo")).status, 403, "unapproved named root is rejected");

    db.sqlite.prepare("UPDATE fabric_host_grants SET generation = 2, roots_json = ? WHERE host_id = ?")
      .run(JSON.stringify(["other"]), "host-a");
    assert.equal((await dispatch(auth, "src/repo")).status, 409, "cached DO context cannot outlive D1 scope commit");

    const fence = (generation) => session.fetch(new Request("https://do.internal/fence", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ host_id: "host-a", generation }),
    }));
    assert.equal((await fence(2)).status, 204);
    assert.equal(storedHost, undefined, "a stale generation is fenced after D1 commits its replacement");
    storedHost = { ...host, grant_generation: 2, approved_roots: ["other"] };
    assert.equal((await fence(2)).status, 204);
    assert.equal(storedHost.grant_generation, 2, "the delayed committed-generation fence preserves a reconnected Link");
    storedHost = { ...storedHost, grant_generation: 3, grant_id: crypto.randomUUID() };
    assert.equal((await fence(2)).status, 204);
    assert.equal(storedHost.grant_generation, 3, "a stale retry cannot fence a later generation");
  });
});
