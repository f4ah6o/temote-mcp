import { loadMemoryConfiguration } from "./config.js";
import { boundedInput, extractionInputBudget, extractKnowledge, MemoryError } from "./extractor.js";
import {
  isConstraintQuote,
  isDecisionQuote,
  isExplicitChangeDirective,
  explicitRepositoryPredecessors,
  semanticKeyFor,
} from "./safety.js";

const WORKER_ID = "temote-memory-v1";
const MAX_QUEUE_MESSAGES_PER_BATCH = 100;
const OUTBOX_SWEEP_LIMIT = 32;
const OUTBOX_REDELIVERY_SECONDS = 60;
const MAX_COMMIT_STATEMENTS = 512;
const SUPPORTS_PER_INSERT = 12;
const ERROR_CODES = new Set([
  "db_unavailable",
  "extractor_not_configured",
  "provider_not_configured",
  "provider_timeout",
  "provider_unavailable",
  "provider_rejected",
  "provider_configuration_invalid",
  "provider_invalid_response",
  "provider_output_too_large",
  "provider_envelope_too_large",
  "provider_incomplete_response",
  "invalid_output",
  "invalid_support",
  "invalid_scope",
  "invalid_verification_support",
  "invalid_supersession",
  "input_too_large",
  "producer_generation_stale",
  "producer_generation_conflict",
  "run_retry_exhausted",
  "projection_too_large",
  "commit_rejected",
  "worker_internal",
]);

const READ_HEAD = [
  "SELECT owner_id, repository_key, requested_generation, requested_producer_version,",
  "active_generation, active_producer_version, epoch, published_at",
  "FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ?",
].join(" ");

const READ_CHECKPOINT = [
  "SELECT owner_id, repository_key, worker_id, producer_version, last_cloud_seq,",
  "last_success_at, last_error_at, last_error_code, stale, lease_token, lease_until, fence, projection_epoch",
  "FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
].join(" ");

const READ_OBSERVATIONS = [
  "SELECT cloud_seq, owner_id, host_id, session_id, observation_id, source_revision,",
  "repository_key, workspace_id, COALESCE(task_id, (SELECT MIN(linked.task_id) FROM observations AS linked",
  "WHERE linked.owner_id = observations.owner_id AND linked.repository_key = observations.repository_key",
  "AND linked.host_id = observations.host_id AND linked.session_id = observations.session_id",
  "AND observations.operation_id IS NOT NULL AND linked.operation_id = observations.operation_id",
  "AND linked.task_id IS NOT NULL HAVING COUNT(DISTINCT linked.task_id) = 1)) AS task_id,",
  "execution_id, operation_id, kind, action,",
  "target_backend, content_kind, content_preview, content_digest, state_status, state_revision,",
  "evidence_refs, observed_at",
  "FROM observations WHERE owner_id = ? AND repository_key = ? AND cloud_seq > ?",
  "ORDER BY cloud_seq LIMIT ?",
].join(" ");

const CLAIM_CHECKPOINT = [
  "UPDATE memory_checkpoints SET lease_token = ?, lease_until = ?, fence = fence + 1,",
  "projection_epoch = ?",
  "WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
  "AND last_cloud_seq = ? AND projection_epoch = ?",
  "AND (lease_until IS NULL OR lease_until <= ?)",
  "AND EXISTS (SELECT 1 FROM memory_projection_heads AS head",
  "WHERE head.owner_id = memory_checkpoints.owner_id",
  "AND head.repository_key = memory_checkpoints.repository_key",
  "AND head.epoch = ? AND head.requested_producer_version = ?)",
].join(" ");

const INSERT_RUN = [
  "INSERT INTO memory_runs (run_id, owner_id, repository_key, producer_version, from_seq, to_seq,",
  "status, attempt_count, started_at, lease_token, fence, projection_epoch, input_count)",
  "VALUES (?, ?, ?, ?, ?, ?, 'running', 1, ?, ?, ?, ?, ?)",
  "ON CONFLICT(run_id) DO UPDATE SET status = 'running', attempt_count = memory_runs.attempt_count + 1,",
  "started_at = excluded.started_at, completed_at = NULL, error_code = NULL,",
  "lease_token = excluded.lease_token, fence = excluded.fence, projection_epoch = excluded.projection_epoch,",
  "input_count = excluded.input_count",
  "WHERE memory_runs.status <> 'completed' AND memory_runs.attempt_count < ?",
].join(" ");

const INSERT_CLAIM_GUARD = [
  "INSERT INTO memory_claim_guards (guard_id, owner_id, repository_key, worker_id, producer_version,",
  "run_id, from_seq, to_seq, fence, lease_token, projection_epoch, max_attempts)",
  "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
].join(" ");
const DELETE_CLAIM_GUARD = "DELETE FROM memory_claim_guards WHERE guard_id = ?";

const RECORD_CLAIM_FAILURE = [
  "INSERT INTO memory_runs (run_id, owner_id, repository_key, producer_version, from_seq, to_seq,",
  "status, attempt_count, started_at, error_code)",
  "SELECT ?, ?, ?, ?, ?, ?, CASE WHEN 1 >= ? THEN 'failed' ELSE 'pending' END, 1, ?, ?",
  "WHERE EXISTS (SELECT 1 FROM memory_checkpoints AS checkpoint",
  "JOIN memory_projection_heads AS head ON head.owner_id = checkpoint.owner_id",
  "AND head.repository_key = checkpoint.repository_key",
  "WHERE checkpoint.owner_id = ? AND checkpoint.repository_key = ?",
  "AND checkpoint.worker_id = ? AND checkpoint.producer_version = ?",
  "AND checkpoint.last_cloud_seq = ? AND checkpoint.projection_epoch = ?",
  "AND (checkpoint.lease_token IS NULL OR checkpoint.lease_until <= ?)",
  "AND head.epoch = ? AND head.requested_producer_version = ?)",
  "ON CONFLICT(run_id) DO UPDATE SET",
  "status = CASE WHEN memory_runs.attempt_count + 1 >= ? THEN 'failed' ELSE 'pending' END,",
  "attempt_count = memory_runs.attempt_count + 1, started_at = excluded.started_at,",
  "error_code = excluded.error_code, lease_token = NULL",
  "WHERE memory_runs.status <> 'completed' AND memory_runs.attempt_count < ?",
].join(" ");

const RECORD_CLAIM_CHECKPOINT_ERROR = [
  "UPDATE memory_checkpoints SET last_error_at = ?, last_error_code = ?, stale = 1",
  "WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
  "AND last_cloud_seq = ? AND projection_epoch = ?",
  "AND (lease_token IS NULL OR lease_until <= ?)",
  "AND EXISTS (SELECT 1 FROM memory_projection_heads AS head",
  "WHERE head.owner_id = memory_checkpoints.owner_id",
  "AND head.repository_key = memory_checkpoints.repository_key",
  "AND head.epoch = ? AND head.requested_producer_version = ?)",
].join(" ");

const READ_INCOMPLETE_RUN = [
  "SELECT * FROM memory_runs WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
  "AND from_seq = ? AND status <> 'completed' ORDER BY to_seq LIMIT 1",
].join(" ");

const READ_RUN = "SELECT * FROM memory_runs WHERE run_id = ?";
const MAX_SUPERSESSION_CANDIDATES = 64;

const INSERT_COMMIT_GUARD = [
  "INSERT INTO memory_commit_guards (guard_id, owner_id, repository_key, worker_id, producer_version,",
  "run_id, from_seq, to_seq, fence, lease_token, projection_epoch, publish_projection, created_at)",
  "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,",
  "CASE WHEN ? = 1 AND NOT EXISTS (SELECT 1 FROM observations",
  "WHERE owner_id = ? AND repository_key = ? AND cloud_seq > ?) THEN 1 ELSE 0 END, ?)",
].join(" ");

const UPDATE_CHECKPOINT_COMMIT = [
  "UPDATE memory_checkpoints SET last_cloud_seq = ?, last_success_at = ?, last_error_at = NULL,",
  "last_error_code = NULL, stale = 0, lease_token = NULL, lease_until = NULL",
  "WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
  "AND last_cloud_seq = ? AND lease_token = ?",
  "AND fence = ? AND projection_epoch = ?",
].join(" ");

const COMPLETE_RUN = [
  "UPDATE memory_runs SET status = 'completed', completed_at = ?, error_code = NULL, outcome = ?,",
  "lease_token = NULL WHERE run_id = ? AND status = 'running' AND lease_token = ? AND fence = ?",
].join(" ");

const DELETE_COMMIT_GUARD = "DELETE FROM memory_commit_guards WHERE guard_id = ?";

const READ_ACTIVE_STATUS = [
  "SELECT head.active_generation, head.active_producer_version, head.requested_generation,",
  "head.requested_producer_version, head.epoch, checkpoint.last_cloud_seq, checkpoint.projection_epoch, checkpoint.last_success_at,",
  "checkpoint.last_error_at, checkpoint.last_error_code, checkpoint.stale",
  "FROM memory_projection_heads AS head LEFT JOIN memory_checkpoints AS checkpoint",
  "ON checkpoint.owner_id = head.owner_id AND checkpoint.repository_key = head.repository_key",
  "AND checkpoint.worker_id = ? AND checkpoint.producer_version = head.active_producer_version",
  "WHERE head.owner_id = ? AND head.repository_key = ?",
].join(" ");

const READ_REQUESTED_STATUS = [
  "SELECT last_success_at, last_error_at, last_error_code, stale, last_cloud_seq, projection_epoch",
  "FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
].join(" ");

const READ_REPOSITORY_HEAD = [
  "SELECT COALESCE(MAX(cloud_seq), 0) AS latest_cloud_seq FROM observations",
  "WHERE owner_id = ? AND repository_key = ?",
].join(" ");

const READ_HAS_MORE = [
  "SELECT EXISTS(SELECT 1 FROM observations WHERE owner_id = ? AND repository_key = ?",
  "AND cloud_seq > ?) AS has_more",
].join(" ");

const READ_PENDING_COUNT = [
  "SELECT COUNT(*) AS pending_count FROM observations WHERE owner_id = ? AND repository_key = ?",
  "AND cloud_seq > ?",
].join(" ");

const READ_KNOWLEDGE_COUNT = [
  "SELECT COUNT(*) AS knowledge_count FROM knowledge_items",
  "WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
  "AND status IN ('supported', 'current')",
].join(" ");
const READ_SUPPORT_DETAILS = [
  "SELECT support.observation_cloud_seq, support.observation_id, support.support_role,",
  "observation.host_id, observation.session_id, observation.source_revision, observation.kind",
  "FROM knowledge_support AS support JOIN observations AS observation",
  "ON observation.owner_id = support.owner_id AND observation.repository_key = support.repository_key",
  "AND observation.cloud_seq = support.observation_cloud_seq AND observation.observation_id = support.observation_id",
  "WHERE support.owner_id = ? AND support.repository_key = ? AND support.knowledge_id = ?",
  "ORDER BY support.observation_cloud_seq, support.support_role LIMIT ?",
].join(" ");
const MAX_SUPPORTS_PER_KNOWLEDGE = 16;
const MAX_SUPERSESSION_SUPPORT_ROWS = 1024;

const READ_OUTBOX = [
  "SELECT owner_id, repository_key, through_cloud_seq, queued_at, attempt_count, next_attempt_at, last_error_code",
  "FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
].join(" ");

const UPDATE_OUTBOX_SENT = [
  "UPDATE memory_outbox SET queued_at = ?, last_error_code = NULL, next_attempt_at = 0,",
  "attempt_count = 0, updated_at = ?",
  "WHERE owner_id = ? AND repository_key = ? AND through_cloud_seq = ?",
].join(" ");

const UPDATE_OUTBOX_FAILED = [
  "UPDATE memory_outbox SET attempt_count = MIN(attempt_count + 1, 30),",
  "next_attempt_at = ?, last_error_code = 'queue_send_failed', updated_at = ?",
  "WHERE owner_id = ? AND repository_key = ? AND through_cloud_seq = ?",
].join(" ");

const SELECT_DUE_OUTBOX = [
  "SELECT outbox.owner_id, outbox.repository_key FROM memory_outbox AS outbox",
  "LEFT JOIN memory_projection_heads AS head ON head.owner_id = outbox.owner_id",
  "AND head.repository_key = outbox.repository_key",
  "LEFT JOIN memory_checkpoints AS checkpoint ON checkpoint.owner_id = outbox.owner_id",
  "AND checkpoint.repository_key = outbox.repository_key AND checkpoint.worker_id = ?",
  "AND checkpoint.producer_version = ?",
  "WHERE outbox.next_attempt_at <= ? AND (outbox.queued_at IS NULL",
  "OR head.requested_generation IS NOT ? OR head.requested_producer_version IS NOT ?",
  "OR COALESCE(checkpoint.last_cloud_seq, 0) < outbox.through_cloud_seq)",
  "AND NOT EXISTS (SELECT 1 FROM memory_runs AS run",
  "WHERE run.owner_id = outbox.owner_id AND run.repository_key = outbox.repository_key",
  "AND run.producer_version = ? AND run.from_seq = COALESCE(checkpoint.last_cloud_seq, 0)",
  "AND run.status <> 'completed' AND run.attempt_count >= ?)",
  "ORDER BY outbox.next_attempt_at, outbox.updated_at LIMIT ?",
].join(" ");

export async function consumeMemoryBatch(batch, env) {
  if (!isD1(env?.OBSERVATION_DB) || !Array.isArray(batch?.messages)) {
    return { processed: 0, completed: 0, retried: 0, state: "unavailable" };
  }
  const config = await loadMemoryConfiguration(env);
  if (!config.enabled || !config.configured) {
    ackAll(batch.messages);
    return { processed: 0, completed: 0, retried: 0, state: config.enabled ? "not_configured" : "disabled" };
  }

  const ownerId = trustedOwnerId(env);
  const grouped = groupWakeMessages(batch.messages, ownerId, env);
  let processed = 0;
  let completed = 0;
  let retried = 0;
  for (const overflow of grouped.slice(1)) {
    retryAll(overflow.messages, 5);
    retried += overflow.messages.length;
  }
  for (const group of grouped.slice(0, 1)) {
    processed += 1;
    let outcome;
    try {
      outcome = await consumeRepository(env.OBSERVATION_DB, group, config);
    } catch (error) {
      outcome = { state: "failed", errorCode: stableErrorCode(error), retryable: true };
    }
    if (outcome.state === "completed" || outcome.state === "empty" || outcome.state === "busy"
        || outcome.state === "stale" || outcome.state === "exhausted") {
      ackAll(group.messages);
      if (outcome.state === "completed" || outcome.state === "empty") completed += 1;
    } else if (outcome.retryable) {
      retryAll(group.messages, retryDelaySeconds(outcome.attempt ?? 1));
      retried += group.messages.length;
    } else {
      ackAll(group.messages);
    }
  }
  return { processed, completed, retried, state: "processed" };
}

async function consumeRepository(db, group, config) {
  const ownerId = trustedOwnerId(group.env);
  if (!ownerId || ownerId !== group.ownerId) return { state: "stale" };
  const requested = await ensureProducerRequest(db, ownerId, group.repositoryKey, config);
  if (requested.error) return { state: "stale", errorCode: requested.error };
  const checkpoint = await ensureCheckpoint(db, ownerId, group.repositoryKey, config, requested.head.epoch);
  const fromSeq = Number(checkpoint.last_cloud_seq);
  const priorRun = await first(db, READ_INCOMPLETE_RUN, [
    ownerId, group.repositoryKey, config.producerVersion, fromSeq,
  ]);
  if (priorRun && Number(priorRun.attempt_count) >= config.maxAttempts) {
    return { state: "exhausted", errorCode: priorRun.error_code };
  }

  const hasFixedRange = Boolean(priorRun);
  const fixedToSeq = hasFixedRange ? Number(priorRun.to_seq) : null;
  const observations = await all(db, READ_OBSERVATIONS, [
    ownerId,
    group.repositoryKey,
    fromSeq,
    hasFixedRange ? Math.min(config.batchSize + 1, Math.max(1, Number(priorRun.input_count))) : config.batchSize + 1,
  ]);
  const throughRange = hasFixedRange
    ? observations.filter((observation) => Number(observation.cloud_seq) <= fixedToSeq)
    : observations;
  const selected = hasFixedRange ? throughRange : observations.slice(0, config.batchSize);
  if (selected.length === 0 && (!hasFixedRange || fixedToSeq === fromSeq)) {
    if (requested.head.active_producer_version === config.producerVersion
        && requested.head.active_generation === config.generation) {
      return { state: "empty" };
    }
    return commitEmptyProjection(db, ownerId, group.repositoryKey, config, requested.head, checkpoint);
  }

  let bounded = { observations: [] };
  if (selected.length > 0) bounded = boundedInput(selected, extractionInputBudget(config));
  const inputToSeq = hasFixedRange ? fixedToSeq : Number(
    bounded.observations.at(-1)?.cloud_seq ?? selected[0]?.cloud_seq ?? fromSeq,
  );
  const pendingAfterSelection = await first(db, READ_HAS_MORE, [ownerId, group.repositoryKey, inputToSeq]);
  const hasMore = Number(pendingAfterSelection?.has_more ?? 0) === 1;
  const inputError = bounded.observations.length === 0
    ? "input_too_large"
    : hasFixedRange && Number(selected.at(-1)?.cloud_seq ?? fromSeq) !== fixedToSeq
      ? "invalid_support"
      : hasFixedRange && bounded.observations.length !== selected.length
        ? "input_too_large"
        : null;

  const effectiveRunId = priorRun?.run_id ?? await stableRunId(
    ownerId,
    group.repositoryKey,
    config.producerVersion,
    fromSeq,
    inputToSeq,
  );
  const retryRun = await first(db, READ_RUN, [effectiveRunId]);
  if (retryRun?.status === "completed") return { state: "empty" };
  if (retryRun && Number(retryRun.attempt_count) >= config.maxAttempts) {
    return { state: "exhausted", errorCode: retryRun.error_code };
  }

  const claim = await claimCheckpoint(
    db,
    ownerId,
    group.repositoryKey,
    config,
    requested.head,
    checkpoint,
    effectiveRunId,
    fromSeq,
    inputToSeq,
    bounded.observations.length,
  );
  if (!claim.ok) {
    return {
      state: claim.state,
      errorCode: claim.errorCode,
      retryable: claim.retryable ?? false,
    };
  }

  if (inputError) {
    const attempt = await finishFailedAttempt(db, {
      ownerId,
      repositoryKey: group.repositoryKey,
      config,
      runId: effectiveRunId,
      fromSeq,
      claim,
      errorCode: inputError,
    });
    return {
      state: "failed",
      errorCode: inputError,
      attempt,
      retryable: attempt < config.maxAttempts,
    };
  }

  try {
    const extracted = await extractKnowledge(bounded.observations, config, effectiveRunId);
    const policy = await prepareProjection(
      db,
      ownerId,
      group.repositoryKey,
      config,
      extracted.items,
      inputToSeq,
    );
    const publishProjection = !hasMore;
    await commitProjection(db, {
      ownerId,
      repositoryKey: group.repositoryKey,
      config,
      runId: effectiveRunId,
      fromSeq,
      toSeq: inputToSeq,
      inputCount: extracted.inputCount,
      outcome: policy.items.length > 0 ? "projected" : "empty",
      items: policy.items,
      claim,
      head: requested.head,
      publishProjection,
    });
    const stillPending = await first(db, READ_HAS_MORE, [ownerId, group.repositoryKey, inputToSeq]);
    if (Number(stillPending?.has_more ?? 0) === 1) {
      await db.prepare([
        "UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0, updated_at = ?",
        "WHERE owner_id = ? AND repository_key = ? AND through_cloud_seq >= ?",
      ].join(" ")).bind(
        new Date().toISOString(),
        ownerId,
        group.repositoryKey,
        inputToSeq,
      ).run();
      await wakeMemory(group.env, ownerId, group.repositoryKey);
    }
    return { state: policy.items.length > 0 ? "completed" : "empty" };
  } catch (error) {
    const errorCode = stableErrorCode(error);
    const attempt = await finishFailedAttempt(db, {
      ownerId,
      repositoryKey: group.repositoryKey,
      config,
      runId: effectiveRunId,
      fromSeq,
      claim,
      errorCode,
    });
    return {
      state: "failed",
      errorCode,
      attempt,
      retryable: attempt < config.maxAttempts,
    };
  }
}

async function ensureProducerRequest(db, ownerId, repositoryKey, config) {
  const results = await db.batch([
    db.prepare([
      "INSERT OR IGNORE INTO memory_projection_heads (owner_id, repository_key, requested_generation,",
      "requested_producer_version, epoch) VALUES (?, ?, ?, ?, 1)",
    ].join(" ")).bind(ownerId, repositoryKey, config.generation, config.producerVersion),
    db.prepare([
      "UPDATE memory_projection_heads SET requested_generation = ?, requested_producer_version = ?,",
      "epoch = epoch + 1 WHERE owner_id = ? AND repository_key = ? AND requested_generation < ?",
    ].join(" ")).bind(
      config.generation,
      config.producerVersion,
      ownerId,
      repositoryKey,
      config.generation,
    ),
    db.prepare(READ_HEAD).bind(ownerId, repositoryKey),
  ]);
  const head = results?.[2]?.results?.[0] ?? null;
  if (!head) return { error: "db_unavailable" };
  if (Number(head.requested_generation) > config.generation) return { error: "producer_generation_stale" };
  if (Number(head.requested_generation) === config.generation
      && head.requested_producer_version !== config.producerVersion) {
    return { error: "producer_generation_conflict" };
  }
  return { head };
}

async function ensureCheckpoint(db, ownerId, repositoryKey, config, epoch) {
  await db.prepare([
    "INSERT OR IGNORE INTO memory_checkpoints (owner_id, repository_key, worker_id, producer_version,",
    "last_cloud_seq, projection_epoch, stale) VALUES (?, ?, ?, ?, 0, ?, 1)",
  ].join(" ")).bind(ownerId, repositoryKey, WORKER_ID, config.producerVersion, epoch).run();
  const checkpoint = await first(db, READ_CHECKPOINT, [
    ownerId,
    repositoryKey,
    WORKER_ID,
    config.producerVersion,
  ]);
  if (!checkpoint) throw new MemoryError("db_unavailable");
  return checkpoint;
}

async function claimCheckpoint(
  db,
  ownerId,
  repositoryKey,
  config,
  head,
  checkpoint,
  runId,
  fromSeq,
  toSeq,
  inputCount,
) {
  const leaseToken = crypto.randomUUID();
  const nowSeconds = Math.floor(Date.now() / 1000);
  const leaseUntil = nowSeconds + Math.max(30, Math.ceil(config.timeoutMs / 1000) + 30);
  const projectionEpoch = Number(head.epoch);
  const fence = Number(checkpoint.fence ?? 0) + 1;
  const guardId = crypto.randomUUID();
  try {
    await db.batch([
      db.prepare(CLAIM_CHECKPOINT).bind(
        leaseToken,
        leaseUntil,
        projectionEpoch,
        ownerId,
        repositoryKey,
        WORKER_ID,
        config.producerVersion,
        fromSeq,
        projectionEpoch,
        nowSeconds,
        projectionEpoch,
        config.producerVersion,
      ),
      db.prepare(INSERT_CLAIM_GUARD).bind(
        guardId,
        ownerId,
        repositoryKey,
        WORKER_ID,
        config.producerVersion,
        runId,
        fromSeq,
        toSeq,
        fence,
        leaseToken,
        projectionEpoch,
        config.maxAttempts,
      ),
      db.prepare(INSERT_RUN).bind(
        runId,
        ownerId,
        repositoryKey,
        config.producerVersion,
        fromSeq,
        toSeq,
        new Date().toISOString(),
        leaseToken,
        fence,
        projectionEpoch,
        inputCount,
        config.maxAttempts,
      ),
      db.prepare(DELETE_CLAIM_GUARD).bind(guardId),
    ]);
  } catch {
    const current = await first(db, READ_CHECKPOINT, [ownerId, repositoryKey, WORKER_ID, config.producerVersion]);
    if (current?.lease_token && Number(current.lease_until) > nowSeconds) return { ok: false, state: "busy" };
    const currentHead = await first(db, READ_HEAD, [ownerId, repositoryKey]);
    if (!currentHead || Number(currentHead.epoch) !== projectionEpoch
        || currentHead.requested_producer_version !== config.producerVersion) {
      return { ok: false, state: "stale" };
    }
    const existingRun = await first(db, READ_RUN, [runId]);
    if (existingRun && Number(existingRun.attempt_count) >= config.maxAttempts) {
      return { ok: false, state: "exhausted" };
    }
    const attempt = await recordClaimFailure(db, {
      ownerId,
      repositoryKey,
      config,
      fromSeq,
      toSeq,
      projectionEpoch,
      nowSeconds,
      errorCode: "commit_rejected",
    });
    return {
      ok: false,
      state: "failed",
      errorCode: "commit_rejected",
      retryable: attempt < config.maxAttempts,
    };
  }
  const run = await first(db, READ_RUN, [runId]);
  if (run?.lease_token !== leaseToken || Number(run?.fence) !== fence) {
    const current = await first(db, READ_CHECKPOINT, [ownerId, repositoryKey, WORKER_ID, config.producerVersion]);
    if (current?.lease_token && Number(current.lease_until) > nowSeconds) {
      return { ok: false, state: "busy" };
    }
    const attempt = await recordClaimFailure(db, {
      ownerId,
      repositoryKey,
      config,
      fromSeq,
      toSeq,
      projectionEpoch,
      nowSeconds,
      errorCode: "commit_rejected",
    });
    return {
      ok: false,
      state: "failed",
      errorCode: "commit_rejected",
      retryable: attempt < config.maxAttempts,
    };
  }
  const attempt = Number(run?.attempt_count ?? 1);
  return {
    ok: true,
    leaseToken,
    leaseUntil,
    fence,
    projectionEpoch,
    attempt,
  };
}

async function commitProjection(db, data) {
  const {
    ownerId,
    repositoryKey,
    config,
    runId,
    fromSeq,
    toSeq,
    outcome,
    items,
    claim,
    head,
    publishProjection,
  } = data;
  const now = new Date().toISOString();
  const guardId = crypto.randomUUID();
  const supportRows = [];
  const statements = [db.prepare(INSERT_COMMIT_GUARD).bind(
    guardId,
    ownerId,
    repositoryKey,
    WORKER_ID,
    config.producerVersion,
    runId,
    fromSeq,
    toSeq,
    claim.fence,
    claim.leaseToken,
    claim.projectionEpoch,
    publishProjection ? 1 : 0,
    ownerId,
    repositoryKey,
    toSeq,
    now,
  )];

  for (const item of items) {
    if (!item.reuseKnowledgeId) {
      statements.push(db.prepare([
        "INSERT INTO knowledge_items (knowledge_id, owner_id, repository_key, scope_type, scope_id,",
        "kind, semantic_key, text, status, confidence, valid_from, producer, producer_version,",
        "produced_at, source_through_cloud_seq, support_incomplete)",
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, 'temote-memory', ?, ?, ?, ?)",
      ].join(" ")).bind(
        item.knowledgeId,
        ownerId,
        repositoryKey,
        item.scopeType,
        item.scopeId,
        item.kind,
        item.semanticKey,
        item.text,
        item.status,
        item.validFrom,
        config.producerVersion,
        now,
        toSeq,
        item.supportIncomplete ? 1 : 0,
      ));
    } else {
      statements.push(db.prepare([
        "UPDATE knowledge_items SET status = CASE WHEN status = 'current' THEN 'current' ELSE ? END,",
        "valid_from = COALESCE(valid_from, ?), support_incomplete = MAX(support_incomplete, ?)",
        "WHERE owner_id = ? AND repository_key = ?",
        "AND producer_version = ? AND knowledge_id = ? AND status IN ('candidate', 'supported', 'current')",
      ].join(" ")).bind(
        item.status,
        item.validFrom,
        item.supportIncomplete ? 1 : 0,
        ownerId,
        repositoryKey,
        config.producerVersion,
        item.knowledgeId,
      ));
    }
    for (const support of item.persistSupport ?? item.support) supportRows.push({ item, support });
    for (const oldKnowledgeId of item.supersedes) {
      statements.push(db.prepare([
        "UPDATE knowledge_items SET status = 'superseded', valid_until = ?",
        "WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
        "AND status IN ('supported', 'current') AND knowledge_id = ?",
      ].join(" ")).bind(
        now,
        ownerId,
        repositoryKey,
        config.producerVersion,
        oldKnowledgeId,
      ));
      statements.push(db.prepare([
        "INSERT INTO knowledge_supersession (owner_id, repository_key, new_knowledge_id, old_knowledge_id,",
        "relationship, created_at) VALUES (?, ?, ?, ?, 'explicit_change', ?)",
      ].join(" ")).bind(ownerId, repositoryKey, item.knowledgeId, oldKnowledgeId, now));
    }
  }

  for (let offset = 0; offset < supportRows.length; offset += SUPPORTS_PER_INSERT) {
    const chunk = supportRows.slice(offset, offset + SUPPORTS_PER_INSERT);
    const values = chunk.map(() => "(?, ?, ?, ?, ?, ?)").join(", ");
    statements.push(db.prepare([
      "INSERT INTO knowledge_support (owner_id, repository_key, knowledge_id, observation_cloud_seq,",
      "observation_id, support_role) VALUES",
      values,
      "ON CONFLICT(owner_id, repository_key, knowledge_id, observation_cloud_seq, support_role) DO NOTHING",
    ].join(" ")).bind(...chunk.flatMap(({ item, support }) => [
      ownerId,
      repositoryKey,
      item.knowledgeId,
      support.cloudSeq,
      support.observationId,
      support.role ?? "direct_quote",
    ])));
  }

  if (publishProjection) {
    statements.push(db.prepare([
      "UPDATE memory_projection_heads SET active_generation = requested_generation,",
      "active_producer_version = requested_producer_version, published_at = ?",
      "WHERE owner_id = ? AND repository_key = ? AND epoch = ?",
      "AND requested_generation = ? AND requested_producer_version = ?",
      "AND EXISTS (SELECT 1 FROM memory_commit_guards AS guard",
      "WHERE guard.guard_id = ? AND guard.publish_projection = 1)",
    ].join(" ")).bind(
      now,
      ownerId,
      repositoryKey,
      claim.projectionEpoch,
      config.generation,
      config.producerVersion,
      guardId,
    ));
  }

  statements.push(db.prepare(UPDATE_CHECKPOINT_COMMIT).bind(
    toSeq,
    now,
    ownerId,
    repositoryKey,
    WORKER_ID,
    config.producerVersion,
    fromSeq,
    claim.leaseToken,
    claim.fence,
    claim.projectionEpoch,
  ));
  statements.push(db.prepare(COMPLETE_RUN).bind(
    now,
    outcome,
    runId,
    claim.leaseToken,
    claim.fence,
  ));
  statements.push(db.prepare(DELETE_COMMIT_GUARD).bind(guardId));
  if (statements.length > MAX_COMMIT_STATEMENTS) {
    throw new MemoryError("projection_too_large");
  }
  try {
    await db.batch(statements);
  } catch {
    throw new MemoryError("commit_rejected");
  }
}

async function prepareProjection(db, ownerId, repositoryKey, config, extractedItems, toSeq) {
  const items = [];
  const unique = new Map();
  const overlays = new Map();
  const sorted = [...extractedItems].sort((left, right) =>
    Number(left.support[0]?.cloud_seq ?? 0) - Number(right.support[0]?.cloud_seq ?? 0));
  for (let index = 0; index < sorted.length; index += 1) {
    const extractedInput = sorted[index];
    const extracted = {
      ...extractedInput,
      semanticKey: extractedInput.internalSummary
        ? extractedInput.semanticKey
        : semanticKeyFor(extractedInput.kind, extractedInput.text),
    };
    const semanticIdentity = [
      extracted.scopeType,
      extracted.scopeId,
      extracted.kind,
      extracted.semanticKey,
      extracted.text,
    ].join("\u0000");
    const prior = unique.get(semanticIdentity);
    if (prior) {
      const additions = extracted.support.map(supportToStored).filter((support) =>
        !prior.support.some((existing) => existing.cloudSeq === support.cloudSeq
          && existing.role === support.role));
      const supportAvailable = Math.max(0, MAX_SUPPORTS_PER_KNOWLEDGE - prior.support.length);
      if (additions.length > supportAvailable) prior.supportIncomplete = true;
      prior.support.push(...additions.slice(0, supportAvailable));
      const persisted = extracted.support.map(supportToStored).filter((support) =>
        !prior.persistSupport.some((existing) => existing.cloudSeq === support.cloudSeq
          && existing.role === support.role)
        && !prior.persistedSupport.some((existing) => existing.cloudSeq === support.cloudSeq
          && existing.role === support.role));
      const persistAvailable = Math.max(0, prior.supportCapacity - prior.persistSupport.length);
      if (persisted.length > persistAvailable) prior.supportIncomplete = true;
      const persistedAccepted = persisted.slice(0, persistAvailable);
      prior.persistSupport.push(...persistedAccepted);
      prior.persistedSupport.push(...persistedAccepted);
      continue;
    }

    const keyValues = [
      ownerId,
      repositoryKey,
      config.producerVersion,
      extracted.scopeType,
      extracted.scopeId,
      extracted.kind,
      extracted.semanticKey,
    ];
    const activeRows = await all(db, [
      "SELECT knowledge_id, text, status, support_incomplete FROM knowledge_items",
      "WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
      "AND scope_type = ? AND scope_id = ? AND kind = ? AND semantic_key = ?",
      "AND status IN ('supported', 'current') ORDER BY status DESC, produced_at DESC LIMIT ?",
    ].join(" "), [...keyValues, MAX_SUPERSESSION_CANDIDATES + 1]);
    const tooManyConflicts = activeRows.length > MAX_SUPERSESSION_CANDIDATES;
    const matching = activeRows.find((existing) => existing.text === extracted.text) ?? null;
    const dbConflicts = activeRows.filter((existing) => existing.text !== extracted.text);
    const overlayKey = [
      extracted.scopeType,
      extracted.scopeId,
      extracted.kind,
      extracted.semanticKey,
    ].join("\u0000");
    const overlay = overlays.get(overlayKey) ?? [];
    const stagedMatching = overlay.find((existing) => existing.status !== "superseded"
      && existing.text === extracted.text);
    const stagedConflicts = overlay.filter((existing) => existing.status !== "superseded"
      && existing.text !== extracted.text);
    const policySemanticKey = (support) => extracted.kind === "summary"
      ? semanticKeyFor("constraint", support.quote)
      : extracted.semanticKey;
    const changeSources = extracted.support.filter((support) =>
      support.source.kind === "instruction"
      && isExplicitChangeDirective(support.source, support.quote, policySemanticKey(support))
    );
    let status = promotedStatus(extracted);
    let supersedes = [];
    const conflicts = [...dbConflicts, ...stagedConflicts];
    let authorizedConflicts = [];
    if (changeSources.length > 0 && conflicts.length > 0
        && !tooManyConflicts && conflicts.length <= MAX_SUPERSESSION_CANDIDATES) {
      const authorization = await authorizeSupersession(
        db,
        ownerId,
        repositoryKey,
        conflicts,
        changeSources.map((support) => support.source),
      );
      if (authorization === "invalid_support") throw new MemoryError("invalid_supersession");
      if (authorization) {
        authorizedConflicts = conflicts;
      }
    }
    if (conflicts.length > 0) {
      if (authorizedConflicts.length === conflicts.length) {
        supersedes = authorizedConflicts.map((existing) => existing.knowledge_id ?? existing.knowledgeId);
        status = promotedStatus(extracted);
        for (const oldItem of stagedConflicts) oldItem.status = "superseded";
      } else {
        status = "supported";
      }
    } else if (matching || stagedMatching) {
      const existing = matching ?? stagedMatching;
      status = existing.status === "current" ? "current" : status;
    }

    const existingMatchingId = matching?.knowledge_id ?? stagedMatching?.knowledgeId ?? null;
    const existingSupportRows = matching
      ? await all(db, READ_SUPPORT_DETAILS, [ownerId, repositoryKey, matching.knowledge_id, MAX_SUPPORTS_PER_KNOWLEDGE + 1])
      : [];
    const existingSupports = existingSupportRows.slice(0, MAX_SUPPORTS_PER_KNOWLEDGE).map((row) => ({
      cloudSeq: Number(row.observation_cloud_seq),
      observationId: row.observation_id,
      role: row.support_role,
      hostId: row.host_id,
      sessionId: row.session_id,
      sourceRevision: row.source_revision == null ? null : Number(row.source_revision),
      sourceKind: row.kind,
    }));
    const stagedSupports = matching ? [] : stagedMatching?.support ?? [];
    const priorSupports = matching ? existingSupports : stagedSupports;
    const priorSupportKeys = new Set(priorSupports.map((support) => `${support.cloudSeq}\u0000${support.role}`));
    const incomingSupports = extracted.support.map(supportToStored);
    const incomingUnique = incomingSupports.filter((support, supportIndex) => {
      const key = `${support.cloudSeq}\u0000${support.role}`;
      return !priorSupportKeys.has(key)
        && incomingSupports.findIndex((candidate) => candidate.cloudSeq === support.cloudSeq
          && candidate.role === support.role) === supportIndex;
    });
    const existingSupportCount = matching ? existingSupportRows.length : priorSupports.length;
    const supportCapacity = Math.max(0, MAX_SUPPORTS_PER_KNOWLEDGE - Math.min(MAX_SUPPORTS_PER_KNOWLEDGE, existingSupportCount));
    const persistSupport = incomingUnique.slice(0, supportCapacity);
    const support = [...priorSupports, ...incomingUnique].slice(0, MAX_SUPPORTS_PER_KNOWLEDGE);
    const supportIncomplete = Boolean(matching?.support_incomplete ?? stagedMatching?.supportIncomplete)
      || existingSupportRows.length > MAX_SUPPORTS_PER_KNOWLEDGE
      || priorSupports.length + incomingUnique.length > MAX_SUPPORTS_PER_KNOWLEDGE
      || persistSupport.length < incomingUnique.length;
    const knowledgeId = existingMatchingId ?? await deterministicId([
      ownerId,
      repositoryKey,
      config.producerVersion,
      extracted.scopeType,
      extracted.scopeId,
      extracted.kind,
      extracted.semanticKey,
      extracted.text,
      String(extracted.support[0].cloud_seq),
    ]);
    const item = {
      knowledgeId,
      reuseKnowledgeId: Boolean(matching || stagedMatching),
      scopeType: extracted.scopeType,
      scopeId: extracted.scopeId,
      kind: extracted.kind,
      semanticKey: extracted.semanticKey,
      text: extracted.text,
      status,
      validFrom: extracted.support[0].source.observed_at,
      support,
      persistedSupport: [...priorSupports, ...persistSupport],
      persistSupport,
      supportCapacity,
      supportIncomplete,
      supersedes,
    };
    unique.set(semanticIdentity, item);
    items.push(item);
    if (status !== "superseded") {
      const nextOverlay = authorizedConflicts.length > 0
        ? [item]
        : [...overlay.filter((existing) => existing.status !== "superseded"), item];
      overlays.set(overlayKey, nextOverlay);
    }
    if (extracted.kind === "constraint" && extracted.scopeType === "repository"
        && status === "current") {
      sorted.splice(index + 1, 0, {
        kind: "summary",
        semanticKey: "summary:" + semanticKeyFor("constraint", extracted.text).slice("constraint:".length),
        internalSummary: true,
        text: extracted.text,
        scopeType: "repository",
        scopeId: extracted.scopeId,
        support: [extracted.support[0]],
      });
    }
  }
  return { items, toSeq };
}

async function authorizeSupersession(db, ownerId, repositoryKey, conflicts, changeSources) {
  const explicitIds = new Set(conflicts.filter((item) => changeSources.some((source) =>
    explicitRepositoryPredecessors(source).includes(item.text)))
  .map((item) => item.knowledge_id ?? item.knowledgeId));
  const dbRows = conflicts.filter((item) => item.knowledge_id && !explicitIds.has(item.knowledge_id));
  let supportByKnowledge = new Map();
  if (dbRows.length > 0) {
    const placeholders = dbRows.map(() => "?").join(", ");
    const provenance = await all(db, [
      "SELECT support.knowledge_id, observation.host_id, observation.session_id,",
      "observation.source_revision, observation.kind FROM knowledge_support AS support",
      "JOIN observations AS observation ON observation.owner_id = support.owner_id",
      "AND observation.repository_key = support.repository_key",
      "AND observation.cloud_seq = support.observation_cloud_seq",
      "AND observation.observation_id = support.observation_id",
      `WHERE support.owner_id = ? AND support.repository_key = ? AND support.knowledge_id IN (${placeholders})`,
      "AND support.support_role IN ('direct_quote', 'summary_quote')",
      "LIMIT ?",
    ].join(" "), [ownerId, repositoryKey, ...dbRows.map((item) => item.knowledge_id), MAX_SUPERSESSION_SUPPORT_ROWS + 1]);
    if (provenance.length > MAX_SUPERSESSION_SUPPORT_ROWS) return "invalid_support";
    supportByKnowledge = new Map();
    for (const item of provenance) {
      const list = supportByKnowledge.get(item.knowledge_id) ?? [];
      list.push(item);
      supportByKnowledge.set(item.knowledge_id, list);
    }
  }
  for (const prior of conflicts) {
    const oldId = prior.knowledge_id ?? prior.knowledgeId;
    if (explicitIds.has(oldId)) continue;
    if (prior.support_incomplete === 1 || prior.support_incomplete === true || prior.supportIncomplete) return false;
    const oldSupports = prior.knowledge_id
      ? supportByKnowledge.get(oldId) ?? []
      : prior.support.map((support) => ({
        host_id: support.hostId,
        session_id: support.sessionId,
        source_revision: support.sourceRevision,
        kind: support.sourceKind,
      }));
    if (oldSupports.length === 0) return "invalid_support";
    const sameSourceOrdered = oldSupports.every((oldSupport) => oldSupport.kind === "instruction"
      && changeSources.every((source) => source.host_id === oldSupport.host_id
        && source.session_id === oldSupport.session_id
        && Number(source.source_revision) > Number(oldSupport.source_revision)));
    if (!sameSourceOrdered) return false;
  }
  return true;
}

function supportToStored(support) {
  return {
    cloudSeq: support.cloud_seq,
    observationId: support.observation_id,
    quote: support.quote,
    role: support.role ?? "direct_quote",
    hostId: support.source?.host_id ?? null,
    sessionId: support.source?.session_id ?? null,
    sourceRevision: support.source?.source_revision == null ? null : Number(support.source.source_revision),
    sourceKind: support.source?.kind ?? null,
  };
}

function promotedStatus(item) {
  const hasDirectSupport = item.support.length > 0;
  if (!hasDirectSupport) return "candidate";
  if (item.kind === "constraint"
      && item.support.every((support) => support.source.kind === "instruction"
        && (isConstraintQuote(support.source, support.quote)
          || support.source.repository_key && isExplicitChangeDirective(
            support.source,
            support.quote,
            semanticKeyFor("constraint", support.quote),
          )))) {
    return item.scopeType === "repository" ? "current" : "supported";
  }
  if (item.kind === "decision"
      && item.support.every((support) => support.source.kind === "instruction"
        && isDecisionQuote(support.source, support.quote))) {
    return "current";
  }
  if (item.kind === "summary" && item.support.every((support) =>
    support.source.content_kind === "text" || support.source.content_kind === "view"
  )) return "supported";
  return "supported";
}

function withSupportedSummary(items) {
  const policies = items.filter((item) => item.kind === "constraint"
    && item.scopeType === "repository" && item.support?.[0]);
  if (policies.length === 0) return items;
  return [...items, ...policies.map((policy) => ({
    kind: "summary",
    semanticKey: "summary:" + semanticKeyFor("constraint", policy.text).slice("constraint:".length),
    internalSummary: true,
    text: policy.text,
    scopeType: "repository",
    scopeId: policy.scopeId,
    support: [{ ...policy.support[0], role: "summary_quote" }],
    supersedes: [],
    verificationPath: null,
  }))];
}

async function commitEmptyProjection(db, ownerId, repositoryKey, config, head, checkpoint) {
  const fromSeq = Number(checkpoint.last_cloud_seq);
  const runId = await stableRunId(ownerId, repositoryKey, config.producerVersion, fromSeq, fromSeq);
  const existing = await first(db, READ_RUN, [runId]);
  if (existing?.status === "completed") return { state: "empty" };
  if (existing && Number(existing.attempt_count) >= config.maxAttempts) {
    return { state: "exhausted" };
  }
  const claim = await claimCheckpoint(
    db, ownerId, repositoryKey, config, head, checkpoint, runId, fromSeq, fromSeq, 0,
  );
  if (!claim.ok) {
    return {
      state: claim.state,
      errorCode: claim.errorCode,
      retryable: claim.retryable ?? false,
    };
  }
  try {
    await commitProjection(db, {
      ownerId,
      repositoryKey,
      config,
      runId,
      fromSeq,
      toSeq: fromSeq,
      inputCount: 0,
      outcome: "empty",
      items: [],
      supersessions: [],
      claim,
      head,
      publishProjection: true,
    });
    return { state: "empty" };
  } catch (error) {
    const errorCode = stableErrorCode(error);
    const attempt = await finishFailedAttempt(db, {
      ownerId,
      repositoryKey,
      config,
      runId,
      fromSeq,
      claim,
      errorCode,
    });
    return { state: "failed", errorCode, attempt, retryable: attempt < config.maxAttempts };
  }
}

async function recordClaimFailure(db, details) {
  const {
    ownerId, repositoryKey, config, fromSeq, projectionEpoch, nowSeconds, errorCode,
  } = details;
  const now = new Date().toISOString();
  const run = await stableRunId(ownerId, repositoryKey, config.producerVersion, fromSeq, details.toSeq);
  await db.batch([
    db.prepare(RECORD_CLAIM_FAILURE).bind(
      run,
      ownerId,
      repositoryKey,
      config.producerVersion,
      fromSeq,
      details.toSeq,
      config.maxAttempts,
      now,
      errorCode,
      ownerId,
      repositoryKey,
      WORKER_ID,
      config.producerVersion,
      fromSeq,
      projectionEpoch,
      nowSeconds,
      projectionEpoch,
      config.producerVersion,
      config.maxAttempts,
      config.maxAttempts,
    ),
    db.prepare(RECORD_CLAIM_CHECKPOINT_ERROR).bind(
      now,
      errorCode,
      ownerId,
      repositoryKey,
      WORKER_ID,
      config.producerVersion,
      fromSeq,
      projectionEpoch,
      nowSeconds,
      projectionEpoch,
      config.producerVersion,
    ),
  ]);
  const saved = await first(db, READ_RUN, [run]);
  return Number(saved?.attempt_count ?? config.maxAttempts);
}

async function finishFailedAttempt(db, details) {
  const {
    ownerId, repositoryKey, config, runId, fromSeq, claim, errorCode,
  } = details;
  const now = new Date().toISOString();
  const attempts = await first(db, READ_RUN, [runId]);
  const attemptCount = Number(attempts?.attempt_count ?? claim.attempt ?? 1);
  const status = attemptCount >= config.maxAttempts ? "failed" : "pending";
  const stable = ERROR_CODES.has(errorCode) ? errorCode : "worker_internal";
  await db.batch([
    db.prepare([
      "UPDATE memory_runs SET status = ?, error_code = ?, lease_token = NULL",
      "WHERE run_id = ? AND lease_token = ? AND fence = ? AND from_seq = ?",
    ].join(" ")).bind(status, stable, runId, claim.leaseToken, claim.fence, fromSeq),
    db.prepare([
      "UPDATE memory_checkpoints SET last_error_at = ?, last_error_code = ?, stale = 1,",
      "lease_token = NULL, lease_until = NULL",
      "WHERE owner_id = ? AND repository_key = ? AND worker_id = ? AND producer_version = ?",
      "AND lease_token = ? AND fence = ? AND last_cloud_seq = ?",
    ].join(" ")).bind(
      now,
      stable,
      ownerId,
      repositoryKey,
      WORKER_ID,
      config.producerVersion,
      claim.leaseToken,
      claim.fence,
      fromSeq,
    ),
  ]);
  return attemptCount;
}

export async function wakeMemory(env, ownerId, repositoryKey) {
  if (!isD1(env?.OBSERVATION_DB) || !validNamespace(ownerId, repositoryKey)) return false;
  const config = await loadMemoryConfiguration(env);
  if (!config.enabled || !config.configured || !env?.MEMORY_QUEUE
      || typeof env.MEMORY_QUEUE.send !== "function") return false;
  const outbox = await first(env.OBSERVATION_DB, READ_OUTBOX, [ownerId, repositoryKey]);
  if (!outbox) return false;
  const nowSeconds = Math.floor(Date.now() / 1000);
  if (Number(outbox.next_attempt_at) > nowSeconds) return false;
  const queuedAt = Date.parse(outbox.queued_at ?? "");
  if (Number.isFinite(queuedAt)
      && Math.floor(queuedAt / 1000) > nowSeconds - OUTBOX_REDELIVERY_SECONDS) return false;
  const message = {
    owner_id: ownerId,
    repository_key: repositoryKey,
    through_cloud_seq: Number(outbox.through_cloud_seq),
  };
  try {
    await env.MEMORY_QUEUE.send(message);
    const now = new Date().toISOString();
    await env.OBSERVATION_DB.prepare(UPDATE_OUTBOX_SENT).bind(
      now,
      now,
      ownerId,
      repositoryKey,
      Number(outbox.through_cloud_seq),
    ).run();
    return true;
  } catch {
    const attempts = Math.min(Number(outbox.attempt_count ?? 0) + 1, 30);
    const delay = Math.min(60 * (2 ** Math.min(attempts, 6)), 3600);
    const now = new Date().toISOString();
    await env.OBSERVATION_DB.prepare(UPDATE_OUTBOX_FAILED).bind(
      nowSeconds + delay,
      now,
      ownerId,
      repositoryKey,
      Number(outbox.through_cloud_seq),
    ).run().catch(() => {});
    return false;
  }
}

export async function sweepMemoryOutbox(env) {
  if (!isD1(env?.OBSERVATION_DB)) return { attempted: 0, queued: 0, state: "unavailable" };
  const config = await loadMemoryConfiguration(env);
  if (!config.enabled || !config.configured || typeof env?.MEMORY_QUEUE?.send !== "function") {
    return { attempted: 0, queued: 0, state: config.enabled ? "not_configured" : "disabled" };
  }
  const nowSeconds = Math.floor(Date.now() / 1000);
  const due = await all(env.OBSERVATION_DB, SELECT_DUE_OUTBOX, [
    WORKER_ID,
    config.producerVersion,
    nowSeconds,
    config.generation,
    config.producerVersion,
    config.producerVersion,
    config.maxAttempts,
    OUTBOX_SWEEP_LIMIT,
  ]);
  let queued = 0;
  for (const item of due) {
    if (await wakeMemory(env, item.owner_id, item.repository_key)) queued += 1;
  }
  return { attempted: due.length, queued, state: "swept" };
}

export async function memoryWorkerStatus(env, ownerId, repositoryKey) {
  const config = await loadMemoryConfiguration(env);
  const queueConfigured = typeof env?.MEMORY_QUEUE?.send === "function";
  const workerConfigured = config.configured && queueConfigured;
  const base = {
    state: !config.enabled ? "disabled" : !workerConfigured ? "not_configured" : "ready_empty",
    enabled: config.enabled,
    extractor: config.extractor,
    producerVersion: config.producerVersion,
    activeProducerVersion: null,
    activeGeneration: null,
    requestedGeneration: null,
    requestedProducerVersion: null,
    workerLastCloudSeq: null,
    latestCloudSeq: 0,
    workerLag: 0,
    pendingObservationCount: 0,
    lastSuccessAt: null,
    lastErrorAt: null,
    lastErrorCode: config.errorCode ?? (config.enabled && !queueConfigured ? "queue_not_configured" : null),
    stale: !workerConfigured,
  };
  if (!isD1(env?.OBSERVATION_DB) || !validNamespace(ownerId, repositoryKey)) {
    return { ...base, state: workerConfigured ? "failed" : base.state, lastErrorCode: "db_unavailable", stale: true };
  }
  try {
    const [head, latest] = await Promise.all([
      first(env.OBSERVATION_DB, READ_ACTIVE_STATUS, [WORKER_ID, ownerId, repositoryKey]),
      first(env.OBSERVATION_DB, READ_REPOSITORY_HEAD, [ownerId, repositoryKey]),
    ]);
    const latestCloudSeq = Number(latest?.latest_cloud_seq ?? 0);
    const activeProducerVersion = head?.active_producer_version ?? null;
    const requestedProducerVersion = head?.requested_producer_version ?? config.producerVersion;
    const checkpoint = activeProducerVersion ? head : null;
    const requestedCheckpoint = requestedProducerVersion
      ? await first(env.OBSERVATION_DB, READ_REQUESTED_STATUS, [
        ownerId, repositoryKey, WORKER_ID, requestedProducerVersion,
      ])
      : null;
    const requestedLastSeq = requestedCheckpoint?.last_cloud_seq == null
      ? 0
      : Number(requestedCheckpoint.last_cloud_seq);
    const pending = await first(env.OBSERVATION_DB, READ_PENDING_COUNT, [ownerId, repositoryKey, requestedLastSeq]);
    const pendingObservationCount = Number(pending?.pending_count ?? 0);
    const activeKnowledge = activeProducerVersion
      ? await first(env.OBSERVATION_DB, READ_KNOWLEDGE_COUNT, [ownerId, repositoryKey, activeProducerVersion])
      : null;
    const requestedGeneration = head?.requested_generation == null ? null : Number(head.requested_generation);
    const activeGeneration = head?.active_generation == null ? null : Number(head.active_generation);
    const generationMismatch = requestedGeneration !== null && requestedGeneration !== config.generation;
    const producerConflict = requestedGeneration === config.generation && config.configured
      && requestedProducerVersion !== config.producerVersion;
    const activeEpochStale = Boolean(checkpoint && Number(checkpoint.projection_epoch) !== Number(head.epoch));
    let state = base.state;
    let diagnostic = config.errorCode;
    if (!workerConfigured) state = config.enabled ? "not_configured" : "disabled";
    else if (producerConflict) {
      state = "failed";
      diagnostic = "producer_generation_conflict";
    } else if (requestedGeneration !== null && requestedGeneration > config.generation) {
      state = "failed";
      diagnostic = "producer_generation_stale";
    } else if (requestedCheckpoint?.last_error_code || head?.last_error_code) {
      state = "failed";
      diagnostic = requestedCheckpoint?.last_error_code ?? head?.last_error_code;
    } else if (pendingObservationCount > 0 || activeProducerVersion !== requestedProducerVersion
        || generationMismatch || activeEpochStale) state = "lagging";
    else if (Number(activeKnowledge?.knowledge_count ?? 0) === 0) state = "ready_empty";
    else state = "ready";
    return {
      ...base,
      state,
      activeProducerVersion,
      activeGeneration,
      requestedGeneration,
      requestedProducerVersion,
      workerLastCloudSeq: requestedCheckpoint?.last_cloud_seq == null ? null : requestedLastSeq,
      latestCloudSeq,
      workerLag: pendingObservationCount,
      pendingObservationCount,
      lastSuccessAt: requestedCheckpoint?.last_success_at ?? checkpoint?.last_success_at ?? null,
      lastErrorAt: requestedCheckpoint?.last_error_at ?? checkpoint?.last_error_at ?? null,
      lastErrorCode: diagnostic ?? requestedCheckpoint?.last_error_code ?? checkpoint?.last_error_code ?? null,
      stale: !workerConfigured || state === "failed" || state === "lagging"
        || activeEpochStale || Boolean(checkpoint && Number(checkpoint.stale) === 1),
    };
  } catch {
    return { ...base, state: "failed", lastErrorCode: "db_unavailable", stale: true };
  }
}

async function stableRunId(ownerId, repositoryKey, producerVersion, fromSeq, toSeq) {
  const material = JSON.stringify([ownerId, repositoryKey, producerVersion, fromSeq, toSeq]);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(material)));
  const bytes = digest.slice(0, 16);
  bytes[6] = (bytes[6] & 0x0f) | 0x50;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20)].join("-");
}

async function deterministicId(parts) {
  const material = JSON.stringify(parts);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(material)));
  return "memory-" + Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function groupWakeMessages(messages, ownerId, env) {
  const groups = new Map();
  for (const message of messages.slice(0, MAX_QUEUE_MESSAGES_PER_BATCH)) {
    const body = message?.body;
    if (!isRecord(body) || Object.keys(body).sort().join(",") !== "owner_id,repository_key,through_cloud_seq"
        || body.owner_id !== ownerId || !validNamespace(body.owner_id, body.repository_key)
        || !Number.isSafeInteger(body.through_cloud_seq) || body.through_cloud_seq < 0) {
      message?.ack?.();
      continue;
    }
    const existing = groups.get(body.repository_key) ?? {
      ownerId,
      repositoryKey: body.repository_key,
      throughCloudSeq: 0,
      messages: [],
      env,
    };
    existing.throughCloudSeq = Math.max(existing.throughCloudSeq, body.through_cloud_seq);
    existing.messages.push(message);
    groups.set(body.repository_key, existing);
  }
  return [...groups.values()];
}

function ackAll(messages) {
  for (const message of messages) message?.ack?.();
}

function retryAll(messages, delaySeconds) {
  for (const message of messages) message?.retry?.({ delaySeconds });
}

function retryDelaySeconds(attempt) {
  return Math.min(5 * (2 ** Math.min(Math.max(0, attempt - 1), 6)), 300);
}

function stableErrorCode(error) {
  if (error instanceof MemoryError && ERROR_CODES.has(error.code)) return error.code;
  return "worker_internal";
}

function trustedOwnerId(env) {
  const value = env?.OBSERVATION_OWNER_ID;
  return typeof value === "string" && /^[A-Za-z0-9._:@/-]{1,256}$/.test(value) ? value : null;
}

function validNamespace(ownerId, repositoryKey) {
  return typeof ownerId === "string" && /^[A-Za-z0-9._:@/-]{1,256}$/.test(ownerId)
    && typeof repositoryKey === "string" && repositoryKey.length > 0 && repositoryKey.length <= 512
    && repositoryKey.trim() === repositoryKey;
}

function isD1(db) {
  return db && typeof db.prepare === "function" && typeof db.batch === "function";
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

async function all(db, sql, values = []) {
  const result = await db.prepare(sql).bind(...values).all();
  return result?.results ?? [];
}

async function first(db, sql, values = []) {
  if (typeof db.prepare(sql).bind(...values).first === "function") {
    return db.prepare(sql).bind(...values).first();
  }
  return (await all(db, sql, values))[0] ?? null;
}
