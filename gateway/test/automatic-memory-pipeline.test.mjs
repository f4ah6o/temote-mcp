import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import test from "node:test";

import { projectSafeContextStatus } from "../scripts/memory-dogfood.mjs";
import {
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  MEMORY_TEST_REPOSITORY,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const POLICY_QUOTE = "Report output format must be JSON.";
const UNRESOLVED_CLAUSE = "Open question: The required report field set remains undecided.";
const UNRESOLVED_TEXT = "The required report field set remains undecided.";
const CONTENT = [
  "For this repository, the repository-level policy is:",
  POLICY_QUOTE,
  UNRESOLVED_CLAUSE,
  "Implement report.py with render_report(fields), preserve the passed field mapping as-is, add one focused test, and do not change other files.",
].join("\n");
const SESSION = "automatic-memory-session";
const TASK = "automatic-memory-task";
const OPERATION = "550e8400-e29b-41d4-a716-446655440000";

test("failure diagnostics keep only bounded context status fields and stable error codes", () => {
  const projected = projectSafeContextStatus({
    status: 200,
    body: { result: { content: [{
      type: "text",
      text: JSON.stringify({
        memory: {
          state: "failed",
          enabled: true,
          worker_last_cloud_seq: 4,
          latest_cloud_seq: 9,
          worker_lag: 5,
          last_error_code: "provider_timeout; sensitive output omitted",
          stale: true,
          provider_response: "should never be retained",
        },
        freshness: {
          source_count: 2,
          source_head_revision: 8,
          source_acked_revision: 8,
          source_gap_count: 0,
          observation_count: 9,
          worker_state: "failed",
          worker_last_cloud_seq: 4,
          worker_lag: 5,
          knowledge_stale: true,
          source_cursors: [{ session_id: "not copied into diagnostics" }],
        },
        task_body: "not copied into diagnostics",
      }),
    }] } },
  });
  assert.equal(projected.state, "failed");
  assert.equal(projected.last_error_code, "other");
  assert.equal(projected.worker_last_cloud_seq, 4);
  assert.equal(projected.latest_cloud_seq, 9);
  assert.equal(projected.worker_lag, 5);
  assert.equal(projected.freshness.source_acked_revision, 8);
  assert.equal(projected.freshness.observation_count, 9);
  assert.equal(JSON.stringify(projected).includes("sensitive output omitted"), false);
  assert.equal(JSON.stringify(projected).includes("provider_response"), false);
  assert.equal(JSON.stringify(projected).includes("task_body"), false);
});

function observation() {
  const sha256 = createHash("sha256").update(CONTENT).digest("hex");
  return {
    id: randomUUID(),
    schema_version: 1,
    observed_at: Math.floor(Date.now() / 1000),
    session_id: SESSION,
    session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "codex" },
    action: "task_start",
    kind: "instruction",
    task_id: TASK,
    operation_id: OPERATION,
    content: {
      kind: "text",
      preview: CONTENT,
      total_bytes: Buffer.byteLength(CONTENT),
      sha256,
      truncated: false,
    },
    evidence_refs: [],
    provenance: { tool: "codex_task_start", source: "orchestration" },
    revision: 1,
    dedupe_key: "instruction:" + SESSION + ":1",
  };
}

async function mcpValue(runtime, name, args) {
  const response = await runtime.callMcp(MEMORY_TEST_CLIENT_TOKEN, name, args, 17);
  assert.equal(response.status, 200, `${name} should return HTTP 200`);
  assert.equal(response.body.error, undefined, `${name} should not return an RPC error`);
  const text = response.body.result?.content?.find((item) => item.type === "text")?.text;
  assert.equal(typeof text, "string", `${name} should return a text result`);
  return JSON.parse(text);
}

test("host sync automatically drives the real D1 queue consumer into repository context", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    bindings: {
      OBSERVATION_OWNER_ID: MEMORY_TEST_OWNER,
      HOST_TOKENS_JSON: JSON.stringify({ [MEMORY_TEST_HOST_ID]: MEMORY_TEST_HOST_TOKEN }),
      CLIENT_TOKEN: MEMORY_TEST_CLIENT_TOKEN,
    },
  });

  try {
    const record = observation();
    const syncBody = {
      schema_version: 1,
      session_id: SESSION,
      repository_key: MEMORY_TEST_REPOSITORY,
      source_base_revision: 0,
      source_head_revision: 1,
      journal_degraded: false,
      gap_count: 0,
      records: [{ source_revision: 1, observation: record }],
    };
    const response = await runtime.fetch(
      `https://memory-test.local/v1/hosts/${MEMORY_TEST_HOST_ID}/observations/sync`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${MEMORY_TEST_HOST_TOKEN}`,
          "content-type": "application/json",
          "x-temote-host-id": MEMORY_TEST_HOST_ID,
        },
        body: JSON.stringify(syncBody),
      },
    );
    assert.equal(response.status, 200, "authenticated host sync should commit the observation");
    const sync = await response.json();
    assert.equal(sync.acked_through_revision, 1);
    assert.equal(sync.complete, true);

    await runtime.waitFor(async () => {
      const checkpoints = await runtime.querySql(
        "SELECT last_cloud_seq, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoints.length === 1 && Number(checkpoints[0].last_cloud_seq) === Number(sync.cloud_head_seq)
        && checkpoints[0].last_error_code == null;
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const [sources, observations, runs, items, supports, outbox] = await Promise.all([
      runtime.querySql(
        "SELECT host_id, session_id, repository_key, acked_through_revision FROM observation_sources WHERE owner_id = ? AND session_id = ?",
        [MEMORY_TEST_OWNER, SESSION],
      ),
      runtime.querySql(
        "SELECT cloud_seq, observation_id, kind, task_id, operation_id FROM observations WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT status, outcome, input_count FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, kind, text, status, scope_type, scope_id FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, observation_cloud_seq, observation_id FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT queued_at, attempt_count, last_error_code FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
    ]);

    assert.equal(sources.length, 1);
    assert.equal(sources[0].host_id, MEMORY_TEST_HOST_ID);
    assert.equal(sources[0].acked_through_revision, 1);
    assert.equal(observations.length, 1);
    assert.equal(observations[0].task_id, TASK);
    assert.equal(observations[0].operation_id, OPERATION);
    assert.deepEqual(runs.map((run) => run.status), ["completed"]);
    assert.equal(runs[0].outcome, "projected");
    const policy = items.find((item) => item.kind === "constraint" && item.text === POLICY_QUOTE);
    assert.ok(policy, "the normal instruction should produce its supported repository constraint");
    assert.equal(policy.scope_type, "repository");
    assert.equal(policy.scope_id, MEMORY_TEST_REPOSITORY);
    assert.equal(policy.status, "current");
    const policySupport = supports.find((support) => support.knowledge_id === policy.knowledge_id);
    assert.ok(policySupport, "the policy must have a concrete support ref");
    assert.equal(policySupport.observation_id, record.id);
    assert.equal(outbox.length, 1);
    assert.equal(outbox[0].queued_at != null, true);
    assert.equal(outbox[0].last_error_code, null);

    const context = await mcpValue(runtime, "context_resolve", {
      repository: MEMORY_TEST_REPOSITORY,
      limit: 8,
    });
    const values = JSON.stringify(context);
    assert.match(values, /replicated_observed|derived/);
    assert.match(values, /Report output format must be JSON\./);
    assert.match(values, new RegExp(UNRESOLVED_TEXT));
    assert.match(values, /current/);
    assert.match(values, new RegExp(record.id));

    const replay = await runtime.fetch(
      `https://memory-test.local/v1/hosts/${MEMORY_TEST_HOST_ID}/observations/sync`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${MEMORY_TEST_HOST_TOKEN}`,
          "content-type": "application/json",
          "x-temote-host-id": MEMORY_TEST_HOST_ID,
        },
        body: JSON.stringify(syncBody),
      },
    );
    assert.equal(replay.status, 200, "an exact repeated observation batch should be acknowledged");
    const replayBody = await replay.json();
    assert.equal(replayBody.acked_through_revision, sync.acked_through_revision);
    assert.equal(replayBody.cloud_head_seq, sync.cloud_head_seq);

    const [itemsBefore, supportsBefore, runsBefore, checkpointBefore, queueBefore] = await Promise.all([
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_items WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM memory_runs WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT MAX(last_cloud_seq) AS seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.queueDispatchCount(),
    ]);
    const duplicateWake = await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: Number(sync.cloud_head_seq),
    });
    assert.equal(duplicateWake.success, true);
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > queueBefore && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });
    const [itemsAfter, supportsAfter, runsAfter, checkpointAfter] = await Promise.all([
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_items WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM memory_runs WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT MAX(last_cloud_seq) AS seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
    ]);
    assert.equal(itemsAfter[0].count, itemsBefore[0].count, "Queue redelivery must not add knowledge rows");
    assert.equal(supportsAfter[0].count, supportsBefore[0].count, "Queue redelivery must not add support rows");
    assert.equal(runsAfter[0].count, runsBefore[0].count, "Queue redelivery must not add completed runs");
    assert.equal(checkpointAfter[0].seq, checkpointBefore[0].seq, "Queue redelivery must not move the checkpoint");

    const unrelated = await mcpValue(runtime, "context_resolve", {
      repository: "github:temote-tests/memory-continuity-unrelated",
      query: "report output format",
      limit: 8,
    });
    assert.doesNotMatch(JSON.stringify(unrelated), /Report output format must be JSON\./);
  } finally {
    await runtime.dispose();
  }
});
