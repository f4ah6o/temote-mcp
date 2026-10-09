import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";

import worker from "../src/index.js";
import { projectPendingInteraction } from "../src/dashboard/projection.js";

const encoder = new TextEncoder();

function base64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function accessFixture() {
  const team = `dashboard-${randomUUID()}.cloudflareaccess.com`;
  const audience = `dashboard-aud-${randomUUID()}`;
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5", modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  publicKey.kid = `dashboard-${randomUUID()}`;
  publicKey.alg = "RS256";
  publicKey.use = "sig";
  const claims = {
    iss: `https://${team}`, aud: audience,
    exp: Math.floor(Date.now() / 1000) + 3600,
    nbf: Math.floor(Date.now() / 1000) - 1,
    sub: "dashboard-owner", email: "operator@example.test",
  };
  const header = base64url(encoder.encode(JSON.stringify({ alg: "RS256", kid: publicKey.kid })));
  const payload = base64url(encoder.encode(JSON.stringify(claims)));
  const input = `${header}.${payload}`;
  const signature = await crypto.subtle.sign(
    { name: "RSASSA-PKCS1-v1_5" }, keyPair.privateKey, encoder.encode(input),
  );
  return { team, audience, publicKey, assertion: `${input}.${base64url(signature)}` };
}

async function withJwks(fixture, action) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input) => {
    assert.equal(String(input), `https://${fixture.team}/cdn-cgi/access/certs`);
    return Response.json({ keys: [fixture.publicKey] });
  };
  try { return await action(); } finally { globalThis.fetch = originalFetch; }
}

function envFor(fixture, onDataRead) {
  const values = {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    CLIENT_TOKEN: "shared-client-token",
    OBSERVATION_OWNER_ID: "legacy-global-owner",
    OBSERVATION_DB: { prepare() { onDataRead("d1"); throw new Error("unexpected D1 read"); } },
    GATEWAY_REGISTRY: { idFromName() { onDataRead("registry"); throw new Error("unexpected registry read"); } },
    GATEWAY_SESSIONS: { idFromName() { onDataRead("host"); throw new Error("unexpected Host read"); } },
  };
  return new Proxy(values, {
    get(target, key, receiver) {
      if (["OBSERVATION_DB", "GATEWAY_REGISTRY", "GATEWAY_SESSIONS"].includes(key)) onDataRead(key);
      return Reflect.get(target, key, receiver);
    },
  });
}

test("verified browser owners are denied every dashboard data route before legacy data reads", async () => {
  const fixture = await accessFixture();
  const reads = [];
  const env = envFor(fixture, (name) => reads.push(name));
  const paths = [
    "/dash/api/v1/bootstrap",
    "/dash/api/v1/hosts",
    "/dash/api/v1/hosts/mac-main/sessions",
    "/dash/api/v1/hosts/mac-main/sessions/session-a",
    "/dash/api/v1/hosts/mac-main/sessions/session-a/tasks",
    "/dash/api/v1/hosts/mac-main/sessions/session-a/context",
    "/dash/api/v1/hosts/mac-main/sessions/session-a/timeline",
  ];
  await withJwks(fixture, async () => {
    for (const path of paths) {
      const response = await worker.fetch(new Request(`https://fabric.example.test${path}`, {
        headers: { "cf-access-jwt-assertion": fixture.assertion },
      }), env);
      assert.equal(response.status, 403, path);
      assert.deepEqual(await response.json(), { error_code: "browser_data_surface_unavailable" }, path);
      assert.equal(response.headers.get("cache-control"), "no-store");
      assert.equal(response.headers.get("x-frame-options"), "DENY");
    }
  });
  assert.deepEqual(reads, [], "the shared legacy D1, registry, and Host surfaces are untouched");
});

test("pending summaries require a coherent fresh producer-owned projection", () => {
  const now = 1_800_000_000;
  const base = {
    state: "none", count: 0, types: [], summary_revision: 1,
    observed_at: now, producer_kind: "runtime_owner", producer_epoch: 1,
    expires_at: now + 30, truncated: false,
  };
  assert.equal(projectPendingInteraction(base, now).state, "none");
  const noneWithoutCount = { ...base };
  delete noneWithoutCount.count;
  assert.equal(projectPendingInteraction(noneWithoutCount, now).state, "none");
  const pendingWithoutCount = { ...base, state: "pending", types: ["permission"] };
  delete pendingWithoutCount.count;
  assert.equal(projectPendingInteraction(pendingWithoutCount, now).state, "pending");
  assert.equal(projectPendingInteraction({ ...base, summary_revision: 0 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, producer_epoch: 0 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, expires_at: now + 31 }, now).state, "unavailable");
  assert.equal(projectPendingInteraction({ ...base, state: "none", count: 1 }, now).state, "unavailable");
});
