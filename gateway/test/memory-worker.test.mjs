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
