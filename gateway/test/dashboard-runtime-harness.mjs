import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { randomUUID, webcrypto } from "node:crypto";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { unstable_dev } from "wrangler";

const GATEWAY_DIR = fileURLToPath(new URL("..", import.meta.url));
const WORKER_SCRIPT = path.join(GATEWAY_DIR, "src/index.js");
const WRANGLER_CONFIG = path.join(GATEWAY_DIR, "wrangler.toml");

export const DASHBOARD_RUNTIME_CLIENT_TOKEN = "dashboard-runtime-test-client-token";
export const DASHBOARD_RUNTIME_HOST_TOKEN = "dashboard-runtime-test-host-token";
export const DASHBOARD_RUNTIME_FEDERATED_TOKEN = "dashboard-runtime-test-federated-token";

const ENCODER = new TextEncoder();

export async function createDashboardAccessFixture() {
  const teamDomain = `runtime-${randomUUID()}.cloudflareaccess.com`;
  const audience = "dashboard-runtime-test-audience";
  const email = "dashboard-runtime-test@example.com";
  const keyPair = await webcrypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5",
    modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await webcrypto.subtle.exportKey("jwk", keyPair.publicKey);
  publicKey.kid = `dashboard-runtime-${randomUUID()}`;
  publicKey.alg = "RS256";
  publicKey.use = "sig";

  async function sign(overrides = {}) {
    const claims = {
      iss: `https://${teamDomain}`,
      aud: audience,
      exp: Math.floor(Date.now() / 1000) + 3600,
      nbf: Math.floor(Date.now() / 1000) - 1,
      sub: "dashboard-runtime-operator",
      email,
      ...overrides,
    };
    const headerPart = Buffer.from(JSON.stringify({
      alg: "RS256",
      kid: publicKey.kid,
      typ: "JWT",
    })).toString("base64url");
    const claimsPart = Buffer.from(JSON.stringify(claims)).toString("base64url");
    const signingInput = `${headerPart}.${claimsPart}`;
    const signature = await webcrypto.subtle.sign(
      { name: "RSASSA-PKCS1-v1_5" },
      keyPair.privateKey,
      ENCODER.encode(signingInput),
    );
    return `${signingInput}.${Buffer.from(signature).toString("base64url")}`;
  }

  return {
    teamDomain,
    audience,
    email,
    publicKey,
    jwksJson: JSON.stringify({ keys: [publicKey] }),
    sign,
  };
}

export async function startDashboardRuntime({ accessFixture } = {}) {
  const persistencePath = await mkdtemp(path.join(os.tmpdir(), "temote-dashboard-runtime-"));
  let wrapperDirectory;
  let worker;

  try {
    let script = WORKER_SCRIPT;
    if (accessFixture) {
      wrapperDirectory = await mkdtemp(path.join(GATEWAY_DIR, ".dashboard-runtime-"));
      script = path.join(wrapperDirectory, "worker.js");
      await writeFile(script, dashboardRuntimeWrapper(accessFixture), "utf8");
    }

    worker = await unstable_dev(script, {
      config: WRANGLER_CONFIG,
      local: true,
      ip: "127.0.0.1",
      port: 0,
      persist: true,
      persistTo: persistencePath,
      vars: {
        CLIENT_TOKEN: DASHBOARD_RUNTIME_CLIENT_TOKEN,
        HOST_TOKEN: DASHBOARD_RUNTIME_HOST_TOKEN,
        HOST_TOKENS_JSON: JSON.stringify({ "runtime-host": DASHBOARD_RUNTIME_FEDERATED_TOKEN }),
        ACCESS_TEAM_DOMAIN: accessFixture?.teamDomain ?? "runtime.invalid.cloudflareaccess.com",
        ACCESS_AUDIENCE: accessFixture?.audience ?? "dashboard-runtime-test-audience",
        ACCESS_ALLOWED_EMAILS: accessFixture?.email ?? "dashboard-runtime-test@example.com",
      },
      logLevel: accessFixture ? "none" : "error",
      experimental: {
        disableDevRegistry: true,
        disableExperimentalWarning: true,
        showInteractiveDevSession: false,
      },
    });
  } catch (error) {
    if (wrapperDirectory) await rm(wrapperDirectory, { recursive: true, force: true });
    await rm(persistencePath, { recursive: true, force: true });
    throw error;
  }

  return {
    fetch(input, init = {}) {
      return worker.fetch(input, { ...init, redirect: "manual" });
    },
    async close() {
      try {
        await worker.stop();
      } finally {
        if (wrapperDirectory) await rm(wrapperDirectory, { recursive: true, force: true });
        await rm(persistencePath, { recursive: true, force: true });
      }
    },
  };
}

function dashboardRuntimeWrapper(accessFixture) {
  const fixtureUrl = `https://${accessFixture.teamDomain}/cdn-cgi/access/certs`;
  return `
import worker from "../src/index.js";
export * from "../src/index.js";

// This test-only egress fixture supplies public JWKS for the configured test
// issuer. Production Access JWT verification, signature checks, claims checks,
// and Static Assets routing remain the code under test.
const originalFetch = globalThis.fetch;
globalThis.fetch = async (input, init) => {
  const url = input instanceof Request ? input.url : String(input);
  if (url === ${JSON.stringify(fixtureUrl)}) {
    return new Response(${JSON.stringify(accessFixture.jwksJson)}, {
      headers: { "content-type": "application/json" },
    });
  }
  return originalFetch.call(globalThis, input, init);
};

export default worker;
`;
}
