import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import test from "node:test";

import worker from "../src/index.js";
import { isDashboardPathname } from "../src/dashboard/index.js";

const TEXT_ENCODER = new TextEncoder();

function base64url(bytes) {
  return Buffer.from(bytes).toString("base64url");
}

async function makeAccessFixture(overrides = {}) {
  const team = `team-${randomUUID()}.cloudflareaccess.com`;
  const audience = "dashboard-test-audience";
  const keyPair = await crypto.subtle.generateKey({
    name: "RSASSA-PKCS1-v1_5",
    modulusLength: 2048,
    publicExponent: new Uint8Array([1, 0, 1]),
    hash: "SHA-256",
  }, true, ["sign", "verify"]);
  const publicKey = await crypto.subtle.exportKey("jwk", keyPair.publicKey);
  publicKey.kid = "dashboard-test-key";
  publicKey.alg = "RS256";
  publicKey.use = "sig";

  async function sign(claimOverrides = {}) {
    const claims = {
      iss: `https://${team}`,
      aud: audience,
      exp: Math.floor(Date.now() / 1000) + 3600,
      nbf: Math.floor(Date.now() / 1000) - 1,
      sub: "dashboard-user-1",
      email: "operator@example.test",
      ...overrides,
      ...claimOverrides,
    };
    const headerPart = base64url(TEXT_ENCODER.encode(JSON.stringify({
      alg: "RS256",
      kid: publicKey.kid,
      typ: "JWT",
    })));
    const claimsPart = base64url(TEXT_ENCODER.encode(JSON.stringify(claims)));
    const signingInput = `${headerPart}.${claimsPart}`;
    const signature = await crypto.subtle.sign(
      { name: "RSASSA-PKCS1-v1_5" },
      keyPair.privateKey,
      TEXT_ENCODER.encode(signingInput),
    );
    return `${signingInput}.${base64url(signature)}`;
  }

  return {
    team,
    audience,
    assertion: await sign(),
    sign,
    publicKey,
  };
}

function dashboardRequest(path, { method = "GET", assertion, authorization } = {}) {
  const headers = new Headers();
  if (assertion !== undefined) headers.set("cf-access-jwt-assertion", assertion);
  if (authorization !== undefined) headers.set("authorization", authorization);
  return new Request(`https://fabric.example.test${path}`, { method, headers });
}

function dashboardEnv(fixture, extras = {}) {
  return {
    ACCESS_TEAM_DOMAIN: fixture.team,
    ACCESS_AUDIENCE: fixture.audience,
    ACCESS_ALLOWED_EMAILS: "operator@example.test",
    CLIENT_TOKEN: "shared-client-token",
    HOST_TOKEN: "legacy-host-token",
    HOST_TOKENS_JSON: JSON.stringify({ "mac-main": "federated-host-token" }),
    ...extras,
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

function assertDashboardHeaders(response) {
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(response.headers.get("x-content-type-options"), "nosniff");
  assert.equal(response.headers.get("x-frame-options"), "DENY");
  assert.match(response.headers.get("content-security-policy") ?? "", /default-src 'self'/);
  assert.equal(response.headers.get("cross-origin-resource-policy"), "same-origin");
  for (const [name] of response.headers) {
    assert.equal(name.toLowerCase().startsWith("access-control-"), false, name);
  }
}

test("dashboard rejects missing JWT and every shared token fallback before serving content", async () => {
  const fixture = await makeAccessFixture();
  let assetCalls = 0;
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async () => {
      assetCalls += 1;
      return new Response("<main>private dashboard shell</main>", {
        headers: { "content-type": "text/html" },
      });
    } },
  });

  await withJwks(fixture, async () => {
    for (const authorization of [
      undefined,
      "Bearer shared-client-token",
      "Bearer legacy-host-token",
      "Bearer federated-host-token",
    ]) {
      for (const path of [
        "/dash",
        "/dash/",
        "/dash/app.js",
        "/dash/styles.css",
        "/dash/api/v1/bootstrap",
        "/dash/unknown/path",
      ]) {
        const response = await worker.fetch(dashboardRequest(path, { authorization }), env);
        assert.equal(response.status, 401, `${authorization} ${path}`);
        assert.deepEqual(await response.json(), { error_code: "access_unauthorized" });
        assertDashboardHeaders(response);
      }
    }
  });

  assert.equal(assetCalls, 0);
});

test("dashboard route classification catches aliases without normalizing them into assets", () => {
  for (const path of [
    "/dash",
    "/dash/",
    "/DASH/",
    "//dash//app.js",
    "/%64ash/",
    "/dash%2fapp.js",
    "/dash%5capp.js",
    "/dash%",
  ]) {
    assert.equal(isDashboardPathname(path), true, path);
  }
  for (const path of ["/dashboard", "/api/dash", "/healthz", "/mcp"]) {
    assert.equal(isDashboardPathname(path), false, path);
  }
});

test("CLIENT_TOKEN plus an invalid or expired JWT is rejected before static assets", async () => {
  const fixture = await makeAccessFixture();
  let assetCalls = 0;
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async () => {
      assetCalls += 1;
      return new Response("private shell");
    } },
  });
  const expired = await fixture.sign({ exp: Math.floor(Date.now() / 1000) - 10 });
  const wrongSignature = `${fixture.assertion.slice(0, -8)}AAAAAAAA`;
  const malformedClaims = [
    base64url(TEXT_ENCODER.encode(JSON.stringify({ alg: "RS256", kid: "dashboard-test-key" }))),
    base64url(TEXT_ENCODER.encode('{"secret-marker":')),
    "AA",
  ].join(".");
  const originalError = console.error;
  const errorLines = [];
  console.error = (...args) => errorLines.push(args.map(String).join(" "));

  try {
    await withJwks(fixture, async () => {
      for (const assertion of [malformedClaims, wrongSignature, expired]) {
        const response = await worker.fetch(dashboardRequest("/dash/", {
          authorization: "Bearer shared-client-token",
          assertion,
        }), env);
        assert.equal(response.status, 401);
        assertDashboardHeaders(response);
      }
    });
  } finally {
    console.error = originalError;
  }

  assert.equal(assetCalls, 0);
  assert.equal(errorLines.some((line) => line.includes("secret-marker")), false);
});

test("dashboard verifies Access signature, issuer, audience, expiry, subject, and email", async () => {
  const fixture = await makeAccessFixture();
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async () => new Response("dashboard shell", {
      headers: { "content-type": "text/html" },
    }) },
  });

  await withJwks(fixture, async () => {
    const valid = await worker.fetch(dashboardRequest("/dash/", {
      assertion: fixture.assertion,
    }), env);
    assert.equal(valid.status, 200);
    assert.equal(await valid.text(), "dashboard shell");
    assertDashboardHeaders(valid);

    for (const claims of [
      { iss: "https://wrong.cloudflareaccess.com" },
      { aud: "wrong-audience" },
      { exp: Math.floor(Date.now() / 1000) - 10 },
      { nbf: Math.floor(Date.now() / 1000) + 120 },
      { sub: "" },
      { email: "other@example.test" },
    ]) {
      const invalidAssertion = await fixture.sign(claims);
      const response = await worker.fetch(dashboardRequest("/dash/", {
        assertion: invalidAssertion,
      }), env);
      assert.equal(response.status, 401, JSON.stringify(claims));
      assertDashboardHeaders(response);
    }

    const withoutNbf = await fixture.sign({ nbf: undefined });
    const acceptedWithoutNbf = await worker.fetch(dashboardRequest("/dash/", {
      assertion: withoutNbf,
    }), env);
    assert.equal(acceptedWithoutNbf.status, 200);
  });
});

test("dashboard gates assets, redirects, API paths, errors, and methods before global OPTIONS handling", async () => {
  const fixture = await makeAccessFixture();
  const assetPaths = [];
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async (request) => {
      assetPaths.push(new URL(request.url).pathname);
      return new Response("static shell", {
        headers: {
          "content-type": "text/html",
          "access-control-allow-origin": "*",
        },
      });
    } },
  });

  await withJwks(fixture, async () => {
    const noAuth = await worker.fetch(dashboardRequest("/dash", { method: "OPTIONS" }), env);
    assert.equal(noAuth.status, 401);
    assertDashboardHeaders(noAuth);

    const authorizedOptions = await worker.fetch(dashboardRequest("/dash/api/v1/bootstrap", {
      method: "OPTIONS",
      assertion: fixture.assertion,
    }), env);
    assert.equal(authorizedOptions.status, 403);
    assert.deepEqual(await authorizedOptions.json(), { error_code: "browser_data_surface_unavailable" });
    assertDashboardHeaders(authorizedOptions);

    const redirect = await worker.fetch(dashboardRequest("/dash", {
      assertion: fixture.assertion,
    }), env);
    assert.equal(redirect.status, 308);
    assert.equal(redirect.headers.get("location"), "/dash/");
    assertDashboardHeaders(redirect);

    const shell = await worker.fetch(dashboardRequest("/dash/", {
      assertion: fixture.assertion,
    }), env);
    assert.equal(shell.status, 200);
    assertDashboardHeaders(shell);

    for (const path of ["/dash/app.js", "/dash/styles.css"]) {
      const asset = await worker.fetch(dashboardRequest(path, {
        assertion: fixture.assertion,
      }), env);
      assert.equal(asset.status, 200, path);
      assertDashboardHeaders(asset);
    }

    for (const path of ["/%64ash/", "/dash%2fapp.js", "/dash%5cstyles.css", "/DASH/"]) {
      const alias = await worker.fetch(dashboardRequest(path, {
        assertion: fixture.assertion,
      }), env);
      assert.equal(alias.status, 403, path);
      assertDashboardHeaders(alias);
    }

    for (const path of ["/dash/", "/dash/app.js", "/dash/styles.css", "/dash/api/v1/bootstrap"]) {
      const post = await worker.fetch(dashboardRequest(path, {
        method: "POST",
        assertion: fixture.assertion,
      }), env);
      assert.equal(post.status, 403, path);
      assertDashboardHeaders(post);
    }

    const bootstrapWithoutDeployment = await worker.fetch(dashboardRequest("/dash/api/v1/bootstrap", {
      assertion: fixture.assertion,
    }), env);
    assert.equal(bootstrapWithoutDeployment.status, 403);
    assert.equal((await bootstrapWithoutDeployment.json()).error_code, "browser_data_surface_unavailable");
    assertDashboardHeaders(bootstrapWithoutDeployment);

    for (const path of ["/dash/unknown/path"]) {
      const notFound = await worker.fetch(dashboardRequest(path, {
        assertion: fixture.assertion,
      }), env);
      assert.equal(notFound.status, 403, path);
      assert.deepEqual(await notFound.json(), { error_code: "browser_data_surface_unavailable" });
      assertDashboardHeaders(notFound);
    }
  });

  assert.deepEqual(assetPaths, ["/dash/index.html", "/dash/app.js", "/dash/styles.css"]);
});

test("dashboard asset binding exceptions return a private generic 500", async () => {
  const fixture = await makeAccessFixture();
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async () => { throw new Error("secret asset marker"); } },
  });

  const response = await withJwks(fixture, () => worker.fetch(dashboardRequest("/dash/", {
    assertion: fixture.assertion,
  }), env));
  assert.equal(response.status, 500);
  assert.deepEqual(await response.json(), { error_code: "internal_error" });
  assertDashboardHeaders(response);
});

test("dashboard does not change CLIENT_TOKEN compatibility on /mcp", async () => {
  const fixture = await makeAccessFixture();
  let assetCalls = 0;
  const env = dashboardEnv(fixture, {
    ASSETS: { fetch: async () => {
      assetCalls += 1;
      return new Response("private dashboard shell");
    } },
  });
  const mcp = new Request("https://fabric.example.test/mcp", {
    method: "POST",
    headers: {
      authorization: "Bearer shared-client-token",
      "content-type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
  });

  const response = await worker.fetch(mcp, env);
  assert.equal(response.status, 200);
  assert.equal((await response.json()).result.tools.length > 0, true);
  assert.equal(assetCalls, 0);
});
