import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  MEMORY_TEST_REPOSITORY,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const POLICY_JSON = "Report output format must be JSON.";
const POLICY_TOML = "Report output format must be TOML.";

async function syncInstructionBatch(runtime, { hostId, token, sessionId, entries }) {
  const now = Math.floor(Date.now() / 1000);
  const operationId = `operation-${sessionId}`;
  const taskId = `task-${sessionId}`;
  const records = entries.map(({ revision, content }) => ({
    source_revision: revision,
    observation: {
      id: randomUUID(),
      schema_version: 1,
      observed_at: now - 120 + revision,
      session_id: sessionId,
      session_instance: { started_at: now, process_id: 17 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: taskId,
      operation_id: operationId,
      content: {
        kind: "text",
        preview: content,
        total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"),
        truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision,
      dedupe_key: `${sessionId}:${revision}`,
    },
  }));
  const response = await runtime.fetch(
    `https://memory-test.local/v1/hosts/${encodeURIComponent(hostId)}/observations/sync`,
    {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        "x-temote-host-id": hostId,
      },
      body: JSON.stringify({
        schema_version: 1,
        session_id: sessionId,
        repository_key: MEMORY_TEST_REPOSITORY,
        source_base_revision: 0,
        source_head_revision: Math.max(...entries.map((entry) => entry.revision)),
        journal_degraded: false,
        gap_count: 0,
        records,
      }),
    },
  );
  assert.equal(response.status, 200, "instruction batch should be accepted by the host sync boundary");
  return response.json();
}

async function waitForMemoryHead(runtime, cloudHeadSeq) {
  await runtime.waitFor(async () => {
    const checkpoints = await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    return checkpoints.length > 0
      && Number(checkpoints[0].last_cloud_seq) >= Number(cloudHeadSeq);
  }, { timeoutMs: 10_000, intervalMs: 25 });
}

function repositoryPolicy(quote, { changed = false, predecessor = null } = {}) {
  return [
    `For this repository, the repository-level policy ${changed ? "has changed" : "is"}:`,
    quote,
    ...(predecessor ? [`Previous repository-level policy to replace: ${predecessor}`] : []),
  ].join("\n");
}

function abaEntries() {
  return [
    { revision: 1, content: repositoryPolicy(POLICY_JSON) },
    { revision: 2, content: repositoryPolicy(POLICY_TOML, { changed: true, predecessor: POLICY_JSON }) },
    { revision: 3, content: repositoryPolicy(POLICY_JSON, { changed: true, predecessor: POLICY_TOML }) },
  ];
}

async function repositoryContext(runtime) {
  const response = await runtime.callMcp(MEMORY_TEST_CLIENT_TOKEN, "context_resolve", {
    repository: MEMORY_TEST_REPOSITORY,
    limit: 16,
  });
  assert.equal(response.status, 200);
  assert.equal(response.body.error, undefined);
  return JSON.parse(response.body.result.content[0].text);
}

async function knowledgeState(runtime, producerVersion = null) {
  const params = [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY];
  let filter = "";
  if (producerVersion != null) {
    filter = " AND producer_version = ?";
    params.push(producerVersion);
  }
  const constraints = await runtime.querySql(
    `SELECT knowledge_id, text, status, support_incomplete FROM knowledge_items
     WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'${filter}`,
    params,
  );
  const summaries = await runtime.querySql(
    `SELECT knowledge_id, text, status FROM knowledge_items
     WHERE owner_id = ? AND repository_key = ? AND kind = 'summary'${filter}`,
    params,
  );
  const edgeFilter = producerVersion == null ? "" : " AND new_item.producer_version = ?";
  const edges = await runtime.querySql(
    `SELECT edge.new_knowledge_id, edge.old_knowledge_id,
            new_item.text AS new_text, old_item.text AS old_text,
            new_item.kind AS new_kind, old_item.kind AS old_kind
     FROM knowledge_supersession AS edge
     JOIN knowledge_items AS new_item ON new_item.knowledge_id = edge.new_knowledge_id
     JOIN knowledge_items AS old_item ON old_item.knowledge_id = edge.old_knowledge_id
     WHERE edge.owner_id = ? AND edge.repository_key = ?${edgeFilter}`,
    params,
  );
  return { constraints, summaries, edges };
}

async function assertReinstatedPolicyState(runtime, producerVersion = null) {
  const { constraints, summaries, edges } = await knowledgeState(runtime, producerVersion);

  const jsonRows = constraints.filter((item) => item.text === POLICY_JSON);
  const tomlRows = constraints.filter((item) => item.text === POLICY_TOML);
  assert.equal(jsonRows.filter((item) => item.status === "current").length, 1,
    "the reinstated JSON policy must be the single current constraint");
  assert.ok(jsonRows.some((item) => item.status === "superseded"),
    "the original JSON policy must remain in history as superseded");
  assert.equal(tomlRows.length, 1);
  assert.equal(tomlRows[0].status, "superseded",
    "the intermediate TOML policy must never remain current");
  assert.equal(constraints.length, 3,
    "three distinct policy revisions remain in history");

  const supersededJsonId = jsonRows.find((item) => item.status === "superseded").knowledge_id;
  const currentJsonId = jsonRows.find((item) => item.status === "current").knowledge_id;
  const tomlId = tomlRows[0].knowledge_id;
  assert.ok(edges.some((edge) => edge.new_knowledge_id === tomlId
    && edge.old_knowledge_id === supersededJsonId),
    "history records JSON -> TOML as a supersession");
  assert.ok(edges.some((edge) => edge.new_knowledge_id === currentJsonId
    && edge.old_knowledge_id === tomlId),
    "history records TOML -> reinstated JSON as a supersession, not a silent fold");
  assert.equal(edges.every((edge) => edge.new_knowledge_id !== edge.old_knowledge_id), true,
    "supersession must never be self-referential");

  const supportedSummary = summaries.filter((item) => item.status === "supported");
  assert.equal(supportedSummary.length, 1);
  assert.equal(supportedSummary[0].text, POLICY_JSON,
    "the surviving summary must describe the last policy, not an earlier one");

  const context = await repositoryContext(runtime);
  assert.ok(context.constraints.some((item) => item.text === POLICY_JSON && item.status === "current"),
    "context_resolve must surface the reinstated JSON policy as current");
  assert.equal(context.constraints.some((item) => item.text === POLICY_TOML
    && item.status === "current"), false,
    "context_resolve must never surface the intermediate TOML policy as current");
  assert.equal(context.current_summary.knowledge_summary, POLICY_JSON);
}

test("same-batch A->B->A reinstatement ends with the last policy current", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const synced = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-single-batch-session",
      entries: abaEntries(),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    await assertReinstatedPolicyState(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("split batches A -> B -> A and A -> [B,A] converge on the same final policy", {
  timeout: 45_000,
}, async () => {
  const splitThree = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  const splitTail = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const [first, second, third] = abaEntries();
    for (const entry of [first, second, third]) {
      const synced = await syncInstructionBatch(splitThree, {
        hostId: MEMORY_TEST_HOST_ID,
        token: MEMORY_TEST_HOST_TOKEN,
        sessionId: "aba-split-three-session",
        entries: [entry],
      });
      await waitForMemoryHead(splitThree, synced.cloud_head_seq);
    }

    const firstSync = await syncInstructionBatch(splitTail, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-split-tail-session",
      entries: [first],
    });
    await waitForMemoryHead(splitTail, firstSync.cloud_head_seq);
    const secondSync = await syncInstructionBatch(splitTail, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-split-tail-session",
      entries: [second, third],
    });
    await waitForMemoryHead(splitTail, secondSync.cloud_head_seq);

    await assertReinstatedPolicyState(splitThree);
    await assertReinstatedPolicyState(splitTail);
  } finally {
    await splitThree.dispose();
    await splitTail.dispose();
  }
});

test("split batch [A,B] -> A also reinstates the JSON policy", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const [first, second, third] = abaEntries();
    const firstSync = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "ab-then-a-session",
      entries: [first, second],
    });
    await waitForMemoryHead(runtime, firstSync.cloud_head_seq);
    const secondSync = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "ab-then-a-session",
      entries: [third],
    });
    await waitForMemoryHead(runtime, secondSync.cloud_head_seq);
    await assertReinstatedPolicyState(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("provider-path canonical union keeps the reinstatement as an independent change", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "echo-test-provider",
    memoryApiKey: "test-only-provider-key",
    memoryTestProviderMode: "echo_allowed_repository_clauses",
    maxQueueRetries: 0,
  });
  try {
    const synced = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-provider-session",
      entries: abaEntries(),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    await assertReinstatedPolicyState(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("plain A->A remains ordinary duplicate support", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const synced = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aa-duplicate-session",
      entries: [1, 2].map((revision) => ({
        revision,
        content: repositoryPolicy(POLICY_JSON),
      })),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    const { constraints, edges } = await knowledgeState(runtime);
    assert.equal(constraints.length, 1);
    assert.equal(constraints[0].text, POLICY_JSON);
    assert.equal(constraints[0].status, "current");
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, constraints[0].knowledge_id],
    ))[0].count, 2, "duplicate policy mentions fold into support, not new items");
    assert.equal(edges.length, 0, "no supersession without an intervening change");
  } finally {
    await runtime.dispose();
  }
});

test("queue redelivery never duplicates items, support, or supersession", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const synced = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-replay-session",
      entries: abaEntries(),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    const before = await knowledgeState(runtime);
    const countRows = async () => (await runtime.querySql(
      `SELECT (SELECT COUNT(*) FROM knowledge_items WHERE owner_id = ? AND repository_key = ?) AS items,
              (SELECT COUNT(*) FROM knowledge_support WHERE owner_id = ? AND repository_key = ?) AS supports,
              (SELECT COUNT(*) FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?) AS edges`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY,
        MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY,
        MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    const countsBefore = await countRows();

    const queueBefore = await runtime.queueDispatchCount();
    await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: Number(synced.cloud_head_seq),
    });
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > queueBefore && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });
    await runtime.runScheduled();

    const countsAfter = await countRows();
    assert.deepEqual(countsAfter, countsBefore,
      "re-delivered work must be a no-op for items, supports, and supersession edges");
    const after = await knowledgeState(runtime);
    assert.deepEqual(
      after.constraints.map(({ text, status }) => ({ text, status })).sort(),
      before.constraints.map(({ text, status }) => ({ text, status })).sort(),
    );
  } finally {
    await runtime.dispose();
  }
});

test("rebuild from retained observations converges on the same reinstated policy", {
  timeout: 45_000,
}, async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-reinstate-"));
  let initial;
  let rebuilt;
  try {
    initial = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      memoryExtractor: "fixture",
      memoryEnabled: true,
      maxQueueRetries: 0,
    });
    const synced = await syncInstructionBatch(initial, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "aba-rebuild-session",
      entries: abaEntries(),
    });
    await waitForMemoryHead(initial, synced.cloud_head_seq);
    const initialHead = (await initial.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(initialHead.active_generation, 1);
    await initial.dispose();
    initial = null;

    rebuilt = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryExtractor: "fixture",
      memoryEnabled: true,
      maxQueueRetries: 0,
      bindings: { MEMORY_PROJECTION_GENERATION: "2" },
    });
    await rebuilt.executeSql(
      "UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0 WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    await rebuilt.runScheduled();
    await rebuilt.waitFor(async () => {
      const head = await rebuilt.querySql(
        "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return head.length === 1 && head[0].active_generation === 2;
    }, { timeoutMs: 15_000, intervalMs: 25 });

    const finalHead = (await rebuilt.querySql(
      "SELECT active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.notEqual(finalHead.active_producer_version, initialHead.active_producer_version);
    await assertReinstatedPolicyState(rebuilt, finalHead.active_producer_version);
  } finally {
    if (initial) await initial.dispose();
    if (rebuilt) await rebuilt.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});
