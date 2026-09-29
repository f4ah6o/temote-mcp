import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import test from "node:test";

import { memoryConfiguration } from "../src/memory/config.js";
import {
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_OWNER,
  MEMORY_TEST_REPOSITORY,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const SESSION = "d1-context-session";
const TASK = "d1-context-task";
const OPERATION = "550e8400-e29b-41d4-a716-446655440000";
const STAMP = "2026-09-20T00:00:00.000Z";

async function contextValue(runtime, args) {
  return contextToolValue(runtime, "context_resolve", args, 21);
}

async function contextToolValue(runtime, name, args, id = 21) {
  const response = await runtime.callMcp(MEMORY_TEST_CLIENT_TOKEN, name, args, id);
  assert.equal(response.status, 200);
  assert.equal(response.body.error, undefined);
  const text = response.body.result?.content?.find((item) => item.type === "text")?.text;
  assert.equal(typeof text, "string");
  return JSON.parse(text);
}

test("D1 resolver safely joins earlier execution support after task acceptance", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({ memoryExtractor: "fixture", memoryEnabled: true });
  try {
    const config = await memoryConfiguration(runtime.bindings);
    await runtime.executeSql(
      "INSERT INTO observation_sources (owner_id, host_id, session_id, repository_key, source_base_revision, source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count, last_synced_at) VALUES (?, ?, ?, ?, 0, 2, 2, 2, 0, 0, ?)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, SESSION, MEMORY_TEST_REPOSITORY, STAMP],
    );
    await runtime.executeSql(
      "INSERT INTO observations (cloud_seq, owner_id, host_id, session_id, observation_id, source_revision, schema_version, repository_key, workspace_id, operation_id, kind, evidence_refs, observed_at, ingested_at, payload_digest) VALUES (1, ?, ?, ?, 'instruction-before-accept', 1, 1, ?, 'workspace-d1', ?, 'instruction', '[]', ?, ?, ?)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, SESSION, MEMORY_TEST_REPOSITORY, OPERATION, STAMP, STAMP, "a".repeat(64)],
    );
    await runtime.executeSql(
      "INSERT INTO observations (cloud_seq, owner_id, host_id, session_id, observation_id, source_revision, schema_version, repository_key, workspace_id, task_id, operation_id, kind, evidence_refs, observed_at, ingested_at, payload_digest) VALUES (2, ?, ?, ?, 'task-accepted-later', 2, 1, ?, 'workspace-d1', ?, ?, 'operation_accepted', '[]', ?, ?, ?)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, SESSION, MEMORY_TEST_REPOSITORY, TASK, OPERATION, STAMP, STAMP, "b".repeat(64)],
    );
    await runtime.executeSql(
      "INSERT INTO memory_projection_heads (owner_id, repository_key, requested_generation, requested_producer_version, active_generation, active_producer_version, epoch, published_at) VALUES (?, ?, ?, ?, ?, ?, 7, ?)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, 1, config.producerVersion, 1, config.producerVersion, STAMP],
    );
    await runtime.executeSql(
      "INSERT INTO memory_checkpoints (owner_id, repository_key, worker_id, producer_version, last_cloud_seq, last_success_at, stale, projection_epoch) VALUES (?, ?, 'temote-memory-v1', ?, 2, ?, 0, 7)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, config.producerVersion, STAMP],
    );
    await runtime.executeSql(
      "INSERT INTO knowledge_items (knowledge_id, owner_id, repository_key, scope_type, scope_id, kind, semantic_key, text, status, confidence, valid_from, producer, producer_version, produced_at, source_through_cloud_seq) VALUES ('operation-knowledge', ?, ?, 'execution', ?, 'constraint', 'constraint:task-boundary', 'Keep this task inside its explicit boundary.', 'supported', 0.9, ?, 'fixture', ?, ?, 2)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, `operation:${OPERATION}`, STAMP, config.producerVersion, STAMP],
    );
    await runtime.executeSql(
      "INSERT INTO knowledge_support (owner_id, repository_key, knowledge_id, observation_cloud_seq, observation_id, support_role) VALUES (?, ?, 'operation-knowledge', 1, 'instruction-before-accept', 'instruction_quote')",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );

    const taskContext = await contextValue(runtime, {
      repository: MEMORY_TEST_REPOSITORY,
      task_id: TASK,
    });
    assert.equal(taskContext.constraints.length, 1);
    assert.equal(taskContext.constraints[0].knowledge_id, "operation-knowledge");
    assert.equal(taskContext.constraints[0].support_refs[0].task_id, TASK);
    assert.equal(JSON.stringify(taskContext).includes("Keep this task inside"), true);

    const otherTask = await contextValue(runtime, {
      repository: MEMORY_TEST_REPOSITORY,
      task_id: "unrelated-task",
    });
    assert.equal(otherTask.constraints.length, 0);
    const repositoryContext = await contextValue(runtime, { repository: MEMORY_TEST_REPOSITORY });
    assert.equal(repositoryContext.constraints.length, 0, "execution scope is not promoted to repository scope");

    await runtime.executeSql(
      "UPDATE knowledge_items SET support_incomplete = 1 WHERE owner_id = ? AND repository_key = ? AND knowledge_id = 'operation-knowledge'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    const incompleteTaskContext = await contextValue(runtime, {
      repository: MEMORY_TEST_REPOSITORY,
      task_id: TASK,
    });
    assert.equal(incompleteTaskContext.constraints[0].support_incomplete, true);
    assert.equal(incompleteTaskContext.partial.value, true);
    assert.equal(incompleteTaskContext.partial.reasons.includes("knowledge_support_incomplete"), true);
  } finally {
    await runtime.dispose();
  }
});

test("real D1 repository context marks absent owner-scoped sources partial and stale", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({ memoryExtractor: "fixture", memoryEnabled: true });
  try {
    await runtime.executeSql(
      "INSERT INTO observation_sources (owner_id, host_id, session_id, repository_key, source_base_revision, source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count, last_synced_at) VALUES ('other-owner', 'foreign-owner-host', 'foreign-owner-session', ?, 0, 1, 1, 1, 0, 0, ?)",
      [MEMORY_TEST_REPOSITORY, STAMP],
    );
    await runtime.executeSql(
      "INSERT INTO observation_sources (owner_id, host_id, session_id, repository_key, source_base_revision, source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count, last_synced_at) VALUES (?, 'other-repository-host', 'other-repository-session', ?, 0, 1, 1, 1, 0, 0, ?)",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY + "/other", STAMP],
    );

    for (const repository of [MEMORY_TEST_REPOSITORY, MEMORY_TEST_REPOSITORY + "-mistyped"]) {
      for (const name of ["context_resolve", "context_status"]) {
        const context = await contextToolValue(runtime, name, { repository }, 31);
        assert.equal(context.scope.repository, repository);
        assert.equal(context.freshness.source_count, 0);
        assert.deepEqual(context.freshness.source_cursors, []);
        assert.equal(context.freshness.latest_cloud_seq, 0);
        assert.equal(context.freshness.cloud_observation_stale, true);
        assert.equal(context.freshness.partial, true);
        if (name === "context_resolve") {
          assert.equal(context.partial.value, true);
          assert.equal(context.partial.reasons.includes("source_incomplete"), true);
        } else {
          assert.equal(context.partial, true);
        }
        assert.doesNotMatch(JSON.stringify(context), /foreign-owner-host|other-repository-host|foreign-owner-session/);
      }
    }
  } finally {
    await runtime.dispose();
  }
});

test("D1 resolver keeps unsupported cross-source policy changes separate until exact predecessor", {
  timeout: 30_000,
}, async () => {
  const hosts = [
    { id: "context-json-host", token: "context-json-host-token", session: "context-json-session", task: "context-json-task" },
    { id: "context-toml-host", token: "context-toml-host-token", session: "context-toml-session", task: "context-toml-task" },
    { id: "context-replace-host", token: "context-replace-host-token", session: "context-replace-session", task: "context-replace-task" },
  ];
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    bindings: {
      OBSERVATION_OWNER_ID: MEMORY_TEST_OWNER,
      HOST_TOKENS_JSON: JSON.stringify(Object.fromEntries(hosts.map(({ id, token }) => [id, token]))),
      CLIENT_TOKEN: MEMORY_TEST_CLIENT_TOKEN,
    },
  });

  async function syncPolicy({ host, content }) {
    const now = Math.floor(Date.now() / 1000);
    const observationId = randomUUID();
    const operationId = randomUUID();
    const digest = createHash("sha256").update(content).digest("hex");
    const observation = {
      id: observationId,
      schema_version: 1,
      observed_at: now,
      session_id: host.session,
      session_instance: { started_at: now, process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: host.task,
      operation_id: operationId,
      content: {
        kind: "text",
        preview: content,
        total_bytes: Buffer.byteLength(content),
        sha256: digest,
        truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "instruction:" + host.session + ":1",
    };
    const response = await runtime.fetch(
      `https://memory-test.local/v1/hosts/${host.id}/observations/sync`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${host.token}`,
          "content-type": "application/json",
          "x-temote-host-id": host.id,
        },
        body: JSON.stringify({
          schema_version: 1,
          session_id: host.session,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200, "authenticated host sync should commit the policy observation");
    const body = await response.json();
    assert.equal(body.complete, true);
    await runtime.waitFor(async () => {
      const checkpoint = await runtime.querySql(
        "SELECT head.active_producer_version, head.requested_producer_version, checkpoint.last_cloud_seq, checkpoint.last_error_code FROM memory_projection_heads AS head JOIN memory_checkpoints AS checkpoint ON checkpoint.owner_id = head.owner_id AND checkpoint.repository_key = head.repository_key AND checkpoint.producer_version = head.active_producer_version WHERE head.owner_id = ? AND head.repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1
        && checkpoint[0].active_producer_version === checkpoint[0].requested_producer_version
        && Number(checkpoint[0].last_cloud_seq) >= Number(body.cloud_head_seq)
        && checkpoint[0].last_error_code == null;
    }, { timeoutMs: 10_000, intervalMs: 25 });
    return observationId;
  }

  async function repositoryContext() {
    return contextValue(runtime, { repository: MEMORY_TEST_REPOSITORY, limit: 16 });
  }

  try {
    const jsonSupportId = await syncPolicy({
      host: hosts[0],
      content: "For this repository, the repository-level policy is: Report output format must be JSON.",
    });
    const initial = await repositoryContext();
    assert.equal(initial.memory.state, "ready");
    assert.equal(initial.current_summary.knowledge_summary, "Report output format must be JSON.");
    assert.equal(initial.constraints.length, 1);
    assert.equal(initial.constraints[0].text, "Report output format must be JSON.");
    assert.equal(initial.constraints[0].status, "current");
    assert.equal(initial.constraints[0].support_refs[0].observation_id, jsonSupportId);

    const tomlSupportId = await syncPolicy({
      host: hosts[1],
      content: "For this repository, the repository-level policy has changed: Report output format must be TOML.",
    });
    const conflicting = await repositoryContext();
    assert.equal(conflicting.memory.state, "ready");
    assert.equal(conflicting.current_summary.knowledge_summary, "Report output format must be JSON.");
    assert.deepEqual(
      conflicting.constraints.map(({ text, status }) => ({ text, status })),
      [
        { text: "Report output format must be JSON.", status: "current" },
        { text: "Report output format must be TOML.", status: "supported" },
      ],
    );
    assert.equal(
      conflicting.constraints.find((item) => item.text.endsWith("TOML.")).support_refs[0].observation_id,
      tomlSupportId,
    );
    assert.equal(conflicting.constraints.find((item) => item.text.endsWith("TOML.")).supersession_history.length, 0);

    const replacementSupportId = await syncPolicy({
      host: hosts[2],
      content: [
        "For this repository, the repository-level policy has changed: Report output format must be TOML.",
        "Previous repository-level policy to replace: Report output format must be JSON.",
      ].join("\n"),
    });
    const replaced = await repositoryContext();
    assert.equal(replaced.memory.state, "ready");
    assert.equal(replaced.current_summary.knowledge_summary, "Report output format must be TOML.");
    assert.deepEqual(
      replaced.constraints.map(({ text, status }) => ({ text, status })),
      [{ text: "Report output format must be TOML.", status: "current" }],
    );
    assert.equal(replaced.constraints[0].support_refs[0].observation_id, tomlSupportId);
    assert.equal(replaced.constraints[0].supersession_history.length, 1);
    assert.equal(replaced.constraints[0].supersession_history[0].text, "Report output format must be JSON.");
    assert.equal(replaced.constraints[0].supersession_history[0].status, "superseded");
    assert.equal(replaced.constraints[0].supersession_history[0].support_refs[0].observation_id, jsonSupportId);
    assert.equal(replaced.constraints[0].supersedes[0].knowledge_id, replaced.constraints[0].supersession_history[0].knowledge_id);
    assert.equal(replaced.constraints[0].support_refs.some((ref) => ref.observation_id === replacementSupportId), true);
  } finally {
    await runtime.dispose();
  }
});
