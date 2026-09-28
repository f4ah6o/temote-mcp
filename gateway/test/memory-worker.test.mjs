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
import { semanticKeyFor } from "../src/memory/safety.js";

async function syncInstructionBatch(runtime, { hostId, token, sessionId, entries }) {
  const now = Math.floor(Date.now() / 1000);
  const operationId = `operation-${sessionId}`;
  const taskId = `task-${sessionId}`;
  const records = entries.map(({ revision, content }) => ({
    source_revision: revision,
    observation: {
      id: randomUUID(),
      schema_version: 1,
      observed_at: now + revision,
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

test("failed generation keeps the previous active projection and leaves raw observations unchanged", {
  timeout: 30_000,
}, async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-staging-"));
  let baseline;
  let failedBuild;
  try {
    baseline = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      memoryExtractor: "fixture",
      memoryEnabled: true,
      maxQueueRetries: 0,
    });
    const content = [
      "For this repository, the repository-level policy is:",
      "Report output format must be JSON.",
    ].join("\n");
    const record = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "projection-staging-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "projection-staging-task",
      operation_id: randomUUID(),
      content: {
        kind: "text",
        preview: content,
        total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"),
        truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "projection-staging:instruction:1",
    };
    const response = await baseline.fetch(
      `https://memory-test.local/v1/hosts/${MEMORY_TEST_HOST_ID}/observations/sync`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${MEMORY_TEST_HOST_TOKEN}`,
          "content-type": "application/json",
          "x-temote-host-id": MEMORY_TEST_HOST_ID,
        },
        body: JSON.stringify({
          schema_version: 1,
          session_id: record.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation: record }],
        }),
      },
    );
    assert.equal(response.status, 200);
    const synced = await response.json();
    await baseline.waitFor(async () => {
      const checkpoint = await baseline.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === Number(synced.cloud_head_seq);
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const before = await baseline.querySql(
      "SELECT cloud_seq, observation_id, source_revision, repository_key, content_preview, content_digest FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    const activeBefore = (await baseline.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(activeBefore.active_generation, 1);
    await baseline.dispose();
    baseline = null;

    failedBuild = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryExtractor: "openai_compatible",
      memoryEnabled: true,
      memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
      memoryModel: "unreachable-test-provider",
      memoryApiKey: "test-only-invalid-provider-key",
      memoryTestProviderMode: "reject",
      maxQueueRetries: 0,
      bindings: {
        MEMORY_PROJECTION_GENERATION: "2",
        MEMORY_TIMEOUT_MS: "500",
      },
    });
    await failedBuild.executeSql(
      "UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0 WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    await failedBuild.runScheduled();
    await failedBuild.waitFor(async () => {
      const runs = await failedBuild.querySql(
        "SELECT status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY started_at DESC",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.some((run) => run.status === "failed"
        && ["provider_unavailable", "provider_timeout"].includes(run.error_code));
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const failedRun = (await failedBuild.querySql(
      "SELECT status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? AND status = 'failed'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.ok(["provider_unavailable", "provider_timeout"].includes(failedRun.error_code));

    const head = (await failedBuild.querySql(
      "SELECT active_generation, active_producer_version, requested_generation, requested_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    const activeKnowledge = await failedBuild.querySql(
      "SELECT text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, activeBefore.active_producer_version],
    );
    const after = await failedBuild.querySql(
      "SELECT cloud_seq, observation_id, source_revision, repository_key, content_preview, content_digest FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(head.active_generation, 1, "a failed staged generation must not replace the active projection");
    assert.equal(head.active_producer_version, activeBefore.active_producer_version);
    assert.equal(head.requested_generation, 2);
    assert.ok(activeKnowledge.some((item) => item.status === "current"
      && item.text === "Report output format must be JSON."));
    assert.deepEqual(after, before, "rebuild attempts must never mutate source observations");

    const mcp = await failedBuild.callMcp(MEMORY_TEST_CLIENT_TOKEN, "context_resolve", {
      repository: MEMORY_TEST_REPOSITORY,
      limit: 4,
    }, 18);
    assert.equal(mcp.status, 200);
    const text = mcp.body.result.content.find((item) => item.type === "text").text;
    assert.match(text, /Report output format must be JSON\./,
      "resolver must keep serving the previous active projection during a failed rebuild");
  } finally {
    if (baseline) await baseline.dispose();
    if (failedBuild) await failedBuild.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});

test("one projection transaction applies all seven explicitly named policy changes", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const sessionId = "seven-policy-change-session";
    const oldQuotes = Array.from({ length: 7 }, (_, index) =>
      `Report item ${index + 1} format must be JSON.`);
    const newQuotes = oldQuotes.map((quote, index) =>
      quote.replace("JSON", "TOML"));
    const firstContent = oldQuotes.flatMap((quote) => [
      "For this repository, the repository-level policy is:",
      quote,
    ]).join("\n");
    const changedContent = oldQuotes.flatMap((quote, index) => [
      "For this repository, the repository-level policy has changed:",
      newQuotes[index],
      "Previous repository-level policy to replace: " + quote,
    ]).join("\n");

    const postRevision = async (revision, content) => {
      const now = Math.floor(Date.now() / 1000);
      const record = {
        id: randomUUID(),
        schema_version: 1,
        observed_at: now,
        session_id: sessionId,
        session_instance: { started_at: now, process_id: 7 },
        actor: { transport: "mcp-stdio" },
        target: { backend: "codex" },
        action: "task_start",
        kind: "instruction",
        task_id: "seven-policy-change-task",
        operation_id: "seven-policy-change-operation",
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
        dedupe_key: `seven-policy-change:${revision}`,
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
          body: JSON.stringify({
            schema_version: 1,
            session_id: sessionId,
            repository_key: MEMORY_TEST_REPOSITORY,
            source_base_revision: 0,
            source_head_revision: revision,
            journal_degraded: false,
            gap_count: 0,
            records: [{ source_revision: revision, observation: record }],
          }),
        },
      );
      assert.equal(response.status, 200);
      return response.json();
    };

    const firstSync = await postRevision(1, firstContent);
    await runtime.waitFor(async () => {
      const checkpoint = await runtime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === Number(firstSync.cloud_head_seq);
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const secondSync = await postRevision(2, changedContent);
    await runtime.waitFor(async () => {
      const checkpoint = await runtime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === Number(secondSync.cloud_head_seq);
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const constraints = await runtime.querySql(
      "SELECT text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' ORDER BY text",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    const supersessions = await runtime.querySql(
      "SELECT old_item.text AS old_text, new_item.text AS new_text FROM knowledge_supersession AS edge JOIN knowledge_items AS old_item ON old_item.knowledge_id = edge.old_knowledge_id JOIN knowledge_items AS new_item ON new_item.knowledge_id = edge.new_knowledge_id WHERE edge.owner_id = ? AND edge.repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(constraints.length, 14, "all old and new policy revisions remain in history");
    for (let index = 0; index < 7; index += 1) {
      assert.ok(constraints.some((item) => item.text === oldQuotes[index] && item.status === "superseded"));
      assert.ok(constraints.some((item) => item.text === newQuotes[index] && item.status === "current"));
      assert.ok(supersessions.some((edge) => edge.old_text === oldQuotes[index]
        && edge.new_text === newQuotes[index]));
    }
    assert.equal(supersessions.length, 14,
      "seven constraints and their seven exact-source summaries must commit together");
    const runs = await runtime.querySql(
      "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY to_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.deepEqual(runs.map((run) => run.status), ["completed", "completed"]);
    assert.equal(runs.some((run) => run.error_code != null), false);
  } finally {
    await runtime.dispose();
  }
});

test("truncated support cannot authorize delayed same-source supersession, but exact predecessor can", {
  timeout: 45_000,
}, async () => {
  const hosts = {
    "support-source-a": "support-token-a",
    "support-source-b": "support-token-b",
  };
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
  });
  try {
    const oldPolicy = "Report output format must be JSON.";
    const sourceA = { hostId: "support-source-a", token: hosts["support-source-a"], sessionId: "support-session-a" };
    const sourceB = { hostId: "support-source-b", token: hosts["support-source-b"], sessionId: "support-session-b" };
    const first = await syncInstructionBatch(runtime, {
      ...sourceA,
      entries: Array.from({ length: 12 }, (_, index) => ({
        revision: index + 1,
        content: repositoryPolicy(oldPolicy),
      })),
    });
    await waitForMemoryHead(runtime, first.cloud_head_seq);

    const second = await syncInstructionBatch(runtime, {
      ...sourceA,
      entries: Array.from({ length: 4 }, (_, index) => ({
        revision: index + 13,
        content: repositoryPolicy(oldPolicy),
      })),
    });
    await waitForMemoryHead(runtime, second.cloud_head_seq);

    let knowledge = await runtime.querySql(
      "SELECT knowledge_id, text, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.deepEqual(knowledge.map(({ text, status, support_incomplete }) => ({ text, status, support_incomplete })), [
      { text: oldPolicy, status: "current", support_incomplete: 0 },
    ]);
    const oldId = knowledge[0].knowledge_id;
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, oldId],
    ))[0].count, 16);

    const reaffirmed = await syncInstructionBatch(runtime, {
      ...sourceB,
      entries: [1, 2].map((revision) => ({
        revision,
        content: repositoryPolicy(oldPolicy),
      })),
    });
    await waitForMemoryHead(runtime, reaffirmed.cloud_head_seq);
    knowledge = await runtime.querySql(
      "SELECT knowledge_id, text, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(knowledge.length, 1);
    assert.equal(knowledge[0].knowledge_id, oldId);
    assert.equal(knowledge[0].support_incomplete, 1,
      "new independent raw support omitted at the 16-reference limit must be recorded as incomplete");
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, oldId],
    ))[0].count, 16, "support rows stay bounded even after additional independent support arrives");

    const unbound = await syncInstructionBatch(runtime, {
      ...sourceA,
      entries: [{
        revision: 17,
        content: repositoryPolicy("Report output format must be TOML.", { changed: true }),
      }],
    });
    await waitForMemoryHead(runtime, unbound.cloud_head_seq);
    knowledge = await runtime.querySql(
      "SELECT text, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' ORDER BY text",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(knowledge.some((item) => item.text === oldPolicy && item.status === "current"
      && item.support_incomplete === 1),
    "a source-order inference over a truncated support set must not supersede current knowledge");
    assert.ok(knowledge.some((item) => item.text === "Report output format must be TOML." && item.status === "supported"));
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].count, 0);

    const context = await runtime.callMcp(MEMORY_TEST_CLIENT_TOKEN, "context_resolve", {
      repository: MEMORY_TEST_REPOSITORY,
      query: "Report output format",
      limit: 10,
    }, 82);
    assert.equal(context.status, 200);
    const contextJson = JSON.parse(context.body.result.content.find((item) => item.type === "text").text);
    const selected = contextJson.constraints.find((item) => item.text === oldPolicy);
    assert.equal(selected.support_incomplete, true,
      "cloud context must disclose that some support references were omitted");
    assert.ok(contextJson.partial.reasons.includes("knowledge_support_incomplete"));

    const bound = await syncInstructionBatch(runtime, {
      ...sourceA,
      entries: [{
        revision: 18,
        content: repositoryPolicy("Report output format must be CSV.", {
          changed: true,
          predecessor: oldPolicy,
        }),
      }],
    });
    await waitForMemoryHead(runtime, bound.cloud_head_seq);
    knowledge = await runtime.querySql(
      "SELECT text, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' ORDER BY text",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(knowledge.some((item) => item.text === oldPolicy && item.status === "superseded"));
    assert.ok(knowledge.some((item) => item.text === "Report output format must be CSV." && item.status === "current"));
  } finally {
    await runtime.dispose();
  }
});

test("two hosts supporting the same quote in one worker batch block an unbound host change", {
  timeout: 45_000,
}, async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-cross-host-support-"));
  const hosts = {
    "cross-host-a": "cross-token-a",
    "cross-host-b": "cross-token-b",
  };
  let disabled;
  let runtime;
  try {
    disabled = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      memoryEnabled: false,
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
    });
    const oldPolicy = "Report output format must be JSON.";
    const first = await syncInstructionBatch(disabled, {
      hostId: "cross-host-a", token: hosts["cross-host-a"], sessionId: "cross-session-a",
      entries: [{ revision: 1, content: repositoryPolicy(oldPolicy) }],
    });
    const second = await syncInstructionBatch(disabled, {
      hostId: "cross-host-b", token: hosts["cross-host-b"], sessionId: "cross-session-b",
      entries: [{ revision: 1, content: repositoryPolicy(oldPolicy) }],
    });
    assert.equal(second.cloud_head_seq, first.cloud_head_seq + 1);
    await disabled.dispose();
    disabled = null;

    runtime = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryEnabled: true,
      memoryExtractor: "fixture",
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
      maxQueueRetries: 0,
    });
    await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: second.cloud_head_seq,
    });
    await waitForMemoryHead(runtime, second.cloud_head_seq);

    const old = (await runtime.querySql(
      "SELECT knowledge_id, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' AND text = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, oldPolicy],
    ))[0];
    assert.equal(old.status, "current");
    assert.equal(old.support_incomplete, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, old.knowledge_id],
    ))[0].count, 2, "same-batch duplicate merge must persist both independent host supports");

    const changed = await syncInstructionBatch(runtime, {
      hostId: "cross-host-a", token: hosts["cross-host-a"], sessionId: "cross-session-a",
      entries: [{
        revision: 2,
        content: repositoryPolicy("Report output format must be TOML.", { changed: true }),
      }],
    });
    await waitForMemoryHead(runtime, changed.cloud_head_seq);
    const constraints = await runtime.querySql(
      "SELECT text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(constraints.some((item) => item.text === oldPolicy && item.status === "current"));
    assert.ok(constraints.some((item) => item.text === "Report output format must be TOML." && item.status === "supported"));
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].count, 0);
  } finally {
    if (disabled) await disabled.dispose();
    if (runtime) await runtime.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});

test("same-batch reaffirmation retains earlier database provenance before evaluating a change", {
  timeout: 45_000,
}, async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-overlay-provenance-"));
  const hosts = {
    "overlay-source-a": "overlay-token-a",
    "overlay-source-b": "overlay-token-b",
  };
  const oldPolicy = "Report output format must be JSON.";
  let disabled;
  let enabled;
  try {
    disabled = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      memoryEnabled: false,
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
    });
    const first = await syncInstructionBatch(disabled, {
      hostId: "overlay-source-a", token: hosts["overlay-source-a"], sessionId: "overlay-session-a",
      entries: [{ revision: 1, content: repositoryPolicy(oldPolicy) }],
    });
    await disabled.dispose();
    disabled = null;

    enabled = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryEnabled: true,
      memoryExtractor: "fixture",
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
      maxQueueRetries: 0,
    });
    await enabled.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: first.cloud_head_seq,
    });
    await waitForMemoryHead(enabled, first.cloud_head_seq);
    await enabled.dispose();
    enabled = null;

    disabled = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryEnabled: false,
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
    });
    await syncInstructionBatch(disabled, {
      hostId: "overlay-source-b", token: hosts["overlay-source-b"], sessionId: "overlay-session-b",
      entries: [{ revision: 1, content: repositoryPolicy(oldPolicy) }],
    });
    const queuedChange = await syncInstructionBatch(disabled, {
      hostId: "overlay-source-a", token: hosts["overlay-source-a"], sessionId: "overlay-session-a",
      entries: [{
        revision: 2,
        content: repositoryPolicy("Report output format must be TOML.", { changed: true }),
      }],
    });
    await disabled.dispose();
    disabled = null;

    enabled = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryEnabled: true,
      memoryExtractor: "fixture",
      bindings: { HOST_TOKENS_JSON: JSON.stringify(hosts) },
      maxQueueRetries: 0,
    });
    await enabled.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: queuedChange.cloud_head_seq,
    });
    await waitForMemoryHead(enabled, queuedChange.cloud_head_seq);

    const constraints = await enabled.querySql(
      "SELECT knowledge_id, text, status, support_incomplete FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(constraints.some((item) => item.text === oldPolicy && item.status === "current"));
    assert.ok(constraints.some((item) => item.text === "Report output format must be TOML." && item.status === "supported"));
    assert.equal((await enabled.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, constraints.find((item) => item.text === oldPolicy).knowledge_id],
    ))[0].count, 2, "the overlay should persist the independent reaffirmation support");
    assert.equal((await enabled.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].count, 0,
    "a same-source order claim cannot ignore the earlier cross-host support retained from D1");
  } finally {
    if (disabled) await disabled.dispose();
    if (enabled) await enabled.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});

test("mixed Japanese and Latin policy subjects do not collide or supersede an unrelated policy", {
  timeout: 45_000,
}, async () => {
  const outputPolicy = "出力APIの形式は必ずJSONにする。";
  const internalPolicy = "内部APIの形式は必ずTOMLにする。";
  const changedOutputPolicy = "出力APIの形式は必ずYAMLにする。";
  assert.notEqual(
    semanticKeyFor("constraint", outputPolicy),
    semanticKeyFor("constraint", internalPolicy),
    "Japanese subject words must remain part of the semantic identity when ASCII tokens are present",
  );
  assert.equal(
    semanticKeyFor("constraint", outputPolicy),
    semanticKeyFor("constraint", changedOutputPolicy),
    "format value changes must keep the same subject key",
  );

  const runtime = await startMemoryRuntime({ memoryExtractor: "fixture", memoryEnabled: true, maxQueueRetries: 0 });
  try {
    const source = { hostId: MEMORY_TEST_HOST_ID, token: MEMORY_TEST_HOST_TOKEN, sessionId: "mixed-subject-session" };
    const first = await syncInstructionBatch(runtime, {
      ...source,
      entries: [{ revision: 1, content: repositoryPolicy(outputPolicy) }],
    });
    await waitForMemoryHead(runtime, first.cloud_head_seq);
    const second = await syncInstructionBatch(runtime, {
      ...source,
      entries: [{ revision: 2, content: repositoryPolicy(internalPolicy) }],
    });
    await waitForMemoryHead(runtime, second.cloud_head_seq);
    const changed = await syncInstructionBatch(runtime, {
      ...source,
      entries: [{
        revision: 3,
        content: repositoryPolicy(changedOutputPolicy, { changed: true, predecessor: outputPolicy }),
      }],
    });
    await waitForMemoryHead(runtime, changed.cloud_head_seq);

    const constraints = await runtime.querySql(
      "SELECT semantic_key, text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' ORDER BY text",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(constraints.some((item) => item.text === outputPolicy && item.status === "superseded"));
    assert.ok(constraints.some((item) => item.text === changedOutputPolicy && item.status === "current"));
    assert.ok(constraints.some((item) => item.text === internalPolicy && item.status === "current"),
      "an exact predecessor for output API policy must not supersede the unrelated internal API policy");
    assert.equal(new Set(constraints.map((item) => item.semantic_key)).size, 2);
  } finally {
    await runtime.dispose();
  }
});
