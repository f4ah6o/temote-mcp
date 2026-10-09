import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";

import worker from "../src/index.js";

const encoder = new TextEncoder();

function base64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function accessFixture() {
  const team = `dashboard-scope-${randomUUID()}.cloudflareaccess.com`;
  const audience = `dashboard-scope-${randomUUID()}`;
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5", modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  publicKey.kid = `dashboard-scope-${randomUUID()}`;
  publicKey.alg = "RS256";
  publicKey.use = "sig";
  const claims = {
    iss: `https://${team}`, aud: audience,
    exp: Math.floor(Date.now() / 1000) + 3600,
    nbf: Math.floor(Date.now() / 1000) - 1,
    sub: "dashboard-scope-owner", email: "operator@example.test",
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

test("browser dashboard identity cannot probe or read any host/session scope", async () => {
  const fixture = await accessFixture();
  const reads = [];
  const env = {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    GATEWAY_REGISTRY: { idFromName() { reads.push("registry"); throw new Error("must not probe"); } },
    GATEWAY_SESSIONS: { idFromName() { reads.push("host"); throw new Error("must not probe"); } },
    OBSERVATION_DB: { prepare() { reads.push("d1"); throw new Error("must not read replica"); } },
  };
  const paths = [
    "/dash/api/v1/hosts/mac-main/sessions",
    "/dash/api/v1/hosts/other-host/sessions",
    "/dash/api/v1/hosts/mac-main/sessions/shared",
    "/dash/api/v1/hosts/mac-main/sessions/shared/tasks",
    "/dash/api/v1/hosts/mac-main/sessions/shared/context",
    "/dash/api/v1/hosts/mac-main/sessions/shared/timeline",
    "/dash/api/v1/hosts/../other-host/sessions/shared",
  ];
  await withJwks(fixture, async () => {
    for (const path of paths) {
      const response = await worker.fetch(new Request(`https://fabric.example.test${path}`, {
        headers: { "cf-access-jwt-assertion": fixture.assertion },
      }), env);
      assert.equal(response.status, 403, path);
      assert.deepEqual(await response.json(), { error_code: "browser_data_surface_unavailable" }, path);
    }
  });
  assert.deepEqual(reads, [], "browser identities cannot use URL scope to reach the shared replica or host registry");
});
