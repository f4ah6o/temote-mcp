import test from "node:test";
import assert from "node:assert/strict";
import { validateSessionStart } from "../src/session-start.js";

test("source start is receipt-bound and XOR with the legacy logical-path form", () => {
  const operation_id = "ce7232f3-8c89-4055-a568-04e0a2e107d0";
  for (const source of ["owner/repository", { kind: "repository", repository: "owner/repository", vcs: "auto", base: "main" }]) {
    assert.equal(validateSessionStart({ host_id: "host", source, operation_id }), null);
    assert.ok(validateSessionStart({ host_id: "host", source }));
    assert.ok(validateSessionStart({ host_id: "host", source, operation_id, path: "src/repo" }));
  }
  assert.equal(validateSessionStart({ host_id: "host", path: "src/repo" }), null);
  for (const args of [{}, { path: null }, { path: "" }, { source: {} , operation_id },
    { source: { kind: "repository", repository: "owner/repo", executable: "sh" }, operation_id },
    { source: "owner/repo", operation_id: "not-uuid" },
    { path: "src/repo", yolo: true }]) assert.ok(validateSessionStart(args));
});
