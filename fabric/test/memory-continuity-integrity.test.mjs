import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  MEMORY_TEST_REPOSITORY,
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const OWNER = "seeded-owner";
const HOST = "seeded-host";
const SESSION = "seeded-session";
const REPOSITORY = "github:temote-tests/legacy-memory";
const STAMP = "2026-09-28T00:00:00.000Z";
const MIGRATIONS = [
  "migrations/0001_observation_knowledge.sql",
  "migrations/0002_observation_ingest.sql",
  "migrations/0003_memory_worker.sql",
  "migrations/0004_memory_worker_bootstrap.sql",
];

async function exec(runtime, sql, params = []) {
  return runtime.executeSql(sql, params);
}

async function ingestInstruction(runtime, { content, revision = 1, sessionId, taskId }) {
  const observation = {
    id: randomUUID(),
    schema_version: 1,
    observed_at: Math.floor(Date.now() / 1000),
    session_id: sessionId,
    session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "codex" },
    action: "task_start",
    kind: "instruction",
    task_id: taskId,
    operation_id: randomUUID(),
    content: {
      kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
      sha256: createHash("sha256").update(content).digest("hex"), truncated: false,
    },
    evidence_refs: [],
    provenance: { tool: "codex_task_start", source: "orchestration" },
    revision,
    dedupe_key: sessionId + ":instruction:" + revision,
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
        source_base_revision: revision - 1,
        source_head_revision: revision,
        journal_degraded: false,
        gap_count: 0,
        records: [{ source_revision: revision, observation }],
      }),
    },
  );
  assert.equal(response.status, 200, "test instruction should be durably replicated");
  return response.json();
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

async function seedLegacyC0(runtime) {
  await exec(runtime, `INSERT INTO observation_sources (
    owner_id, host_id, session_id, repository_key, source_base_revision,
    source_head_revision, acked_through_revision, cloud_head_seq, last_synced_at
  ) VALUES (?, ?, ?, ?, 0, 1, 1, 1, ?)`, [OWNER, HOST, SESSION, REPOSITORY, STAMP]);
  await exec(runtime, `INSERT INTO observations (
    cloud_seq, owner_id, host_id, session_id, observation_id, source_revision,
    schema_version, repository_key, kind, action, content_kind, content_preview,
    evidence_refs, observed_at, ingested_at
  ) VALUES (1, ?, ?, ?, 'seeded-observation', 1, 1, ?, 'instruction', 'task_start',
    'text', 'Repository policy: report output must be JSON.', '[]', ?, ?)`,
  [OWNER, HOST, SESSION, REPOSITORY, STAMP, STAMP]);
  await exec(runtime, `INSERT INTO knowledge_items (
    knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
    semantic_key, text, status, confidence, valid_from, producer,
    producer_version, produced_at, source_through_cloud_seq
  ) VALUES
    ('legacy-old', ?, ?, 'repository', ?, 'constraint', 'report-format-json',
     'Report output format must be JSON.', 'superseded', 0.9, ?, 'fixture', 'memory-v0', ?, 1),
    ('legacy-new', ?, ?, 'repository', ?, 'constraint', 'report-format-toml',
     'Report output format must be TOML.', 'current', 0.9, ?, 'fixture', 'memory-v0', ?, 1)`,
  [OWNER, REPOSITORY, REPOSITORY, STAMP, STAMP, OWNER, REPOSITORY, REPOSITORY, STAMP, STAMP]);
  await exec(runtime, `INSERT INTO knowledge_support (
    owner_id, repository_key, knowledge_id, observation_cloud_seq, observation_id, support_role
  ) VALUES (?, ?, 'legacy-old', 1, 'seeded-observation', 'support'),
           (?, ?, 'legacy-new', 1, 'seeded-observation', 'support')`,
  [OWNER, REPOSITORY, OWNER, REPOSITORY]);
  await exec(runtime, `INSERT INTO knowledge_supersession (
    owner_id, repository_key, new_knowledge_id, old_knowledge_id, relationship, created_at
  ) VALUES (?, ?, 'legacy-new', 'legacy-old', 'explicit_change', ?)`, [OWNER, REPOSITORY, STAMP]);
}

test("D1 migration 0003 preserves seeded C0 knowledge, support, and supersession rows", async () => {
  const runtime = await startMemoryRuntime({ applyMigrations: false });
  try {
    const source = await fs.readFile(new URL("../migrations/0001_observation_knowledge.sql", import.meta.url), "utf8");
    await exec(runtime, source);
    await seedLegacyC0(runtime);
    const c1 = await fs.readFile(new URL("../migrations/0002_observation_ingest.sql", import.meta.url), "utf8");
    await exec(runtime, c1);
    const c4 = await fs.readFile(new URL("../migrations/0003_memory_worker.sql", import.meta.url), "utf8");
    await exec(runtime, c4);

    assert.deepEqual(await runtime.querySql(
      "SELECT knowledge_id, status FROM knowledge_items ORDER BY knowledge_id"),
    [
      { knowledge_id: "legacy-new", status: "current" },
      { knowledge_id: "legacy-old", status: "superseded" },
    ]);
    assert.deepEqual(await runtime.querySql(
      "SELECT knowledge_id, observation_id FROM knowledge_support ORDER BY knowledge_id"),
    [
      { knowledge_id: "legacy-new", observation_id: "seeded-observation" },
      { knowledge_id: "legacy-old", observation_id: "seeded-observation" },
    ]);
    assert.deepEqual(await runtime.querySql(
      "SELECT new_knowledge_id, old_knowledge_id, relationship FROM knowledge_supersession"),
    [{ new_knowledge_id: "legacy-new", old_knowledge_id: "legacy-old", relationship: "explicit_change" }]);
    assert.deepEqual(await runtime.querySql("PRAGMA foreign_key_check"), []);

    // C0's semantic-key uniqueness prevented retaining a changed assertion
    // beside its old projection. C4 must permit the history and keep both rows.
    await exec(runtime, `INSERT INTO knowledge_items (
      knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
      semantic_key, text, status, producer, producer_version, produced_at
    ) VALUES ('legacy-revision', ?, ?, 'repository', ?, 'constraint',
      'report-format-json', 'Report output format reverted to JSON.', 'current',
      'fixture', 'memory-v0', ?)`, [OWNER, REPOSITORY, REPOSITORY, STAMP]);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE semantic_key = 'report-format-json'"))[0].n, 2);
  } finally {
    await runtime.dispose();
  }
});

test("D1 migration 0004 wakes the memory worker for repositories with only retained observations", async () => {
  const runtime = await startMemoryRuntime({ applyMigrations: false });
  try {
    // A database migrated from the pre-worker releases: observations were
    // durably retained, but no ingest ever wrote a memory_outbox row because
    // the worker machinery did not exist yet. The source host may already be
    // offline, so no future sync can be relied on to wake the worker.
    for (const migration of MIGRATIONS.slice(0, 2)) {
      const source = await fs.readFile(new URL("../" + migration, import.meta.url), "utf8");
      await exec(runtime, source);
    }
    const LEGACY_OTHER_REPOSITORY = "github:temote-tests/legacy-other";
    const policyJson = [
      "For this repository, the repository-level policy is:",
      "Report output format must be JSON.",
    ].join("\n");
    const policyToml = [
      "For this repository, the repository-level policy has changed:",
      "Report output format must be TOML.",
      "Previous repository-level policy to replace: Report output format must be JSON.",
    ].join("\n");
    const policyYaml = [
      "For this repository, the repository-level policy is:",
      "Report output format must be YAML.",
    ].join("\n");
    await exec(runtime, `INSERT INTO observation_sources (
      owner_id, host_id, session_id, repository_key, source_base_revision,
      source_head_revision, acked_through_revision, cloud_head_seq, last_synced_at
    ) VALUES (?, ?, 'legacy-session', ?, 0, 2, 2, 2, ?),
             (?, 'legacy-host-b', 'legacy-session-b', ?, 0, 1, 1, 1, ?),
             (?, 'legacy-host-c', 'legacy-session-c', NULL, 0, 1, 1, 1, ?)`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, MEMORY_TEST_REPOSITORY, STAMP,
      MEMORY_TEST_OWNER, LEGACY_OTHER_REPOSITORY, STAMP,
      MEMORY_TEST_OWNER, STAMP]);
    await exec(runtime, `INSERT INTO observations (
      cloud_seq, owner_id, host_id, session_id, observation_id, source_revision,
      schema_version, repository_key, kind, action, content_kind, content_preview,
      evidence_refs, observed_at, ingested_at, payload_digest
    ) VALUES
      (1, ?, ?, 'legacy-session', 'legacy-obs-1', 1, 1, ?, 'instruction', 'task_start',
       'text', ?, '[]', ?, ?, ?),
      (2, ?, ?, 'legacy-session', 'legacy-obs-2', 2, 1, ?, 'instruction', 'task_start',
       'text', ?, '[]', ?, ?, ?),
      (3, ?, 'legacy-host-b', 'legacy-session-b', 'legacy-obs-3', 1, 1, ?, 'instruction',
       'task_start', 'text', ?, '[]', ?, ?, ?),
      (4, ?, 'legacy-host-c', 'legacy-session-c', 'legacy-obs-4', 1, 1, NULL, 'instruction',
       'task_start', 'text', 'unscoped legacy note', '[]', ?, ?, ?),
      (5, ?, 'legacy-host-c', 'legacy-session-c', 'legacy-obs-5', 2, 1, '', 'instruction',
       'task_start', 'text', 'empty repository key note', '[]', ?, ?, ?)`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, MEMORY_TEST_REPOSITORY, policyJson, STAMP, STAMP, "a".repeat(64),
      MEMORY_TEST_OWNER, MEMORY_TEST_HOST_ID, MEMORY_TEST_REPOSITORY, policyToml, STAMP, STAMP, "b".repeat(64),
      MEMORY_TEST_OWNER, LEGACY_OTHER_REPOSITORY, policyYaml, STAMP, STAMP, "c".repeat(64),
      MEMORY_TEST_OWNER, STAMP, STAMP, "d".repeat(64),
      MEMORY_TEST_OWNER, STAMP, STAMP, "e".repeat(64)]);

    // 0003 creates the worker tables but leaves the outbox empty, so the
    // scheduled sweep finds nothing due even though observations are retained.
    const workerMigration = await fs.readFile(new URL("../" + MIGRATIONS[2], import.meta.url), "utf8");
    await exec(runtime, workerMigration);
    assert.deepEqual(await runtime.querySql(
      "SELECT COUNT(*) AS n FROM memory_outbox"), [{ n: 0 }],
      "a migrated database must start with an empty worker outbox");
    await runtime.runScheduled();
    assert.deepEqual(await runtime.queueDispatches(), [],
      "without bootstrap, the sweep cannot start the worker from retained observations");
    assert.deepEqual(await runtime.querySql(
      "SELECT COUNT(*) AS n FROM memory_checkpoints"), [{ n: 0 }]);

    // The bootstrap migration must seed one pending outbox row per repository
    // at its retained MAX(cloud_seq), and must not enqueue observations whose
    // repository_key is NULL or '' (ingest skips both).
    const bootstrap = await fs.readFile(
      new URL("../" + MIGRATIONS[3], import.meta.url), "utf8");
    await exec(runtime, bootstrap);
    const outbox = await runtime.querySql(
      `SELECT owner_id, repository_key, through_cloud_seq, queued_at, attempt_count,
              next_attempt_at, last_error_code, updated_at
       FROM memory_outbox ORDER BY repository_key`);
    assert.equal(outbox.length, 2, "exactly one outbox row per retained repository");
    assert.deepEqual(outbox.map((row) => ({
      owner_id: row.owner_id,
      repository_key: row.repository_key,
      through_cloud_seq: Number(row.through_cloud_seq),
      queued_at: row.queued_at,
      attempt_count: Number(row.attempt_count),
      next_attempt_at: Number(row.next_attempt_at),
      last_error_code: row.last_error_code,
    })), [
      { owner_id: MEMORY_TEST_OWNER, repository_key: LEGACY_OTHER_REPOSITORY,
        through_cloud_seq: 3, queued_at: null, attempt_count: 0,
        next_attempt_at: 0, last_error_code: null },
      { owner_id: MEMORY_TEST_OWNER, repository_key: MEMORY_TEST_REPOSITORY,
        through_cloud_seq: 2, queued_at: null, attempt_count: 0,
        next_attempt_at: 0, last_error_code: null },
    ]);
    for (const row of outbox) {
      assert.ok(Number.isFinite(Date.parse(row.updated_at)),
        "backfilled outbox rows must carry a parseable updated_at");
    }

    // The scheduled sweep now wakes each repository through the real queue and
    // the worker consumes the retained backlog from scratch.
    await runtime.runScheduled();
    await runtime.waitFor(async () => {
      const checkpoints = await runtime.querySql(
        `SELECT repository_key, last_cloud_seq FROM memory_checkpoints
         WHERE owner_id = ?`, [MEMORY_TEST_OWNER]);
      return checkpoints.length === 2
        && checkpoints.every((row) => Number(row.last_cloud_seq) === (
          row.repository_key === MEMORY_TEST_REPOSITORY ? 2 : 3));
    }, { timeoutMs: 15_000, intervalMs: 25 });
    const runs = await runtime.querySql(
      `SELECT repository_key, status, from_seq, to_seq, input_count, outcome
       FROM memory_runs WHERE owner_id = ? ORDER BY repository_key`, [MEMORY_TEST_OWNER]);
    assert.deepEqual(runs.map((run) => ({
      repository_key: run.repository_key,
      status: run.status,
      from_seq: Number(run.from_seq),
      to_seq: Number(run.to_seq),
      input_count: Number(run.input_count),
      outcome: run.outcome,
    })), [
      { repository_key: LEGACY_OTHER_REPOSITORY, status: "completed",
        from_seq: 0, to_seq: 3, input_count: 1, outcome: "projected" },
      { repository_key: MEMORY_TEST_REPOSITORY, status: "completed",
        from_seq: 0, to_seq: 2, input_count: 2, outcome: "projected" },
    ]);
    const knowledge = await runtime.querySql(
      `SELECT repository_key, kind, text, status FROM knowledge_items
       WHERE owner_id = ? ORDER BY repository_key, kind, text`, [MEMORY_TEST_OWNER]);
    assert.ok(knowledge.some((item) => item.repository_key === MEMORY_TEST_REPOSITORY
      && item.kind === "constraint" && item.status === "current"
      && item.text === "Report output format must be TOML."));
    assert.ok(knowledge.some((item) => item.repository_key === MEMORY_TEST_REPOSITORY
      && item.kind === "constraint" && item.status === "superseded"
      && item.text === "Report output format must be JSON."));
    assert.ok(knowledge.some((item) => item.repository_key === LEGACY_OTHER_REPOSITORY
      && item.kind === "constraint" && item.status === "current"
      && item.text === "Report output format must be YAML."));
    const context = await repositoryContext(runtime);
    assert.equal(context.memory.stale, false);
    assert.ok(context.constraints.some((item) => item.status === "current"
      && item.text === "Report output format must be TOML."));

    // Re-applying the migration keeps the durable outbox state instead of
    // requeueing repositories whose retained head is already consumed.
    await exec(runtime, bootstrap);
    const reApplied = await runtime.querySql(
      `SELECT repository_key, through_cloud_seq, queued_at FROM memory_outbox
       ORDER BY repository_key`);
    assert.equal(reApplied.length, 2);
    assert.deepEqual(reApplied.map((row) => ({
      repository_key: row.repository_key,
      through_cloud_seq: Number(row.through_cloud_seq),
    })), [
      { repository_key: LEGACY_OTHER_REPOSITORY, through_cloud_seq: 3 },
      { repository_key: MEMORY_TEST_REPOSITORY, through_cloud_seq: 2 },
    ]);
    for (const row of reApplied) {
      assert.ok(Number.isFinite(Date.parse(row.queued_at)),
        "re-application must not requeue a repository already covered by a checkpoint");
    }
  } finally {
    await runtime.dispose();
  }
});

test("workerd D1 batch rollback and commit guard reject partial projection checkpoint commits", async () => {
  const runtime = await startMemoryRuntime();
  try {
    const owner = "owner-a";
    const repo = MEMORY_TEST_REPOSITORY;
    const producer = "memory-v1-test";
    const worker = "temote-memory-v1";
    const nowSeconds = Math.floor(Date.now() / 1000);
    const leaseUntil = nowSeconds + 300;
    await exec(runtime, `INSERT INTO observation_sources (
      owner_id, host_id, session_id, repository_key, source_base_revision,
      source_head_revision, acked_through_revision, cloud_head_seq, last_synced_at
    ) VALUES (?, 'test-host', 'test-session', ?, 0, 1, 1, 1, ?)`, [owner, repo, STAMP]);
    await exec(runtime, `INSERT INTO observations (
      owner_id, host_id, session_id, observation_id, source_revision,
      schema_version, repository_key, kind, evidence_refs, observed_at, ingested_at, payload_digest
    ) VALUES (?, 'test-host', 'test-session', 'test-observation', 1, 1, ?,
      'instruction', '[]', ?, ?, ?)`, [owner, repo, STAMP, STAMP, "a".repeat(64)]);
    await exec(runtime, `INSERT INTO memory_projection_heads (
      owner_id, repository_key, requested_generation, requested_producer_version, epoch
    ) VALUES (?, ?, 1, ?, 1)`, [owner, repo, producer]);
    await exec(runtime, `INSERT INTO memory_checkpoints (
      owner_id, repository_key, worker_id, producer_version, last_cloud_seq,
      stale, lease_token, lease_until, fence, projection_epoch
    ) VALUES (?, ?, ?, ?, 0, 1, 'lease-a', ?, 1, 1)`,
    [owner, repo, worker, producer, leaseUntil]);
    await exec(runtime, `INSERT INTO memory_runs (
      run_id, owner_id, repository_key, producer_version, from_seq, to_seq,
      status, attempt_count, started_at, lease_token, fence, projection_epoch, input_count
    ) VALUES ('run-a', ?, ?, ?, 0, 1, 'running', 1, ?, 'lease-a', 1, 1, 1)`,
    [owner, repo, producer, STAMP]);

    // UPDATE matching no rows is a successful D1 statement. A worker must
    // verify affected-row counts and the transaction guard must reject writes
    // that try to keep a knowledge insert while its checkpoint CAS loses.
    const noMatch = await runtime.executeSql(
      "UPDATE memory_checkpoints SET last_cloud_seq = 1 WHERE owner_id = ? AND lease_token = ?",
      [owner, "stale-lease"],
    );
    assert.equal(noMatch.result?.meta?.changes ?? noMatch.result?.changes, 0);

    await assert.rejects(runtime.batchSql([
      {
        sql: `INSERT INTO knowledge_items (
          knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
          semantic_key, text, status, producer, producer_version, produced_at, source_through_cloud_seq
        ) VALUES ('would-leak', ?, ?, 'repository', ?, 'fact', 'x', 'uncommitted',
          'supported', 'fixture', ?, ?, 1)`,
        params: [owner, repo, repo, producer, STAMP],
      },
      {
        sql: `UPDATE memory_checkpoints SET last_cloud_seq = 1 WHERE owner_id = ?
          AND repository_key = ? AND worker_id = ? AND producer_version = ?`,
        params: [owner, repo, worker, producer],
      },
    ]), /D1_BATCH_FAILED/);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE knowledge_id = 'would-leak'"))[0].n, 0);
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [owner, repo]))[0].last_cloud_seq, 0);
    assert.equal((await runtime.querySql(
      "SELECT status FROM memory_runs WHERE run_id = 'run-a'"))[0].status, "running");
  } finally {
    await runtime.dispose();
  }
});

test("workerd D1 conditional claim serializes concurrent repository workers", async () => {
  const runtime = await startMemoryRuntime();
  try {
    const owner = "owner-race";
    const repo = "github:temote-tests/claim-race";
    const producer = "memory-v1-race";
    const worker = "temote-memory-v1";
    const nowSeconds = Math.floor(Date.now() / 1000);
    await exec(runtime, `INSERT INTO memory_projection_heads (
      owner_id, repository_key, requested_generation, requested_producer_version, epoch
    ) VALUES (?, ?, 1, ?, 1)`, [owner, repo, producer]);
    await exec(runtime, `INSERT INTO memory_checkpoints (
      owner_id, repository_key, worker_id, producer_version, last_cloud_seq,
      stale, projection_epoch
    ) VALUES (?, ?, ?, ?, 0, 1, 1)`, [owner, repo, worker, producer]);

    const claimBatch = (suffix) => {
      const runId = "race-run-" + suffix;
      const guardId = "race-guard-" + suffix;
      const leaseToken = "race-lease-" + suffix;
      return runtime.batchSql([
        {
          sql: `UPDATE memory_checkpoints SET lease_token = ?, lease_until = ?,
            fence = fence + 1, projection_epoch = 1
            WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?
              AND last_cloud_seq = 0 AND projection_epoch = 1
              AND (lease_until IS NULL OR lease_until <= ?)
              AND EXISTS (SELECT 1 FROM memory_projection_heads AS head
                WHERE head.owner_id = memory_checkpoints.owner_id
                  AND head.repository_key = memory_checkpoints.repository_key
                  AND head.epoch = 1 AND head.requested_producer_version = ?)`,
          params: [leaseToken, nowSeconds + 300, owner, repo, worker, producer, nowSeconds, producer],
        },
        {
          sql: `INSERT INTO memory_claim_guards (
            guard_id, owner_id, repository_key, worker_id, producer_version,
            run_id, from_seq, to_seq, fence, lease_token, projection_epoch, max_attempts
          ) VALUES (?, ?, ?, ?, ?, ?, 0, 1, 1, ?, 1, 3)`,
          params: [guardId, owner, repo, worker, producer, runId, leaseToken],
        },
        {
          sql: `INSERT INTO memory_runs (
            run_id, owner_id, repository_key, producer_version, from_seq, to_seq,
            status, attempt_count, started_at, lease_token, fence, projection_epoch, input_count
          ) VALUES (?, ?, ?, ?, 0, 1, 'running', 1, ?, ?, 1, 1, 1)`,
          params: [runId, owner, repo, producer, STAMP, leaseToken],
        },
        { sql: "DELETE FROM memory_claim_guards WHERE guard_id = ?", params: [guardId] },
      ]);
    };
    const outcomes = await Promise.allSettled([claimBatch("a"), claimBatch("b")]);
    assert.equal(outcomes.filter((outcome) => outcome.status === "fulfilled").length, 1);
    assert.equal(outcomes.filter((outcome) => outcome.status === "rejected").length, 1);

    const checkpoint = (await runtime.querySql(
      `SELECT lease_token, fence, last_cloud_seq FROM memory_checkpoints
       WHERE owner_id = ? AND repository_key = ?`, [owner, repo]))[0];
    assert.ok(["race-lease-a", "race-lease-b"].includes(checkpoint.lease_token));
    assert.equal(checkpoint.fence, 1);
    assert.equal(checkpoint.last_cloud_seq, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [owner, repo]))[0].n, 1, "losing worker's run row must roll back with its failed guard");
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM memory_claim_guards WHERE owner_id = ? AND repository_key = ?",
      [owner, repo]))[0].n, 0);
  } finally {
    await runtime.dispose();
  }
});

test("D1 commit guard rolls back projection when conditional checkpoint CAS affects zero rows", async () => {
  const runtime = await startMemoryRuntime();
  try {
    const owner = "owner-cas-loser";
    const repo = "github:temote-tests/cas-loser";
    const producer = "memory-v1-cas";
    const worker = "temote-memory-v1";
    const leaseUntil = Math.floor(Date.now() / 1000) + 300;
    await exec(runtime, `INSERT INTO observation_sources (
      owner_id, host_id, session_id, repository_key, source_base_revision,
      source_head_revision, acked_through_revision, cloud_head_seq, last_synced_at
    ) VALUES (?, 'cas-host', 'cas-session', ?, 1, 1, 1, 1, ?)`, [owner, repo, STAMP]);
    await exec(runtime, `INSERT INTO observations (
      cloud_seq, owner_id, host_id, session_id, observation_id, source_revision,
      schema_version, repository_key, kind, action, content_kind, content_preview,
      evidence_refs, observed_at, ingested_at, payload_digest
    ) VALUES (1, ?, 'cas-host', 'cas-session', 'cas-observation', 1, 1, ?,
      'instruction', 'task_start', 'text', 'Report output format must be JSON.', '[]', ?, ?, ?)`,
    [owner, repo, STAMP, STAMP, "b".repeat(64)]);
    await exec(runtime, `INSERT INTO memory_projection_heads (
      owner_id, repository_key, requested_generation, requested_producer_version, epoch
    ) VALUES (?, ?, 1, ?, 1)`, [owner, repo, producer]);
    await exec(runtime, `INSERT INTO memory_checkpoints (
      owner_id, repository_key, worker_id, producer_version, last_cloud_seq, stale,
      lease_token, lease_until, fence, projection_epoch
    ) VALUES (?, ?, ?, ?, 0, 1, 'live-lease', ?, 1, 1)`,
    [owner, repo, worker, producer, leaseUntil]);
    await exec(runtime, `INSERT INTO memory_runs (
      run_id, owner_id, repository_key, producer_version, from_seq, to_seq,
      status, attempt_count, started_at, lease_token, fence, projection_epoch, input_count
    ) VALUES ('cas-run', ?, ?, ?, 0, 1, 'running', 1, ?, 'live-lease', 1, 1, 1)`,
    [owner, repo, producer, STAMP]);

    await assert.rejects(runtime.batchSql([
      {
        sql: `INSERT INTO memory_commit_guards (
          guard_id, owner_id, repository_key, worker_id, producer_version,
          run_id, from_seq, to_seq, fence, lease_token, projection_epoch, publish_projection, created_at
        ) VALUES ('cas-guard', ?, ?, ?, ?, 'cas-run', 0, 1, 1, 'live-lease', 1, 1, ?)`,
        params: [owner, repo, worker, producer, STAMP],
      },
      {
        sql: `INSERT INTO knowledge_items (
          knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
          semantic_key, text, status, producer, producer_version, produced_at, source_through_cloud_seq
        ) VALUES ('cas-loser-knowledge', ?, ?, 'repository', ?, 'constraint',
          'report-format', 'Report output format must be JSON.', 'current',
          'temote-memory', ?, ?, 1)`,
        params: [owner, repo, repo, producer, STAMP],
      },
      {
        sql: `INSERT INTO knowledge_support (
          owner_id, repository_key, knowledge_id, observation_cloud_seq, observation_id, support_role
        ) VALUES (?, ?, 'cas-loser-knowledge', 1, 'cas-observation', 'direct_quote')`,
        params: [owner, repo],
      },
      {
        sql: `UPDATE memory_projection_heads SET active_generation = 1, active_producer_version = ?
          WHERE owner_id = ? AND repository_key = ? AND epoch = 1`,
        params: [producer, owner, repo],
      },
      {
        sql: `UPDATE memory_checkpoints SET last_cloud_seq = 1, lease_token = NULL, lease_until = NULL
          WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?
            AND last_cloud_seq = 0 AND lease_token = 'stale-lease' AND fence = 1 AND projection_epoch = 1`,
        params: [owner, repo, worker, producer],
      },
      {
        sql: `UPDATE memory_runs SET status = 'completed', completed_at = ?, outcome = 'projected', lease_token = NULL
          WHERE run_id = 'cas-run' AND status = 'running' AND lease_token = 'stale-lease' AND fence = 1`,
        params: [STAMP],
      },
      { sql: "DELETE FROM memory_commit_guards WHERE guard_id = 'cas-guard'", params: [] },
    ]), /D1_BATCH_FAILED/);

    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE knowledge_id = 'cas-loser-knowledge'"))[0].n, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_support WHERE knowledge_id = 'cas-loser-knowledge'"))[0].n, 0);
    assert.deepEqual((await runtime.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [owner, repo]))[0], { active_generation: null, active_producer_version: null });
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [owner, repo]))[0].last_cloud_seq, 0);
    assert.equal((await runtime.querySql(
      "SELECT status FROM memory_runs WHERE run_id = 'cas-run'"))[0].status, "running");
  } finally {
    await runtime.dispose();
  }
});

test("repository memory runtime offers the real queue consumer and scheduled sweep", async () => {
  const runtime = await startMemoryRuntime({ memoryEnabled: false });
  try {
    assert.equal(runtime.queueName, "temote-memory-test");
    assert.equal(typeof runtime.enqueue, "function");
    assert.equal(typeof runtime.runScheduled, "function");
    const response = await runtime.fetch("https://memory-test.local/__memory_test/health", {
      method: "POST", headers: { "content-type": "application/json" }, body: "{}",
    });
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { success: true });
  } finally {
    await runtime.dispose();
  }
});

test("D1 outbox and scheduled sweep recover an ingest after Queue send failure", async () => {
  const runtime = await startMemoryRuntime({
    bindings: { MEMORY_TEST_QUEUE_SEND_FAILURES: "1" },
  });
  try {
    const content = "For this repository, the repository-level policy is:\nReport output format must be JSON.";
    const id = randomUUID();
    const digest = createHash("sha256").update(content).digest("hex");
    const observation = {
      id,
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "outbox-recovery-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "outbox-recovery-task",
      operation_id: randomUUID(),
      content: {
        kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
        sha256: digest, truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "outbox-recovery:instruction:1",
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
          session_id: observation.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200, "Queue outage must not undo accepted D1 ingest");
    const sync = await response.json();
    assert.equal(sync.acked_through_revision, 1);
    assert.equal(await runtime.queueDispatchCount(), 0, "the injected first send fails before dispatch");
    const failedOutbox = (await runtime.querySql(
      "SELECT queued_at, attempt_count, last_error_code FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(failedOutbox.queued_at, null);
    assert.equal(failedOutbox.attempt_count, 1);
    assert.equal(failedOutbox.last_error_code, "queue_send_failed");

    // Advance the durable retry deadline as if the bounded backoff interval
    // elapsed; recovery itself is driven by the real production scheduled hook.
    await exec(runtime,
      "UPDATE memory_outbox SET next_attempt_at = 0 WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    const sweep = await runtime.runScheduled();
    assert.equal(sweep.success, true);
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > 0 && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });
    await runtime.waitFor(async () => {
      const checkpoint = await runtime.querySql(
        "SELECT last_cloud_seq, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === Number(sync.cloud_head_seq)
        && checkpoint[0].last_error_code == null;
    }, { timeoutMs: 10_000, intervalMs: 25 });
    assert.equal((await runtime.querySql(
      "SELECT status, outcome FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].status, "completed");
    assert.ok((await runtime.queueDispatchCount()) > 0);
  } finally {
    await runtime.dispose();
  }
});

test("successful extractor output retries after an atomic D1 commit failure without advancing checkpoint", async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "provider-contract-test",
    memoryApiKey: "test-only-provider-key",
    memoryTestProviderMode: "echo_allowed_repository_clauses",
    maxQueueRetries: 0,
    bindings: { MEMORY_MAX_ATTEMPTS: "2" },
  });
  try {
    const content = [
      "For this repository, the repository-level policy is:",
      "Report output format must be JSON.",
      "Open question: The required report field set remains undecided.",
    ].join("\n");
    const id = randomUUID();
    const observation = {
      id,
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "commit-retry-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "commit-retry-task",
      operation_id: randomUUID(),
      content: {
        kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"), truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "commit-retry:instruction:1",
    };
    await exec(runtime, `CREATE TRIGGER reject_checkpoint_once BEFORE UPDATE OF last_cloud_seq
      ON memory_checkpoints WHEN NEW.last_cloud_seq > OLD.last_cloud_seq
      BEGIN SELECT RAISE(ABORT, 'test_checkpoint_commit_failure'); END`);
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
          session_id: observation.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200, "accepted ingest must survive later projection failure");
    const sync = await response.json();
    try {
      await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length === 1 && runs[0].error_code === "commit_rejected";
      }, { timeoutMs: 3_000, intervalMs: 25 });
    } catch {
      const snapshot = {
        runs: await runtime.querySql("SELECT status, attempt_count, error_code FROM memory_runs"),
        checkpoints: await runtime.querySql("SELECT last_cloud_seq, last_error_code FROM memory_checkpoints"),
        outbox: await runtime.querySql("SELECT queued_at, next_attempt_at, last_error_code FROM memory_outbox"),
        dispatches: await runtime.queueDispatches(),
      };
      assert.fail("worker did not record the commit failure: " + JSON.stringify(snapshot));
    }
    const failedRun = (await runtime.querySql(
      "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(failedRun.error_code, "commit_rejected", "provider response was accepted before D1 commit failed");
    assert.equal(failedRun.attempt_count, 1);
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].last_cloud_seq, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0, "the failed batch must leave no partial knowledge");

    await exec(runtime, "DROP TRIGGER reject_checkpoint_once");
    await exec(runtime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    await runtime.runScheduled();
    await runtime.waitFor(async () => {
      const checkpoint = await runtime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === Number(sync.cloud_head_seq);
    }, { timeoutMs: 10_000, intervalMs: 25 });
    const completedRun = (await runtime.querySql(
      "SELECT status, attempt_count, outcome FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.deepEqual(completedRun, { status: "completed", attempt_count: 2, outcome: "projected" });
    const knowledge = await runtime.querySql(
      "SELECT kind, text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.ok(knowledge.some((item) => item.text === "Report output format must be JSON." && item.status === "current"));
    assert.ok(knowledge.some((item) => item.kind === "unresolved"
      && item.text === "The required report field set remains undecided."));
    assert.equal(knowledge.filter((item) => item.kind === "constraint").length, 1);
    assert.equal(knowledge.filter((item) => item.kind === "unresolved").length, 1);
    assert.equal(knowledge.filter((item) => item.kind === "summary").length, 1);
  } finally {
    await runtime.dispose();
  }
});

test("a new workerd runtime resumes durable outbox work after restart", async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-restart-"));
  let firstRuntime;
  let restartedRuntime;
  try {
    firstRuntime = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      bindings: { MEMORY_TEST_QUEUE_SEND_FAILURES: "1" },
    });
    const content = [
      "For this repository, the repository-level policy is:",
      "Report output format must be JSON.",
    ].join("\n");
    const observation = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "restart-recovery-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "restart-recovery-task",
      operation_id: randomUUID(),
      content: {
        kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"), truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "restart-recovery:instruction:1",
    };
    const response = await firstRuntime.fetch(
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
          session_id: observation.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200);
    assert.equal(await firstRuntime.queueDispatchCount(), 0);
    assert.equal((await firstRuntime.querySql(
      "SELECT last_error_code FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].last_error_code, "queue_send_failed");
    await firstRuntime.dispose();
    firstRuntime = null;

    restartedRuntime = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      bindings: { MEMORY_TEST_QUEUE_SEND_FAILURES: "0" },
    });
    assert.equal((await restartedRuntime.querySql(
      "SELECT COUNT(*) AS n FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 1, "the D1 replica must survive worker restart");
    await exec(restartedRuntime, `UPDATE memory_outbox SET next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    await restartedRuntime.runScheduled();
    await restartedRuntime.waitFor(async () => {
      const checkpoint = await restartedRuntime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoint.length === 1 && Number(checkpoint[0].last_cloud_seq) === 1;
    }, { timeoutMs: 10_000, intervalMs: 25 });
    assert.equal((await restartedRuntime.querySql(
      "SELECT status FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].status, "completed");
  } finally {
    if (firstRuntime) await firstRuntime.dispose();
    if (restartedRuntime) await restartedRuntime.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});

test("Queue ACK loss after commit redelivers as a projection no-op", async () => {
  const runtime = await startMemoryRuntime({
    maxQueueRetries: 1,
    bindings: { MEMORY_TEST_QUEUE_ACK_FAILURES: "1" },
  });
  try {
    const content = [
      "For this repository, the repository-level policy is:",
      "Report output format must be JSON.",
    ].join("\n");
    const observation = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "ack-loss-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "ack-loss-task",
      operation_id: randomUUID(),
      content: {
        kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"), truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "ack-loss:instruction:1",
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
          session_id: observation.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200);
    const sync = await response.json();
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length >= 2 && dispatches[0].state === "failed"
        && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const [checkpoint, runs, knowledge, support] = await Promise.all([
      runtime.querySql("SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT status, outcome, attempt_count FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT kind, text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT knowledge_id, observation_id FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
    ]);
    assert.equal(checkpoint[0].last_cloud_seq, sync.cloud_head_seq);
    assert.deepEqual(runs, [{ status: "completed", outcome: "projected", attempt_count: 1 }]);
    assert.equal(knowledge.filter((item) => item.kind === "constraint"
      && item.text === "Report output format must be JSON.").length, 1);
    assert.equal(support.length, 2, "the first commit's support rows must remain single-instance after ACK replay");
  } finally {
    await runtime.dispose();
  }
});

test("execution_state-only batch with an empty provider extraction completes without fabricated knowledge", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "empty-target-contract-test",
    memoryApiKey: "test-only-provider-key",
    // The bounded mock provider returns exactly {items: []} when no allowed
    // repository clause exists. This exercises the production HTTP adapter,
    // Queue consumer, and D1 commit path without using a live model.
    memoryTestProviderMode: "echo_allowed_repository_clauses",
  });
  try {
    const observedAt = Math.floor(Date.now() / 1000);
    const sessionId = "execution-state-empty-session";
    const taskId = "execution-state-empty-task";
    const observation = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: observedAt,
      session_id: sessionId,
      session_instance: { started_at: observedAt, process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_get",
      kind: "execution_state",
      task_id: taskId,
      execution_id: "execution-state-empty-execution",
      operation_id: randomUUID(),
      content: { kind: "none" },
      state_ref: {
        task_id: taskId,
        status: "running",
        revision: 1,
        generation: 1,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_get", source: "orchestration" },
      revision: 1,
      dedupe_key: "execution-state-empty:revision:1",
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
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation }],
        }),
      },
    );
    assert.equal(response.status, 200, "the execution state must be replicated before extraction");
    const sync = await response.json();
    assert.equal(sync.acked_through_revision, 1);

    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length === 1 && dispatches[0].state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const [runRows, checkpointRows, headRows, itemRows, supportRows, supersessionRows] = await Promise.all([
      runtime.querySql(
        "SELECT status, outcome, attempt_count, from_seq, to_seq, input_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT last_cloud_seq, stale, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT requested_producer_version, active_producer_version, requested_generation, active_generation FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, kind, text FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, observation_id FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT new_knowledge_id, old_knowledge_id FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
    ]);

    assert.deepEqual(runRows, [{
      status: "completed",
      outcome: "empty",
      attempt_count: 1,
      from_seq: 0,
      to_seq: sync.cloud_head_seq,
      input_count: 1,
      error_code: null,
    }]);
    assert.deepEqual(checkpointRows, [{ last_cloud_seq: sync.cloud_head_seq, stale: 0, last_error_code: null }]);
    assert.equal(headRows.length, 1);
    assert.equal(headRows[0].active_producer_version, headRows[0].requested_producer_version);
    assert.equal(headRows[0].active_generation, headRows[0].requested_generation);
    assert.deepEqual(itemRows, [], "an execution status alone is not reusable knowledge");
    assert.deepEqual(supportRows, [], "empty extraction must not fabricate support rows");
    assert.deepEqual(supersessionRows, [], "empty extraction must not fabricate history");

    const context = await repositoryContext(runtime);
    assert.equal(context.memory.state, "ready_empty");
    assert.equal(context.current_summary.knowledge_summary, null);
    assert.deepEqual(context.constraints, []);
    assert.equal(context.recent_related_tasks[0].state.status, "running",
      "the structured host observation remains available separately from derived knowledge");
  } finally {
    await runtime.dispose();
  }
});

test("workerd D1 rejects parseable provider output when finish_reason is content_filter", {
  timeout: 30_000,
}, async () => {
  const testDirectory = path.dirname(fileURLToPath(import.meta.url));
  const providerWorkerPath = path.join(testDirectory, `memory-filtered-provider-${randomUUID()}.mjs`);
  let runtime;
  try {
    await fs.writeFile(providerWorkerPath, `
      import production from "../src/index.js";
      export { GatewayRegistry, GatewaySession } from "../src/routing-runtime.js";

      export default {
        ...production,
        async queue(batch, env, ctx) {
          const originalFetch = globalThis.fetch;
          globalThis.fetch = async (input, init) => {
            if (String(input) === env.MEMORY_ENDPOINT) {
              return Response.json({ choices: [{
                message: { role: "assistant", content: "{\\\"items\\\":[]}" },
                finish_reason: "content_filter",
              }] });
            }
            return originalFetch(input, init);
          };
          try {
            return await production.queue(batch, env, ctx);
          } finally {
            globalThis.fetch = originalFetch;
          }
        },
      };
    `, { mode: 0o600 });

    runtime = await startMemoryRuntime({
      memoryExtractor: "openai_compatible",
      memoryEnabled: true,
      memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
      memoryModel: "filtered-response-contract-test",
      memoryApiKey: "test-only-provider-key",
      productionEntryPath: providerWorkerPath,
      maxQueueRetries: 0,
      bindings: { MEMORY_MAX_ATTEMPTS: "1" },
    });
    const sync = await ingestInstruction(runtime, {
      content: [
        "For this repository, the repository-level policy is:",
        "Report output format must be JSON.",
        "Open question: The required report field set remains undecided.",
      ].join("\n"),
      sessionId: "filtered-provider-session",
      taskId: "filtered-provider-task",
    });

    await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length === 1 && runs[0].status === "failed"
        && runs[0].error_code === "provider_incomplete_response";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    assert.deepEqual(await runtime.querySql(
      "SELECT last_cloud_seq, stale, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ), [{ last_cloud_seq: 0, stale: 1, last_error_code: "provider_incomplete_response" }]);
    assert.deepEqual(await runtime.querySql(
      "SELECT status, outcome, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ), [{ status: "failed", outcome: null, attempt_count: 1, error_code: "provider_incomplete_response" }]);
    assert.deepEqual(await runtime.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ), [{ active_generation: null, active_producer_version: null }]);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0, "canonical source clauses must not rescue filtered model output");
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 1, "ingested source remains durable while failed extraction leaves no projection");
    assert.equal(sync.cloud_head_seq, 1);
  } finally {
    try {
      if (runtime) await runtime.dispose();
    } finally {
      await fs.rm(providerWorkerPath, { force: true });
    }
  }
});

test("invalid_support provider output exhausts a fixed batch without advancing the projection", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "invalid-support-contract-test",
    memoryApiKey: "test-only-provider-key",
    // Test-only adapter response: the quote/ref are valid, but the assertion
    // text is paraphrased and must fail exact-support validation.
    memoryTestProviderMode: "paraphrase_allowed_repository_clauses",
    maxQueueRetries: 0,
    bindings: { MEMORY_MAX_ATTEMPTS: "2" },
  });
  try {
    const sync = await ingestInstruction(runtime, {
      content: "For this repository, the repository-level policy is:\nReport output format must be JSON.",
      sessionId: "invalid-support-session",
      taskId: "invalid-support-task",
    });
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      const runs = await runtime.querySql(
        "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return dispatches.length > 0 && dispatches.at(-1).state === "completed"
        && runs.length === 1 && runs[0].status === "pending"
        && runs[0].attempt_count === 1 && runs[0].error_code === "invalid_support";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const firstAttemptCheckpoint = (await runtime.querySql(
      "SELECT last_cloud_seq, stale, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(firstAttemptCheckpoint.last_cloud_seq, 0);
    assert.equal(firstAttemptCheckpoint.stale, 1);
    assert.equal(firstAttemptCheckpoint.last_error_code, "invalid_support");
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0);

    await exec(runtime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    await runtime.runScheduled();
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      const runs = await runtime.querySql(
        "SELECT status, attempt_count, error_code, from_seq, to_seq FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return dispatches.length >= 2 && dispatches.at(-1).state === "completed"
        && runs.length === 1 && runs[0].status === "failed"
        && runs[0].attempt_count === 2 && runs[0].error_code === "invalid_support";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const cappedRun = (await runtime.querySql(
      "SELECT status, outcome, attempt_count, error_code, from_seq, to_seq FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.deepEqual(cappedRun, {
      status: "failed",
      outcome: null,
      attempt_count: 2,
      error_code: "invalid_support",
      from_seq: 0,
      to_seq: sync.cloud_head_seq,
    });
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].last_cloud_seq, 0);
    assert.deepEqual(await runtime.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ), [{ active_generation: null, active_producer_version: null }]);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0);

    const dispatchCountBeforeReplay = await runtime.queueDispatchCount();
    const replay = await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: sync.cloud_head_seq,
    });
    assert.equal(replay.success, true);
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > dispatchCountBeforeReplay
        && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });

    assert.deepEqual((await runtime.querySql(
      "SELECT status, attempt_count, error_code, from_seq, to_seq FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0], {
      status: "failed",
      attempt_count: 2,
      error_code: "invalid_support",
      from_seq: 0,
      to_seq: sync.cloud_head_seq,
    }, "a capped invalid-support batch must not get a fresh retry budget on redelivery");
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].last_cloud_seq, 0);
  } finally {
    await runtime.dispose();
  }
});

test("provider poison batches stop at configured attempts and later ingest does not reset the cap", async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "provider-failure-test",
    memoryApiKey: "test-only-provider-key",
    memoryTestProviderMode: "reject",
    maxQueueRetries: 0,
    bindings: { MEMORY_MAX_ATTEMPTS: "2" },
  });
  const sendObservation = async (revision, content) => {
    const observation = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: Math.floor(Date.now() / 1000),
      session_id: "provider-failure-session",
      session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "provider-failure-task-" + revision,
      operation_id: randomUUID(),
      content: {
        kind: "text", preview: content, total_bytes: Buffer.byteLength(content),
        sha256: createHash("sha256").update(content).digest("hex"), truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision,
      dedupe_key: "provider-failure:instruction:" + revision,
    };
    return runtime.fetch(
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
          session_id: observation.session_id,
          repository_key: MEMORY_TEST_REPOSITORY,
          source_base_revision: revision - 1,
          source_head_revision: revision,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: revision, observation }],
        }),
      },
    );
  };
  try {
    const first = await sendObservation(
      1,
      "For this repository, the repository-level policy is:\nReport output format must be JSON.",
    );
    assert.equal(first.status, 200);
    await runtime.waitFor(async () => (await runtime.queueDispatchCount()) >= 1);
    await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length === 1 && runs[0].attempt_count === 1 && runs[0].error_code === "provider_unavailable";
    }, { timeoutMs: 5000, intervalMs: 25 });

    await exec(runtime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    await runtime.runScheduled();
    await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, attempt_count, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length === 1 && runs[0].status === "failed" && runs[0].attempt_count === 2;
    }, { timeoutMs: 5000, intervalMs: 25 });

    await exec(runtime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    const second = await sendObservation(
      2,
      "For this repository, the repository-level policy has changed:\nReport output format must be TOML.\nOpen question: The required report field set remains undecided.\nPrevious repository-level policy to replace: Report output format must be JSON.",
    );
    assert.equal(second.status, 200, "new observations remain accepted while the old batch is poisoned");
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length >= 3 && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 5000, intervalMs: 25 });
    assert.deepEqual((await runtime.querySql(
      "SELECT status, attempt_count, error_code, from_seq, to_seq FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0], {
      status: "failed", attempt_count: 2, error_code: "provider_unavailable", from_seq: 0, to_seq: 1,
    });
    assert.equal((await runtime.querySql(
      "SELECT last_cloud_seq, stale, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].last_cloud_seq, 0);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 2);
    assert.equal((await runtime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0].n, 0, "provider failures never advance the checkpoint without a projection");
  } finally {
    await runtime.dispose();
  }
});

test("failed rebuild keeps active knowledge readable and a later generation publishes without changing raw observations", async () => {
  const persistencePath = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-rebuild-"));
  let initialRuntime;
  let failedBuildRuntime;
  let rebuiltRuntime;
  const rawObservationHash = async (runtime) => {
    const rows = await runtime.querySql(`SELECT cloud_seq, observation_id, source_revision,
      payload_digest, content_preview, evidence_refs, observed_at, ingested_at
      FROM observations WHERE owner_id = ? AND repository_key = ? ORDER BY cloud_seq`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    return createHash("sha256").update(JSON.stringify(rows)).digest("hex");
  };
  const waitForCompletedDispatch = async (runtime, afterCount) => runtime.waitFor(async () => {
    const rows = await runtime.queueDispatches();
    return rows.length > afterCount && rows.at(-1).state === "completed";
  }, { timeoutMs: 10_000, intervalMs: 25 });

  try {
    initialRuntime = await startMemoryRuntime({ resourcePersistencePath: persistencePath });
    const sync = await ingestInstruction(initialRuntime, {
      content: [
        "For this repository, the repository-level policy is:",
        "Report output format must be JSON.",
      ].join("\n"),
      sessionId: "rebuild-source-session",
      taskId: "rebuild-source-task",
    });
    await initialRuntime.waitFor(async () => {
      const rows = await initialRuntime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return rows.length === 1 && Number(rows[0].last_cloud_seq) === Number(sync.cloud_head_seq);
    }, { timeoutMs: 10_000, intervalMs: 25 });
    const initialHead = (await initialRuntime.querySql(
      "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(initialHead.active_generation, 1);
    const originalHash = await rawObservationHash(initialRuntime);
    const originalKnowledgeCount = (await initialRuntime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, initialHead.active_producer_version],
    ))[0].n;
    assert.ok(originalKnowledgeCount >= 1);
    await initialRuntime.dispose();
    initialRuntime = null;

    failedBuildRuntime = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryExtractor: "openai_compatible",
      memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
      memoryModel: "rebuild-provider-test",
      memoryApiKey: "test-only-provider-key",
      memoryTestProviderMode: "reject",
      maxQueueRetries: 0,
      bindings: { MEMORY_PROJECTION_GENERATION: "2", MEMORY_MAX_ATTEMPTS: "2" },
    });
    await exec(failedBuildRuntime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    const previousDispatchCount = await failedBuildRuntime.queueDispatchCount();
    await failedBuildRuntime.runScheduled();
    await waitForCompletedDispatch(failedBuildRuntime, previousDispatchCount);
    await failedBuildRuntime.waitFor(async () => {
      const rows = await failedBuildRuntime.querySql(
        "SELECT status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? AND producer_version <> ? ORDER BY started_at DESC LIMIT 1",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, initialHead.active_producer_version],
      );
      return rows.length === 1 && rows[0].error_code === "provider_unavailable";
    }, { timeoutMs: 5000, intervalMs: 25 });
    const afterFailedBuild = (await failedBuildRuntime.querySql(
      "SELECT active_generation, active_producer_version, requested_generation FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(afterFailedBuild.active_generation, 1);
    assert.equal(afterFailedBuild.active_producer_version, initialHead.active_producer_version);
    assert.equal(afterFailedBuild.requested_generation, 2);
    const duringFailureContext = await repositoryContext(failedBuildRuntime);
    assert.equal(duringFailureContext.memory.active_generation, 1);
    assert.equal(duringFailureContext.memory.stale, true);
    assert.ok(duringFailureContext.constraints.some((item) => item.text === "Report output format must be JSON."));
    assert.equal(await rawObservationHash(failedBuildRuntime), originalHash);
    await failedBuildRuntime.dispose();
    failedBuildRuntime = null;

    rebuiltRuntime = await startMemoryRuntime({
      resourcePersistencePath: persistencePath,
      applyMigrations: false,
      memoryExtractor: "openai_compatible",
      memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
      memoryModel: "rebuild-provider-test",
      memoryApiKey: "test-only-provider-key",
      memoryTestProviderMode: "echo_allowed_repository_clauses",
      maxQueueRetries: 0,
      bindings: { MEMORY_PROJECTION_GENERATION: "3", MEMORY_MAX_ATTEMPTS: "2" },
    });
    await exec(rebuiltRuntime, `UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0
      WHERE owner_id = ? AND repository_key = ?`, [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    const rebuildDispatchCount = await rebuiltRuntime.queueDispatchCount();
    await rebuiltRuntime.runScheduled();
    await waitForCompletedDispatch(rebuiltRuntime, rebuildDispatchCount);
    await rebuiltRuntime.waitFor(async () => {
      const head = await rebuiltRuntime.querySql(
        "SELECT active_generation, active_producer_version FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return head.length === 1 && head[0].active_generation === 3;
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const finalHead = (await rebuiltRuntime.querySql(
      "SELECT active_generation, active_producer_version, requested_generation FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    ))[0];
    assert.equal(finalHead.active_generation, 3);
    assert.equal(finalHead.requested_generation, 3);
    assert.notEqual(finalHead.active_producer_version, initialHead.active_producer_version);
    assert.equal((await rebuiltRuntime.querySql(
      "SELECT COUNT(*) AS n FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, initialHead.active_producer_version],
    ))[0].n, originalKnowledgeCount, "the legacy valid projection remains retained");
    const finalContext = await repositoryContext(rebuiltRuntime);
    assert.equal(finalContext.memory.active_generation, 3);
    assert.equal(finalContext.memory.stale, false);
    assert.ok(finalContext.constraints.some((item) => item.text === "Report output format must be JSON."));
    assert.equal(await rawObservationHash(rebuiltRuntime), originalHash,
      "rebuilding projections must not edit source observations");
  } finally {
    if (initialRuntime) await initialRuntime.dispose();
    if (failedBuildRuntime) await failedBuildRuntime.dispose();
    if (rebuiltRuntime) await rebuiltRuntime.dispose();
    await fs.rm(persistencePath, { recursive: true, force: true });
  }
});
