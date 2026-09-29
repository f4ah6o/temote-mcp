import assert from "node:assert/strict";
import test, { after, before } from "node:test";

import {
  DASHBOARD_RUNTIME_CLIENT_TOKEN,
  DASHBOARD_RUNTIME_HOST_TOKEN,
  startDashboardRuntime,
} from "./dashboard-runtime-harness.mjs";

const DASHBOARD_ROUTES = [
  "/dash",
  "/dash/",
  "/dash/index.html",
  "/dash/app.js",
  "/dash/styles.css",
  "/dash/api/v1/bootstrap",
  "/dash/unknown",
  "/dash/unknown/index.html",
];

const DASHBOARD_PATH_ALIASES = [
  "/%64ash/",
  "/%64ash/app.js",
  "/dash%2fapp.js",
  "/dash%2Fapp.js",
  "/DASH/",
  "/DASH/app.js",
  "/Dash/styles.css",
  "/dash//",
  "/dash//app.js",
  "/dash/./app.js",
  "/dash/%2e/app.js",
  "/dash/app.js/",
  "/dash/index.html/",
  "/dash/../dash/app.js",
  "/dash/%2e%2e/dash/app.js",
];

const SECURITY_HEADERS = [
  "cache-control",
  "content-security-policy",
  "cross-origin-resource-policy",
  "permissions-policy",
  "referrer-policy",
  "x-content-type-options",
  "x-frame-options",
];

let runtime;

before(async () => {
  runtime = await startDashboardRuntime();
});

after(async () => {
  await runtime?.close();
});

async function request(path, init) {
  const response = await runtime.fetch(path, init);
  return {
    response,
    body: await response.text(),
  };
}

function assertDashboardResponseIsPrivate(response, path) {
  assert.equal(response.headers.get("cache-control"), "no-store", `${path}: cache policy`);
  assert.notEqual(
    response.headers.get("access-control-allow-origin"),
    "*",
    `${path}: dashboard response must not enable wildcard CORS`,
  );
  assert.equal(
    response.headers.get("content-security-policy")?.includes("default-src 'self'"),
    true,
    `${path}: CSP`,
  );
  assert.equal(response.headers.get("cross-origin-resource-policy"), "same-origin", `${path}: CORP`);
  assert.equal(response.headers.get("x-content-type-options"), "nosniff", `${path}: nosniff`);
  assert.equal(response.headers.get("x-frame-options"), "DENY", `${path}: frame policy`);
  for (const name of SECURITY_HEADERS) {
    assert.ok(response.headers.has(name), `${path}: missing ${name}`);
  }
}

function assertRejectedWithoutDashboardContent({ response, body }, path) {
  assert.ok(
    [401, 403, 404, 405].includes(response.status),
    `${path}: expected an auth denial or not-found response, got ${response.status}`,
  );
  assert.equal(response.headers.has("location"), false, `${path}: unauthenticated request must not redirect`);
  assert.doesNotMatch(body, /Temote Fabric dashboard|REFRESH_FOREGROUND_MS|color-scheme:\s*light/);
}

test("actual Workers Static Assets routing keeps canonical dashboard files behind Access auth", async () => {
  for (const route of DASHBOARD_ROUTES) {
    const unauthenticated = await request(route);
    assert.equal(unauthenticated.response.status, 401, `${route}: missing Access JWT`);
    assertDashboardResponseIsPrivate(unauthenticated.response, route);
    assert.doesNotMatch(
      unauthenticated.body,
      /Temote Fabric dashboard|REFRESH_FOREGROUND_MS|color-scheme:\s*light/,
      `${route}: no dashboard shell or asset content before auth`,
    );

    const clientTokenOnly = await request(route, {
      headers: { authorization: `Bearer ${DASHBOARD_RUNTIME_CLIENT_TOKEN}` },
    });
    assert.equal(clientTokenOnly.response.status, 401, `${route}: CLIENT_TOKEN must not authorize dashboard`);
    assertDashboardResponseIsPrivate(clientTokenOnly.response, route);
    assert.doesNotMatch(
      clientTokenOnly.body,
      /Temote Fabric dashboard|REFRESH_FOREGROUND_MS|color-scheme:\s*light/,
      `${route}: client token must not reveal dashboard content`,
    );

    const hostTokenOnly = await request(route, {
      headers: { authorization: `Bearer ${DASHBOARD_RUNTIME_HOST_TOKEN}` },
    });
    assert.equal(hostTokenOnly.response.status, 401, `${route}: HOST_TOKEN must not authorize dashboard`);
    assertDashboardResponseIsPrivate(hostTokenOnly.response, route);
  }
});

test("dashboard aliases cannot expose Static Assets or escape dashboard response handling", async (context) => {
  for (const alias of DASHBOARD_PATH_ALIASES) {
    for (const [label, headers] of [
      ["without token", undefined],
      ["CLIENT_TOKEN only", { authorization: `Bearer ${DASHBOARD_RUNTIME_CLIENT_TOKEN}` }],
      ["HOST_TOKEN only", { authorization: `Bearer ${DASHBOARD_RUNTIME_HOST_TOKEN}` }],
    ]) {
      await context.test(`${alias} ${label}`, async () => {
        const result = await request(alias, { headers });
        assertRejectedWithoutDashboardContent(result, alias);
        assertDashboardResponseIsPrivate(result.response, alias);
      });
    }

    await context.test(`${alias} OPTIONS`, async () => {
      const preflight = await request(alias, { method: "OPTIONS" });
      assert.notEqual(preflight.response.status, 204, `${alias}: dashboard must not use global OPTIONS handling`);
      assertDashboardResponseIsPrivate(preflight.response, `${alias} OPTIONS`);
    });
  }
});

test("dashboard assets authenticate before redirecting /dash and reject direct index-file aliases", async () => {
  const dash = await request("/dash");
  assert.equal(dash.response.status, 401);
  assert.equal(dash.response.headers.has("location"), false);
  assertDashboardResponseIsPrivate(dash.response, "/dash");

  const index = await request("/dash/index.html");
  assert.equal(index.response.status, 401);
  assertDashboardResponseIsPrivate(index.response, "/dash/index.html");
});

test("existing /mcp CLIENT_TOKEN and /healthz behavior still works in the real Worker runtime", async () => {
  const initialize = await request("/mcp", {
    method: "POST",
    headers: {
      authorization: `Bearer ${DASHBOARD_RUNTIME_CLIENT_TOKEN}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "dashboard-assets-runtime-test", version: "1" },
      },
    }),
  });
  assert.equal(initialize.response.status, 200);
  assert.equal(initialize.response.headers.get("access-control-allow-origin"), "*");
  assert.match(initialize.body, /"name":"temote-mcp-gateway"/);

  const health = await request("/healthz");
  assert.equal(health.response.status, 200);
  assert.equal(health.response.headers.get("access-control-allow-origin"), "*");
  const healthPayload = JSON.parse(health.body);
  assert.equal(healthPayload.status, "ok");
  assert.equal(healthPayload.service, "temote-mcp-gateway");
  assert.equal(healthPayload.readiness, "ready");
  assert.equal(healthPayload.identity, "temote-mcp-gateway");
  assert.match(healthPayload.contractFingerprint, /^[0-9a-f]{64}$/);
});
