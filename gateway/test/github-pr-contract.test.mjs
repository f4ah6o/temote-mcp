import test from "node:test";
import assert from "node:assert/strict";

import { PUBLIC_TOOLS, stripContractProse } from "../src/protocol.js";

const tools = stripContractProse(PUBLIC_TOOLS);
const names = tools.map(({ name }) => name);
const prTools = tools.filter(({ name }) => name.startsWith("github_pr_"));
const sessionProperties = {
  host_id: { type: "string" },
  session_id: { type: "string" },
};

test("exposes exactly three GitHub PR tools in the required order", () => {
  assert.deepEqual(
    prTools.map(({ name }) => name),
    ["github_pr_list", "github_pr_get", "github_pr_close"],
  );
  assert.equal(new Set(names).size, names.length);
  const workflowIndex = names.indexOf("github_workflow_run_get");
  const executeIndex = names.indexOf("execute");
  assert.deepEqual(names.slice(workflowIndex + 1, executeIndex), [
    "github_pr_list",
    "github_pr_get",
    "github_pr_close",
  ]);
});

test("GitHub PR tools have exact stripped input shapes", () => {
  const common = {
    type: "object",
    additionalProperties: false,
  };
  assert.deepEqual(prTools.map(({ name, inputSchema }) => ({ name, inputSchema })), [
    {
      name: "github_pr_list",
      inputSchema: {
        ...common,
        properties: {
          ...sessionProperties,
          cwd: { type: "string" },
          remote: { type: "string", default: "origin" },
        },
        required: ["session_id"],
      },
    },
    {
      name: "github_pr_get",
      inputSchema: {
        ...common,
        properties: {
          ...sessionProperties,
          cwd: { type: "string" },
          remote: { type: "string", default: "origin" },
          number: { type: "string", minLength: 1, maxLength: 20 },
        },
        required: ["session_id", "number"],
      },
    },
    {
      name: "github_pr_close",
      inputSchema: {
        ...common,
        properties: {
          ...sessionProperties,
          cwd: { type: "string" },
          remote: { type: "string", default: "origin" },
          number: { type: "string", minLength: 1, maxLength: 20 },
        },
        required: ["session_id", "number"],
      },
    },
  ]);
});

test("GitHub PR tools declare exact annotations", () => {
  assert.deepEqual(prTools.map(({ name, annotations }) => ({ name, annotations })), [
    {
      name: "github_pr_list",
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    {
      name: "github_pr_get",
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    {
      name: "github_pr_close",
      annotations: {
        readOnlyHint: false,
        destructiveHint: true,
        idempotentHint: false,
        openWorldHint: true,
      },
    },
  ]);
});

test("GitHub PR schemas expose no fields outside their exact allowlists", () => {
  const allowlists = {
    github_pr_list: ["host_id", "session_id", "cwd", "remote"],
    github_pr_get: ["host_id", "session_id", "cwd", "remote", "number"],
    github_pr_close: ["host_id", "session_id", "cwd", "remote", "number"],
  };
  const forbidden = ["token", "url", "repo", "method", "body", "path", "owner"];

  for (const tool of prTools) {
    assert.deepEqual(Object.keys(tool.inputSchema.properties), allowlists[tool.name]);
    for (const field of forbidden) {
      assert.equal(field in tool.inputSchema.properties, false);
    }
  }
});
