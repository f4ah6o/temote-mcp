import assert from "node:assert/strict";
import test, { after, before } from "node:test";

import {
  DASHBOARD_RUNTIME_CLIENT_TOKEN,
  DASHBOARD_RUNTIME_FEDERATED_TOKEN,
  DASHBOARD_RUNTIME_HOST_TOKEN,
  createDashboardAccessFixture,
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
  return requestWith(runtime, path, init);
}

async function requestWith(targetRuntime, path, init) {
  const response = await targetRuntime.fetch(path, init);
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
    assert.equal(unauthenticated.response.headers.has("location"), false, `${route}: no redirect before auth`);

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

    const federatedHostTokenOnly = await request(route, {
      headers: { authorization: `Bearer ${DASHBOARD_RUNTIME_FEDERATED_TOKEN}` },
    });
    assert.equal(federatedHostTokenOnly.response.status, 401, `${route}: federated host token must not authorize dashboard`);
    assertDashboardResponseIsPrivate(federatedHostTokenOnly.response, route);

  }
});

test("dashboard aliases cannot expose Static Assets or escape dashboard response handling", async (context) => {
  for (const alias of DASHBOARD_PATH_ALIASES) {
    for (const [label, headers] of [
      ["without token", undefined],
      ["CLIENT_TOKEN only", { authorization: `Bearer ${DASHBOARD_RUNTIME_CLIENT_TOKEN}` }],
      ["HOST_TOKEN only", { authorization: `Bearer ${DASHBOARD_RUNTIME_HOST_TOKEN}` }],
      ["federated host token only", { authorization: `Bearer ${DASHBOARD_RUNTIME_FEDERATED_TOKEN}` }],
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

test("a verified Access JWT reaches the real configured Static Assets binding", async () => {
  const fixture = await createDashboardAccessFixture();
  const assertion = await fixture.sign();
  const validHeaders = { "cf-access-jwt-assertion": assertion };
  const accessRuntime = await startDashboardRuntime({ accessFixture: fixture });

  try {
    const redirect = await requestWith(accessRuntime, "/dash", { headers: validHeaders });
    assert.equal(redirect.response.status, 308);
    assert.equal(redirect.response.headers.get("location"), "/dash/");
    assertDashboardResponseIsPrivate(redirect.response, "/dash");

    const shell = await requestWith(accessRuntime, "/dash/", { headers: validHeaders });
    assert.equal(shell.response.status, 200);
    assert.match(shell.response.headers.get("content-type") ?? "", /text\/html/i);
    assert.match(shell.body, /<title>Temote Fabric dashboard<\/title>/);
    assert.match(shell.body, /<script type="module" src="\/dash\/app\.js"><\/script>/);
    assertDashboardResponseIsPrivate(shell.response, "/dash/");

    const app = await requestWith(accessRuntime, "/dash/app.js", { headers: validHeaders });
    assert.equal(app.response.status, 200);
    assert.match(app.response.headers.get("content-type") ?? "", /javascript/i);
    assert.match(app.body, /REFRESH_FOREGROUND_MS/);
    assertDashboardResponseIsPrivate(app.response, "/dash/app.js");

    const styles = await requestWith(accessRuntime, "/dash/styles.css", { headers: validHeaders });
    assert.equal(styles.response.status, 200);
    assert.match(styles.response.headers.get("content-type") ?? "", /text\/css/i);
    assert.match(styles.body, /--ink:/);
    assertDashboardResponseIsPrivate(styles.response, "/dash/styles.css");

    for (const alias of [
      "/%64ash/",
      "/%64ash/app.js",
      "/dash%2fapp.js",
      "/DASH/app.js",
      "/dash//app.js",
      "/dash/app.js/",
      "/dash/index.html",
      "/dash/unknown/index.html",
    ]) {
      const result = await requestWith(accessRuntime, alias, { headers: validHeaders });
      assert.equal(result.response.status, 404, `${alias}: aliases and direct index paths must not fall through to an asset`);
      assert.doesNotMatch(result.body, /Temote Fabric dashboard|REFRESH_FOREGROUND_MS|color-scheme:\s*light/);
      assertDashboardResponseIsPrivate(result.response, alias);
    }

    const authorizedOptions = await requestWith(accessRuntime, "/dash/api/v1/bootstrap", {
      method: "OPTIONS",
      headers: validHeaders,
    });
    assert.equal(authorizedOptions.response.status, 405);
    assertDashboardResponseIsPrivate(authorizedOptions.response, "authorized dashboard OPTIONS");

    const assertionParts = assertion.split(".");
    const changedSignature = assertionParts[2][0] === "A" ? "B" : "A";
    assertionParts[2] = `${changedSignature}${assertionParts[2].slice(1)}`;
    const invalidJwt = await requestWith(accessRuntime, "/dash/app.js", {
      headers: { "cf-access-jwt-assertion": assertionParts.join(".") },
    });
    assert.equal(invalidJwt.response.status, 401);
    assertRejectedWithoutDashboardContent(invalidJwt, "/dash/app.js with invalid signature");
    assertDashboardResponseIsPrivate(invalidJwt.response, "/dash/app.js with invalid signature");
  } finally {
    await accessRuntime.close();
  }
});
