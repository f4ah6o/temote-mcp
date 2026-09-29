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
const POLICY_YAML = "Report output format must be YAML.";

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

function unconfirmedThenChangeEntries() {
  return [
    { revision: 1, content: repositoryPolicy(POLICY_JSON) },
    { revision: 2, content: repositoryPolicy(POLICY_TOML) },
    { revision: 3, content: repositoryPolicy(POLICY_TOML, { changed: true, predecessor: POLICY_JSON }) },
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

async function assertPromotedChangeState(runtime, producerVersion = null) {
  const { constraints, summaries, edges } = await knowledgeState(runtime, producerVersion);

  const jsonRows = constraints.filter((item) => item.text === POLICY_JSON);
  const tomlRows = constraints.filter((item) => item.text === POLICY_TOML);
  assert.equal(jsonRows.length, 1);
  assert.equal(jsonRows[0].status, "superseded",
    "the explicitly replaced JSON policy must be superseded");
  assert.equal(tomlRows.length, 1,
    "the unconfirmed and changed TOML mentions merge into one item");
  assert.equal(tomlRows[0].status, "current",
    "the explicitly changed TOML policy must be promoted to current");

  const tomlSupportSeqs = (await runtime.querySql(
    `SELECT observation_cloud_seq FROM knowledge_support
     WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?
     ORDER BY observation_cloud_seq`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, tomlRows[0].knowledge_id],
  )).map((row) => Number(row.observation_cloud_seq));
  assert.deepEqual(tomlSupportSeqs, [2, 3],
    "the promoted item keeps both the unconfirmed and changed supports");

  const promotionEdges = edges.filter((edge) => edge.new_knowledge_id === tomlRows[0].knowledge_id);
  assert.equal(promotionEdges.length, 1,
    "the promoted policy supersedes the replaced row exactly once");
  assert.equal(promotionEdges[0].old_knowledge_id, jsonRows[0].knowledge_id);
  assert.equal(edges.every((edge) => edge.new_knowledge_id !== edge.old_knowledge_id), true,
    "supersession must never be self-referential");

  const supportedSummary = summaries.filter((item) => item.status === "supported");
  assert.equal(supportedSummary.length, 1);
  assert.equal(supportedSummary[0].text, POLICY_TOML,
    "the surviving summary must describe the promoted policy");

  const context = await repositoryContext(runtime);
  assert.ok(context.constraints.some((item) => item.text === POLICY_TOML && item.status === "current"),
    "context_resolve must surface the promoted TOML policy as current");
  assert.equal(context.constraints.some((item) => item.text === POLICY_JSON
    && item.status === "current"), false,
    "context_resolve must never surface the replaced JSON policy as current");
  assert.equal(context.current_summary.knowledge_summary, POLICY_TOML);
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

test("same-batch unconfirmed then changed text promotes the explicit change", {
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
      sessionId: "unconfirmed-change-session",
      entries: unconfirmedThenChangeEntries(),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    await assertPromotedChangeState(runtime);

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
    assert.deepEqual(await countRows(), countsBefore,
      "re-delivered work must not duplicate items, supports, or supersession edges");
    await assertPromotedChangeState(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("split batches of unconfirmed-then-changed converge on the promoted policy", {
  timeout: 60_000,
}, async () => {
  const splitTail = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  const splitHead = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  const splitEach = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const [first, second, third] = unconfirmedThenChangeEntries();

    const tailFirst = await syncInstructionBatch(splitTail, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "unchanged-split-tail-session",
      entries: [first],
    });
    await waitForMemoryHead(splitTail, tailFirst.cloud_head_seq);
    const tailSecond = await syncInstructionBatch(splitTail, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "unchanged-split-tail-session",
      entries: [second, third],
    });
    await waitForMemoryHead(splitTail, tailSecond.cloud_head_seq);

    const headFirst = await syncInstructionBatch(splitHead, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "unchanged-split-head-session",
      entries: [first, second],
    });
    await waitForMemoryHead(splitHead, headFirst.cloud_head_seq);
    const headSecond = await syncInstructionBatch(splitHead, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "unchanged-split-head-session",
      entries: [third],
    });
    await waitForMemoryHead(splitHead, headSecond.cloud_head_seq);

    for (const entry of [first, second, third]) {
      const synced = await syncInstructionBatch(splitEach, {
        hostId: MEMORY_TEST_HOST_ID,
        token: MEMORY_TEST_HOST_TOKEN,
        sessionId: "unchanged-split-each-session",
        entries: [entry],
      });
      await waitForMemoryHead(splitEach, synced.cloud_head_seq);
    }

    await assertPromotedChangeState(splitTail);
    await assertPromotedChangeState(splitHead);
    await assertPromotedChangeState(splitEach);
  } finally {
    await splitTail.dispose();
    await splitHead.dispose();
    await splitEach.dispose();
  }
});

test("provider-path canonical union promotes the explicit same-text change", {
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
      sessionId: "unchanged-provider-session",
      entries: unconfirmedThenChangeEntries(),
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    await assertPromotedChangeState(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("unconfirmed text alone never promotes without a change directive", {
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
      sessionId: "unconfirmed-only-session",
      entries: [
        { revision: 1, content: repositoryPolicy(POLICY_JSON) },
        { revision: 2, content: repositoryPolicy(POLICY_TOML) },
      ],
    });
    await waitForMemoryHead(runtime, synced.cloud_head_seq);
    const { constraints, edges } = await knowledgeState(runtime);
    const jsonRow = constraints.find((item) => item.text === POLICY_JSON);
    const tomlRow = constraints.find((item) => item.text === POLICY_TOML);
    assert.equal(jsonRow.status, "current");
    assert.equal(tomlRow.status, "supported",
      "an unconfirmed competing policy statement stays supported");
    assert.equal(edges.length, 0,
      "no supersession without an explicit change directive");
  } finally {
    await runtime.dispose();
  }
});

test("an unauthorized cross-source change claim stays blocked on the same-text path", {
  timeout: 30_000,
}, async () => {
  const hosts = { "policy-host-a": "policy-token-a", "policy-host-b": "policy-token-b" };
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
  });
  try {
    const first = await syncInstructionBatch(runtime, {
      hostId: "policy-host-a",
      token: hosts["policy-host-a"],
      sessionId: "policy-session-a",
      entries: [{ revision: 1, content: repositoryPolicy(POLICY_JSON) }],
    });
    await waitForMemoryHead(runtime, first.cloud_head_seq);
    const second = await syncInstructionBatch(runtime, {
      hostId: "policy-host-b",
      token: hosts["policy-host-b"],
      sessionId: "policy-session-b",
      entries: [
        { revision: 1, content: repositoryPolicy(POLICY_TOML) },
        { revision: 2, content: repositoryPolicy(POLICY_TOML, { changed: true }) },
      ],
    });
    await waitForMemoryHead(runtime, second.cloud_head_seq);
    const { constraints, edges } = await knowledgeState(runtime);
    const jsonRow = constraints.find((item) => item.text === POLICY_JSON);
    const tomlRow = constraints.find((item) => item.text === POLICY_TOML);
    assert.equal(jsonRow.status, "current");
    assert.equal(tomlRow.status, "supported",
      "a cross-source change claim without ordering or predecessor stays unconfirmed");
    assert.equal(edges.length, 0,
      "an unauthorized claim must not create supersession edges");
  } finally {
    await runtime.dispose();
  }
});

test("same-batch restatement then change supersedes a reused committed row once", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const sessionId = "restate-change-session";
    const firstSync = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId,
      entries: [{ revision: 1, content: repositoryPolicy(POLICY_JSON) }],
    });
    await waitForMemoryHead(runtime, firstSync.cloud_head_seq);

    const secondSync = await syncInstructionBatch(runtime, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId,
      entries: [
        { revision: 2, content: repositoryPolicy(POLICY_JSON) },
        { revision: 3, content: repositoryPolicy(POLICY_TOML, { changed: true, predecessor: POLICY_JSON }) },
      ],
    });
    await waitForMemoryHead(runtime, secondSync.cloud_head_seq);

    const failedRuns = await runtime.querySql(
      "SELECT status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? AND status = 'failed'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.deepEqual(failedRuns, [], "the restate-plus-change batch must not wedge the run");

    const { constraints, edges } = await knowledgeState(runtime);
    const jsonRow = constraints.find((item) => item.text === POLICY_JSON);
    const tomlRow = constraints.find((item) => item.text === POLICY_TOML);
    assert.equal(jsonRow.status, "superseded");
    assert.equal(tomlRow.status, "current");
    assert.equal(constraints.length, 2, "the restatement reuses the committed row instead of duplicating it");
    const supersessionOfJson = edges.filter((edge) => edge.old_knowledge_id === jsonRow.knowledge_id);
    assert.equal(supersessionOfJson.length, 1,
      "a row reused inside the batch still yields exactly one supersession edge");
    assert.equal(supersessionOfJson[0].new_knowledge_id, tomlRow.knowledge_id);

    const context = await repositoryContext(runtime);
    assert.ok(context.constraints.some((item) => item.text === POLICY_TOML && item.status === "current"),
      "context_resolve must surface the changed TOML policy as current");
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

function contestedEntries() {
  return {
    before: { revision: 1, content: repositoryPolicy(POLICY_TOML, { changed: true }) },
    contested: { revision: 1, content: repositoryPolicy(POLICY_YAML) },
    directive: {
      revision: 2,
      content: repositoryPolicy(POLICY_TOML, { changed: true }),
    },
  };
}

async function assertContestedChangeState(runtime) {
  const { constraints, summaries, edges } = await knowledgeState(runtime);

  const tomlRows = constraints.filter((item) => item.text === POLICY_TOML);
  const yamlRows = constraints.filter((item) => item.text === POLICY_YAML);
  assert.equal(tomlRows.length, 1, "the three TOML mentions fold into one item");
  assert.equal(tomlRows[0].status, "current",
    "an unauthorized contested directive must not demote an earned current");
  assert.equal(yamlRows.length, 1);
  assert.equal(yamlRows[0].status, "supported",
    "the unconfirmed cross-session claim stays supported");

  const supportSeqs = (await runtime.querySql(
    `SELECT observation_cloud_seq FROM knowledge_support
     WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?
     ORDER BY observation_cloud_seq`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, tomlRows[0].knowledge_id],
  )).map((row) => Number(row.observation_cloud_seq));
  assert.deepEqual(supportSeqs, [1, 3],
    "the contested item keeps the original and directive supports");

  const tomlItem = (await runtime.querySql(
    `SELECT valid_from FROM knowledge_items
     WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, tomlRows[0].knowledge_id],
  ))[0];
  const firstSupport = (await runtime.querySql(
    `SELECT observed_at FROM observations WHERE cloud_seq = ?`,
    [supportSeqs[0]],
  ))[0];
  assert.equal(tomlItem.valid_from, firstSupport.observed_at,
    "valid_from tracks the earliest merged support, matching committed rows");

  assert.equal(edges.length, 0,
    "no supersession edge exists when the directive was never authorized");

  const tomlSummary = summaries.filter((item) => item.text === POLICY_TOML);
  assert.equal(tomlSummary.length, 1);
  assert.equal(tomlSummary[0].status, "supported",
    "the surviving summary describes the contested current policy");
  assert.equal(summaries.every((item) => item.status !== "superseded"), true,
    "no summary is superseded when the directive was never authorized");

  const context = await repositoryContext(runtime);
  assert.ok(context.constraints.some((item) => item.text === POLICY_TOML
    && item.status === "current"),
    "context_resolve keeps surfacing the contested-but-earned current policy");
}

test("a contested neighbor cannot demote a staged current or skew valid_from", {
  timeout: 60_000,
}, async () => {
  const sameBatch = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  const splitBatch = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const { before, contested, directive } = contestedEntries();

    // Same batch: the staged current, the contested neighbor, and the
    // unauthorized directive are all evaluated inside one extraction run.
    await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-same-a",
      entries: [before],
    });
    await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-same-b",
      entries: [contested],
    });
    const sameTail = await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-same-a",
      entries: [directive],
    });
    await waitForMemoryHead(sameBatch, sameTail.cloud_head_seq);

    // Split [1] -> [2,3]: the current row is committed before the contested
    // neighbor and the unauthorized directive arrive.
    const splitHead = await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-split-a",
      entries: [before],
    });
    await waitForMemoryHead(splitBatch, splitHead.cloud_head_seq);
    await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-split-b",
      entries: [contested],
    });
    const splitTail = await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "contested-split-a",
      entries: [directive],
    });
    await waitForMemoryHead(splitBatch, splitTail.cloud_head_seq);

    await assertContestedChangeState(sameBatch);
    await assertContestedChangeState(splitBatch);
  } finally {
    await sameBatch.dispose();
    await splitBatch.dispose();
  }
});

test("a staged promotion shadows the stale committed status for sticky current", {
  timeout: 90_000,
}, async () => {
  // [1,2]: YAML earns current, the unconfirmed TOML restatement stays
  // supported and commits. [3,4,5] then promotes TOML over YAML
  // legitimately (same-session explicit predecessor), stages an
  // unauthorized cross-session INI restatement, and finally a
  // cross-session TOML directive that cannot prove its change. The stale
  // committed TOML row still reads `supported` in the batch snapshot, so
  // the fresh staged `current` must win the sticky check.
  const POLICY_INI = "Report output format must be INI.";
  const head = [
    { revision: 1, content: repositoryPolicy(POLICY_YAML) },
    { revision: 2, content: repositoryPolicy(POLICY_TOML) },
  ];
  const promote = [
    { revision: 3, content: repositoryPolicy(POLICY_TOML, { changed: true, predecessor: POLICY_YAML }) },
  ];
  const contested = [
    { revision: 1, content: repositoryPolicy(POLICY_INI) },
  ];
  const lateDirective = [
    { revision: 1, content: repositoryPolicy(POLICY_TOML, { changed: true }) },
  ];

  async function assertPromotedContestedState(runtime) {
    const { constraints, summaries, edges } = await knowledgeState(runtime);
    const tomlRows = constraints.filter((item) => item.text === POLICY_TOML);
    const yamlRows = constraints.filter((item) => item.text === POLICY_YAML);
    const iniRows = constraints.filter((item) => item.text === POLICY_INI);
    assert.equal(tomlRows.length, 1);
    assert.equal(tomlRows[0].status, "current",
      "the legitimately promoted staged current must survive the later unauthorized directive");
    assert.equal(yamlRows.length, 1);
    assert.equal(yamlRows[0].status, "superseded",
      "the authorized predecessor stays superseded");
    assert.equal(iniRows.length, 1);
    assert.equal(iniRows[0].status, "supported",
      "the unauthorized cross-session claim stays supported");

    const supportSeqs = (await runtime.querySql(
      `SELECT observation_cloud_seq FROM knowledge_support
       WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?
       ORDER BY observation_cloud_seq`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, tomlRows[0].knowledge_id],
    )).map((row) => Number(row.observation_cloud_seq));
    assert.deepEqual(supportSeqs, [2, 3, 5],
      "the promoted item keeps restatement, promotion, and late-directive supports");

    const tomlItem = (await runtime.querySql(
      `SELECT valid_from FROM knowledge_items
       WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, tomlRows[0].knowledge_id],
    ))[0];
    const firstSupport = (await runtime.querySql(
      `SELECT observed_at FROM observations WHERE cloud_seq = ?`, [supportSeqs[0]],
    ))[0];
    assert.equal(tomlItem.valid_from, firstSupport.observed_at,
      "valid_from tracks the earliest merged support, not the late directive");

    const promotionEdges = edges.filter((edge) => edge.new_knowledge_id === tomlRows[0].knowledge_id);
    assert.equal(promotionEdges.length, 1);
    assert.equal(promotionEdges[0].old_knowledge_id, yamlRows[0].knowledge_id);
    assert.equal(edges.every((edge) => edge.new_knowledge_id !== edge.old_knowledge_id), true);

    const tomlSummary = summaries.filter((item) => item.text === POLICY_TOML);
    assert.equal(tomlSummary.length, 1);
    assert.equal(tomlSummary[0].status, "supported",
      "the surviving summary describes the promoted current policy");

    const context = await repositoryContext(runtime);
    assert.ok(context.constraints.some((item) => item.text === POLICY_TOML
      && item.status === "current"),
      "context_resolve keeps surfacing the promoted policy as current");
    assert.equal(context.current_summary.knowledge_summary, POLICY_TOML);
  }

  const sameBatch = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  const splitBatch = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    // Bug order: [1,2] commits supported TOML, then [3,4,5] evaluates the
    // authorized promotion and the unauthorized late directive together.
    const sameHead = await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-same-a",
      entries: head,
    });
    await waitForMemoryHead(sameBatch, sameHead.cloud_head_seq);
    await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-same-a",
      entries: promote,
    });
    await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-same-b",
      entries: contested,
    });
    const sameTail = await syncInstructionBatch(sameBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-same-c",
      entries: lateDirective,
    });
    await waitForMemoryHead(sameBatch, sameTail.cloud_head_seq);

    // Split order: [1,2] -> [3,4] -> [5]; the promoted row commits before
    // the unauthorized directive arrives.
    const splitHead = await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-split-a",
      entries: head,
    });
    await waitForMemoryHead(splitBatch, splitHead.cloud_head_seq);
    await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-split-a",
      entries: promote,
    });
    const splitMid = await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-split-b",
      entries: contested,
    });
    await waitForMemoryHead(splitBatch, splitMid.cloud_head_seq);
    const splitTail = await syncInstructionBatch(splitBatch, {
      hostId: MEMORY_TEST_HOST_ID,
      token: MEMORY_TEST_HOST_TOKEN,
      sessionId: "shadow-split-c",
      entries: lateDirective,
    });
    await waitForMemoryHead(splitBatch, splitTail.cloud_head_seq);

    await assertPromotedContestedState(sameBatch);
    await assertPromotedContestedState(splitBatch);
  } finally {
    await sameBatch.dispose();
    await splitBatch.dispose();
  }
});
