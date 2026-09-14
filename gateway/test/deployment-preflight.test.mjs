import test from "node:test";
import assert from "node:assert/strict";

import { evaluateDeploymentPreflight } from "../scripts/deployment-preflight.mjs";

const CONFIG = "workers_dev = false\n";

test("reports a configured route but keeps remote verification unknown", () => {
  const result = evaluateDeploymentPreflight({
    configText: CONFIG,
    hostname: "Gateway.Example.com",
    route: "gateway.example.com/*",
  });

  assert.equal(result.status, "remote_unknown");
  assert.equal(result.target.status, "target_present");
  assert.equal(result.remote.status, "unknown");
});

test("distinguishes a missing target from remote verification", () => {
  const result = evaluateDeploymentPreflight({
    configText: CONFIG,
    hostname: "gateway.example.com",
    customDomain: undefined,
  });

  assert.equal(result.status, "target_missing");
  assert.equal(result.target.status, "target_missing");
  assert.equal(result.remote.status, "not_checked");
});

test("reports an intended-hostname mismatch without contacting Cloudflare", () => {
  const result = evaluateDeploymentPreflight({
    configText: CONFIG,
    hostname: "gateway.example.com",
    route: "other.example.com/*",
  });

  assert.equal(result.status, "target_mismatch");
  assert.equal(result.target.status, "target_mismatch");
  assert.equal(result.remote.status, "not_checked");
});

test("rejects workers.dev configuration before evaluating a target", () => {
  const result = evaluateDeploymentPreflight({
    configText: "workers_dev = true\n",
    hostname: "gateway.example.com",
    route: "gateway.example.com/*",
  });

  assert.equal(result.status, "invalid_config");
  assert.equal(result.target.status, "not_checked");
});

test("does not include target values or configuration paths in the result", () => {
  const result = evaluateDeploymentPreflight({
    configText: CONFIG,
    hostname: "gateway.example.com",
    customDomain: "other.example.com",
  });

  assert.equal(result.status, "target_mismatch");
  assert.equal(JSON.stringify(result).includes("other.example.com"), false);
});
