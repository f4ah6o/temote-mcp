import { validateHostId, validateSessionId } from "../protocol.js";
import { memoryWorkerStatus } from "../memory/index.js";

const CONTEXT_TOOLS = new Set(["context_resolve", "context_status"]);
const MAX_OWNER_ID = 256;
const MAX_REPOSITORY_KEY = 512;
const MAX_SCOPE_ID = 256;
const MAX_QUERY = 512;
const DEFAULT_LIMIT = 16;
const MAX_LIMIT = 64;
const DEFAULT_BUDGET_BYTES = 16 * 1024;
const MIN_BUDGET_BYTES = 1024;
const MAX_BUDGET_BYTES = 64 * 1024;
const MAX_SOURCE_ROWS = 128;
const MAX_OBSERVATION_ROWS = 2048;
// D1 permits at most 100 bound parameters per statement.  The support and
// supersession reads bind one id per current item, plus a small fixed set.
const MAX_KNOWLEDGE_ROWS = 64;
const MAX_SUPPORT_ROWS = 1024;
const MAX_SUPPORTS_PER_ITEM = 16;
const MAX_OPERATION_SCOPES = 80;
const MAX_REFS = 64;
const MAX_KNOWLEDGE_TEXT_BYTES = 8192;
const KINDS = new Set([
  "fact",
  "decision",
  "constraint",
  "observation",
  "failure_pattern",
  "unresolved",
  "summary",
]);
const KNOWLEDGE_STATUSES = new Set(["supported", "current"]);
const TERMINAL_TASK_STATES = new Set(["completed", "failed", "cancelled", "interrupted", "expired"]);
const NEEDS_ATTENTION_STATES = new Set([
  "failed",
  "cancelled",
  "interrupted",
  "expired",
  "dispatch_error",
  "waiting_input",
  "needs_attention",
  "unobserved",
]);

const READ_SESSION_SOURCES = [
  "SELECT owner_id, host_id, session_id, repository_key, source_base_revision,",
  "source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded,",
  "gap_count, last_synced_at FROM observation_sources",
  "WHERE owner_id = ? AND session_id = ?",
].join(" ");

const READ_REPOSITORY_SOURCES = [
  "SELECT owner_id, host_id, session_id, repository_key, source_base_revision,",
  "source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded,",
  "gap_count, last_synced_at FROM observation_sources",
  "WHERE owner_id = ? AND repository_key = ?",
  "ORDER BY host_id, session_id LIMIT ?",
].join(" ");

const READ_REPOSITORY_TOTALS = [
  "SELECT COALESCE(MAX(cloud_seq), 0) AS latest_cloud_seq, COUNT(*) AS observation_count",
  "FROM observations WHERE owner_id = ? AND repository_key = ?",
].join(" ");

const READ_SESSION_TOTALS = [
  "SELECT COALESCE(MAX(cloud_seq), 0) AS latest_cloud_seq, COUNT(*) AS observation_count",
  "FROM observations WHERE owner_id = ? AND host_id = ? AND session_id = ?",
].join(" ");

const READ_KNOWLEDGE = [
  "SELECT knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,",
  "semantic_key, text, status, confidence, valid_from, valid_until, producer,",
  "producer_version, produced_at, source_through_cloud_seq, support_incomplete FROM knowledge_items",
  "WHERE owner_id = ? AND repository_key = ? AND producer_version = ?",
  "AND status IN ('supported', 'current')",
  "AND ((scope_type = 'repository' AND scope_id = ?)",
  "OR (? IS NOT NULL AND scope_type = 'workspace' AND scope_id = ?)",
  "OR (? IS NOT NULL AND scope_type = 'task' AND scope_id = ?))",
  "AND (valid_from IS NULL OR valid_from <= ?)",
  "AND (valid_until IS NULL OR valid_until > ?)",
  "AND (? IS NULL OR instr(lower(text), lower(?)) > 0",
  "OR instr(lower(semantic_key), lower(?)) > 0)",
  "ORDER BY CASE status WHEN 'current' THEN 0 ELSE 1 END, kind,",
  "COALESCE(valid_from, produced_at) DESC, knowledge_id LIMIT ?",
].join(" ");

const READ_SUPPORTS_FOR_KNOWLEDGE = [
  "SELECT support.knowledge_id, support.observation_cloud_seq, support.observation_id,",
  "support.support_role, observation.host_id, observation.session_id,",
  "observation.source_revision, observation.task_id, observation.operation_id, observation.workspace_id,",
  "COALESCE(observation.task_id, (SELECT MIN(linked.task_id) FROM observations AS linked",
  "WHERE linked.owner_id = observation.owner_id AND linked.repository_key = observation.repository_key",
  "AND linked.host_id = observation.host_id AND linked.session_id = observation.session_id",
  "AND linked.operation_id = observation.operation_id AND linked.task_id IS NOT NULL",
  "HAVING COUNT(DISTINCT linked.task_id) = 1)) AS effective_task_id, observation.kind",
  "FROM knowledge_support AS support",
  "JOIN observations AS observation ON observation.owner_id = support.owner_id",
  "AND observation.repository_key = support.repository_key",
  "AND observation.cloud_seq = support.observation_cloud_seq",
  "AND observation.observation_id = support.observation_id",
  "WHERE support.owner_id = ? AND support.repository_key = ?",
  "AND support.knowledge_id IN (",
].join(" ");

const READ_SUPERSESSION_HISTORY = [
  "SELECT edge.new_knowledge_id, edge.old_knowledge_id, edge.relationship,",
  "old_item.owner_id, old_item.repository_key, old_item.scope_type, old_item.scope_id,",
  "old_item.kind, old_item.text, old_item.status, old_item.confidence, old_item.valid_from,",
  "old_item.valid_until, old_item.producer, old_item.producer_version, old_item.produced_at,",
  "old_item.source_through_cloud_seq, old_item.support_incomplete FROM knowledge_supersession AS edge",
  "JOIN knowledge_items AS current_item ON current_item.owner_id = edge.owner_id",
  "AND current_item.repository_key = edge.repository_key",
  "AND current_item.knowledge_id = edge.new_knowledge_id",
  "LEFT JOIN knowledge_items AS old_item ON old_item.owner_id = edge.owner_id",
  "AND old_item.repository_key = edge.repository_key",
  "AND old_item.knowledge_id = edge.old_knowledge_id",
  "WHERE edge.owner_id = ? AND edge.repository_key = ?",
  "AND edge.new_knowledge_id IN (",
].join(" ");

const READ_OBSERVATIONS = [
  "SELECT source.cloud_seq, source.host_id, source.session_id, source.observation_id, source.source_revision,",
  "source.repository_key, source.workspace_id, source.task_id, source.operation_id, source.kind, source.action,",
  "source.target_backend, source.content_kind, source.content_digest, source.state_status, source.state_revision,",
  "source.evidence_refs, source.observed_at, source.ingested_at FROM observations AS source",
].join(" ");

/**
 * Handle cloud context tools before host/session routing.
 *
 * A missing session mapping deliberately returns handled:false so the caller
 * can preserve its existing host fallback. A mapped but ambiguous or
 * mismatched session returns an error and must never fall through to routing.
 */
export async function resolveCloudContext(name, args, env) {
  if (!CONTEXT_TOOLS.has(name) || !env?.OBSERVATION_DB) return { handled: false };

  const checked = validateContextArgs(name, args);
  if (!checked.ok) {
    return contextError("invalid_params", -32602, checked.detail);
  }
  const ownerId = trustedOwnerId(env);
  if (!ownerId) {
    return contextError("observation_owner_unconfigured", -32001);
  }
  if (!isD1(env.OBSERVATION_DB)) {
    return contextError("observation_db_unavailable", -32001);
  }

  try {
    const target = await resolveTarget(env.OBSERVATION_DB, ownerId, checked.value);
    if (target.handled === false) return { handled: false };
    if (target.error) return target.error;

    const repositoryKey = target.repositoryKey;
    const sourceRead = repositoryKey
      ? await readRepositorySources(env.OBSERVATION_DB, ownerId, repositoryKey)
      : { rows: [target.sessionSource], truncated: false };
    const sources = sourceRead.rows;
    const totals = repositoryKey
      ? await firstRow(env.OBSERVATION_DB, READ_REPOSITORY_TOTALS, ownerId, repositoryKey)
      : await firstRow(
        env.OBSERVATION_DB,
        READ_SESSION_TOTALS,
        ownerId,
        target.sessionSource.host_id,
        target.sessionSource.session_id,
      );
    const freshness = sourceFreshness(
      sources,
      totals,
      checked.value,
      target.sessionSource ?? null,
      sourceRead.truncated,
    );
    const worker = repositoryKey
      ? await readWorkerStatus(env, ownerId, repositoryKey, freshness.latest_cloud_seq)
      : unconfiguredWorkerStatus(env, freshness.latest_cloud_seq);

    if (name === "context_status") {
      return {
        handled: true,
        value: {
          context_schema_version: 1,
          scope: scopeProjection(repositoryKey, checked.value),
          authority: { observations: "replicated_observed", knowledge: "derived" },
          freshness: { ...freshness, ...workerFreshness(worker) },
          memory: worker,
          partial: freshness.partial,
          deterministic: true,
        },
      };
    }

    const observationResult = await readObservationProjection(
      env.OBSERVATION_DB,
      ownerId,
      repositoryKey,
      target.sessionSource,
      checked.value,
    );
    const taskOperations = checked.value.taskId
      ? authorizedTaskOperations(observationResult.observations, checked.value.taskId)
      : [];
    const knowledgeResult = repositoryKey
      ? await readKnowledgeProjection(
        env.OBSERVATION_DB,
        ownerId,
        repositoryKey,
        checked.value,
        worker.active_producer_version,
        taskOperations,
      )
      : emptyKnowledgeProjection();
    const result = assembleContext({
      repositoryKey,
      filters: checked.value,
      freshness,
      worker,
      observationResult,
      knowledgeResult,
      sourceSource: target.sessionSource ?? null,
    });
    const bounded = fitBudget(result, checked.value.budgetBytes);
    if (!bounded) return contextError("context_budget_too_small", -32602);
    return { handled: true, value: bounded };
  } catch {
    // The error response is intentionally stable and contains no SQL or data.
    return contextError("context_database_unavailable", -32001);
  }
}

function validateContextArgs(name, args) {
  if (!isObject(args)) return { ok: false, detail: "arguments must be an object" };
  const allowed = name === "context_status"
    ? new Set(["repository", "host_id", "session_id"])
    : new Set([
      "repository", "host_id", "session_id", "task_id", "workspace_id", "query",
      "limit", "budget_bytes", "at_least_revision", "at_least_cloud_seq",
    ]);
  const unknown = Object.keys(args).find((key) => !allowed.has(key));
  if (unknown) return { ok: false, detail: "unknown context argument" };
  if (Object.hasOwn(args, "owner")) return { ok: false, detail: "owner is server-controlled" };

  if (Object.hasOwn(args, "repository") && !validBoundedString(args.repository, MAX_REPOSITORY_KEY)) {
    return { ok: false, detail: "invalid repository" };
  }
  if (Object.hasOwn(args, "host_id") && !validateHostId(args.host_id)) {
    return { ok: false, detail: "invalid host_id" };
  }
  if (Object.hasOwn(args, "session_id") && !validateSessionId(args.session_id)) {
    return { ok: false, detail: "invalid session_id" };
  }
  if (Object.hasOwn(args, "host_id") && !Object.hasOwn(args, "session_id")) {
    return { ok: false, detail: "host_id requires session_id" };
  }
  for (const key of ["task_id", "workspace_id"]) {
    if (Object.hasOwn(args, key) && !validBoundedString(args[key], MAX_SCOPE_ID)) {
      return { ok: false, detail: `invalid ${key}` };
    }
  }
  if (Object.hasOwn(args, "query") && !validBoundedString(args.query, MAX_QUERY)) {
    return { ok: false, detail: "invalid query" };
  }
  const limit = args.limit ?? DEFAULT_LIMIT;
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_LIMIT) {
    return { ok: false, detail: `limit must be in 1..=${MAX_LIMIT}` };
  }
  const budgetBytes = args.budget_bytes ?? DEFAULT_BUDGET_BYTES;
  if (!Number.isSafeInteger(budgetBytes)
      || budgetBytes < MIN_BUDGET_BYTES
      || budgetBytes > MAX_BUDGET_BYTES) {
    return { ok: false, detail: "budget_bytes is outside its supported range" };
  }
  for (const key of ["at_least_revision", "at_least_cloud_seq"]) {
    if (Object.hasOwn(args, key)
        && (!Number.isSafeInteger(args[key]) || args[key] < 0)) {
      return { ok: false, detail: `invalid ${key}` };
    }
  }
  if (!args.repository && !args.session_id) {
    return { ok: false, detail: "repository or session_id is required" };
  }
  return {
    ok: true,
    value: {
      repository: args.repository,
      hostId: args.host_id,
      sessionId: args.session_id,
      taskId: args.task_id,
      workspaceId: args.workspace_id,
      query: args.query,
      limit,
      budgetBytes,
      atLeastRevision: args.at_least_revision,
      atLeastCloudSeq: args.at_least_cloud_seq,
    },
  };
}

async function resolveTarget(db, ownerId, filters) {
  if (filters.sessionId) {
    const sql = filters.hostId
      ? `${READ_SESSION_SOURCES} AND host_id = ? ORDER BY host_id, session_id LIMIT 2`
      : `${READ_SESSION_SOURCES} ORDER BY host_id, session_id LIMIT 2`;
    const sources = filters.hostId
      ? await allRows(db, sql, ownerId, filters.sessionId, filters.hostId)
      : await allRows(db, sql, ownerId, filters.sessionId);
    if (sources.length === 0) return { handled: false };
    if (sources.length > 1) {
      return {
        error: contextError("session_mapping_ambiguous", -32006, undefined, {
          session_id: filters.sessionId,
        }),
      };
    }
    const sessionSource = sources[0];
    const mappedRepository = nonEmptyString(sessionSource.repository_key);
    if (filters.repository && mappedRepository !== filters.repository) {
      return {
        error: contextError("session_repository_mismatch", -32004, undefined, {
          session_id: filters.sessionId,
        }),
      };
    }
    return {
      handled: true,
      repositoryKey: mappedRepository ?? null,
      sessionSource,
      sourcesTruncated: false,
    };
  }
  return {
    handled: true,
    repositoryKey: filters.repository,
    sessionSource: null,
    sourcesTruncated: false,
  };
}

async function readRepositorySources(db, ownerId, repositoryKey) {
  const rows = await allRows(db, READ_REPOSITORY_SOURCES, ownerId, repositoryKey, MAX_SOURCE_ROWS + 1);
  return {
    rows: rows.slice(0, MAX_SOURCE_ROWS).map(normalizeSource),
    truncated: rows.length > MAX_SOURCE_ROWS,
  };
}

function sourceFreshness(sourceRows, totals, filters, selectedSource, sourcesTruncated) {
  const sources = sourceRows
    .filter(Boolean)
    .map(normalizeSource)
    .sort((a, b) => a.host_id.localeCompare(b.host_id) || a.session_id.localeCompare(b.session_id));
  const gaps = sources.reduce((sum, source) => sum + source.gap_count, 0);
  const incomplete = sources.length === 0 || sources.some((source) => source.journal_degraded
    || source.gap_count > 0
    || source.source_acked_revision < source.source_head_revision);
  const latestCloudSeq = nonNegativeNumber(totals?.latest_cloud_seq);
  const observationCount = nonNegativeNumber(totals?.observation_count);
  const revisionSource = selectedSource ? normalizeSource(selectedSource) : null;
  const revisionStale = filters.atLeastRevision !== undefined
    && (!revisionSource || revisionSource.source_acked_revision < filters.atLeastRevision);
  const cloudStale = filters.atLeastCloudSeq !== undefined
    && latestCloudSeq < filters.atLeastCloudSeq;
  const partial = incomplete || sourcesTruncated || revisionStale || cloudStale;
  return {
    latest_cloud_seq: latestCloudSeq,
    source_head_revision: revisionSource?.source_head_revision ?? null,
    source_acked_revision: revisionSource?.source_acked_revision ?? null,
    source_gap_count: revisionSource?.gap_count ?? gaps,
    source_cursors: sources.map((source) => ({
      host_id: source.host_id,
      session_id: source.session_id,
      source_head_revision: source.source_head_revision,
      source_acked_revision: source.source_acked_revision,
      cloud_head_seq: source.cloud_head_seq,
      source_gap_count: source.gap_count,
      journal_degraded: source.journal_degraded,
      last_synced_at: source.last_synced_at,
    })),
    source_count: sources.length,
    source_cursors_truncated: sourcesTruncated,
    observation_count: observationCount,
    cloud_observation_stale: incomplete || sourcesTruncated || revisionStale || cloudStale,
    ...(filters.atLeastRevision !== undefined ? { at_least_revision: filters.atLeastRevision } : {}),
    ...(filters.atLeastCloudSeq !== undefined ? { at_least_cloud_seq: filters.atLeastCloudSeq } : {}),
    partial,
  };
}

function normalizeSource(row) {
  return {
    ...row,
    source_head_revision: nonNegativeNumber(row.source_head_revision),
    source_acked_revision: nonNegativeNumber(row.acked_through_revision),
    cloud_head_seq: nonNegativeNumber(row.cloud_head_seq),
    gap_count: nonNegativeNumber(row.gap_count),
    journal_degraded: Number(row.journal_degraded) === 1,
    last_synced_at: validTimestampOrNull(row.last_synced_at),
  };
}

async function readWorkerStatus(env, ownerId, repositoryKey, latestCloudSeq) {
  const status = await memoryWorkerStatus(env, ownerId, repositoryKey);
  const activeProducerVersion = nonEmptyString(status.activeProducerVersion);
  const requestedProducerVersion = nonEmptyString(status.requestedProducerVersion)
    ?? nonEmptyString(status.producerVersion);
  const workerLastCloudSeq = status.workerLastCloudSeq == null
    ? 0
    : nonNegativeNumber(status.workerLastCloudSeq);
  const workerLag = nonNegativeNumber(status.pendingObservationCount ?? status.workerLag);
  const lastSuccessAt = validTimestampOrNull(status.lastSuccessAt);
  const lastErrorAt = validTimestampOrNull(status.lastErrorAt);
  const lastErrorCode = safeErrorCode(status.lastErrorCode);
  return {
    state: ["disabled", "not_configured", "failed", "lagging", "ready", "ready_empty"].includes(status.state)
      ? status.state
      : "failed",
    enabled: status.enabled === true,
    extractor: nonEmptyString(status.extractor) ?? "disabled",
    active_producer_version: activeProducerVersion,
    active_generation: nullableNonNegativeNumber(status.activeGeneration),
    requested_generation: nullableNonNegativeNumber(status.requestedGeneration),
    producer_version: activeProducerVersion,
    ...(requestedProducerVersion && activeProducerVersion !== requestedProducerVersion
      ? { requested_producer_version: requestedProducerVersion }
      : {}),
    ...(lastErrorCode ? { last_error_code: lastErrorCode } : {}),
    worker_last_cloud_seq: workerLastCloudSeq,
    latest_cloud_seq: nonNegativeNumber(status.latestCloudSeq ?? latestCloudSeq),
    worker_lag: workerLag,
    ...(lastSuccessAt ? { last_success_at: lastSuccessAt } : {}),
    ...(lastErrorAt ? { last_error_at: lastErrorAt } : {}),
    stale: status.stale !== false,
  };
}

function unconfiguredWorkerStatus(_env, latestCloudSeq) {
  return {
    state: "not_configured",
    enabled: false,
    extractor: "disabled",
    active_producer_version: null,
    active_generation: null,
    requested_generation: null,
    producer_version: null,
    worker_last_cloud_seq: 0,
    latest_cloud_seq: latestCloudSeq,
    worker_lag: 0,
    last_success_at: null,
    last_error_at: null,
    last_error_code: null,
    stale: true,
  };
}

function workerFreshness(worker) {
  return {
    worker_state: worker.state,
    worker_last_cloud_seq: worker.worker_last_cloud_seq,
    worker_lag: worker.worker_lag,
    ...(worker.last_success_at ? { worker_last_success_at: worker.last_success_at } : {}),
    knowledge_stale: worker.stale,
    ...(worker.last_error_at ? { worker_last_error_at: worker.last_error_at } : {}),
    ...(worker.last_error_code ? { worker_last_error_code: worker.last_error_code } : {}),
  };
}

async function readObservationProjection(db, ownerId, repositoryKey, sessionSource, filters) {
  let where;
  let binds;
  if (sessionSource) {
    where = "WHERE source.owner_id = ? AND source.host_id = ? AND source.session_id = ?";
    binds = [ownerId, sessionSource.host_id, sessionSource.session_id];
  } else {
    where = "WHERE source.owner_id = ? AND source.repository_key = ?";
    binds = [ownerId, repositoryKey];
  }
  if (filters.workspaceId) {
    where += " AND source.workspace_id = ?";
    binds.push(filters.workspaceId);
  }
  if (filters.taskId) {
    where += " AND (source.task_id = ? OR (source.operation_id IS NOT NULL"
      + " AND (SELECT COUNT(DISTINCT linked_task.task_id) FROM observations AS linked_task"
      + " WHERE linked_task.owner_id = source.owner_id AND linked_task.repository_key = source.repository_key"
      + " AND linked_task.host_id = source.host_id AND linked_task.session_id = source.session_id"
      + " AND linked_task.operation_id = source.operation_id AND linked_task.task_id IS NOT NULL) = 1"
      + " AND EXISTS (SELECT 1 FROM observations AS linked"
      + " WHERE linked.owner_id = source.owner_id AND linked.repository_key = source.repository_key"
      + " AND linked.host_id = source.host_id AND linked.session_id = source.session_id"
      + " AND linked.operation_id = source.operation_id AND linked.task_id = ?)))";
    binds.push(filters.taskId, filters.taskId);
  }
  if (filters.query) {
    where += " AND (instr(lower(source.kind), lower(?)) > 0 OR instr(lower(source.action), lower(?)) > 0"
      + " OR instr(lower(COALESCE(source.task_id, '')), lower(?)) > 0"
      + " OR instr(lower(COALESCE(source.state_status, '')), lower(?)) > 0"
      + " OR instr(lower(COALESCE(source.target_backend, '')), lower(?)) > 0"
      + " OR instr(lower(COALESCE(source.content_preview, '')), lower(?)) > 0)";
    binds.push(...Array(6).fill(filters.query));
  }
  const rows = await allRows(
    db,
    `${READ_OBSERVATIONS} ${where} ORDER BY source.cloud_seq DESC LIMIT ?`,
    ...binds,
    MAX_OBSERVATION_ROWS + 1,
  );
  const truncated = rows.length > MAX_OBSERVATION_ROWS;
  const observations = rows.slice(0, MAX_OBSERVATION_ROWS).map(normalizeObservation);
  return {
    observations,
    tasks: projectTasks(observations, filters.limit),
    truncated,
  };
}

function normalizeObservation(row) {
  return {
    cloudSeq: nonNegativeNumber(row.cloud_seq),
    hostId: stringOrNull(row.host_id),
    sessionId: stringOrNull(row.session_id),
    observationId: stringOrNull(row.observation_id),
    sourceRevision: nonNegativeNumber(row.source_revision),
    repositoryKey: stringOrNull(row.repository_key),
    workspaceId: stringOrNull(row.workspace_id),
    taskId: stringOrNull(row.task_id),
    operationId: stringOrNull(row.operation_id),
    kind: stringOrNull(row.kind),
    action: stringOrNull(row.action),
    backend: stringOrNull(row.target_backend),
    contentKind: stringOrNull(row.content_kind),
    contentDigest: stringOrNull(row.content_digest),
    stateStatus: stringOrNull(row.state_status),
    stateRevision: nullableNonNegativeNumber(row.state_revision),
    evidenceRefs: parseEvidenceRefs(row.evidence_refs),
    observedAt: validTimestampOrNull(row.observed_at),
    ingestedAt: validTimestampOrNull(row.ingested_at),
  };
}

function projectTasks(observations, limit) {
  const ordered = [...observations].sort((a, b) => a.cloudSeq - b.cloudSeq);
  const operationTasks = new Map();
  for (const observation of ordered) {
    const key = operationTaskKey(observation);
    if (key && observation.taskId) {
      const taskIds = operationTasks.get(key) ?? new Set();
      taskIds.add(observation.taskId);
      operationTasks.set(key, taskIds);
    }
  }

  const tasks = new Map();
  for (const observation of [...ordered].reverse()) {
    const operationTaskIds = operationTasks.get(operationTaskKey(observation));
    const associatedTaskId = operationTaskIds?.size === 1 ? [...operationTaskIds][0] : null;
    const taskId = observation.taskId
      ?? associatedTaskId;
    if (!taskId) continue;
    let task = tasks.get(taskId);
    if (!task) {
      task = {
        task_id: taskId,
        backend: observation.backend,
        status: null,
        last_observed_at: observation.observedAt,
        last_cloud_seq: observation.cloudSeq,
        state: null,
        instruction_refs: [],
        refs: [],
      };
      tasks.set(taskId, task);
    }
    if (!task.backend && observation.backend) task.backend = observation.backend;
    if (observation.kind === "instruction") {
      addBoundedRef(task.instruction_refs, observation);
    }
    if (observation.stateStatus && !task.state) {
      task.status = observation.stateStatus;
      task.state = {
        status: observation.stateStatus,
        revision: observation.stateRevision,
        authority: "replicated_observed",
        observed_at: observation.observedAt,
      };
      task.last_observed_at = observation.observedAt;
      task.last_cloud_seq = observation.cloudSeq;
    }
    if (["operation_accepted", "execution_state", "verification", "reconciliation", "delivery"].includes(observation.kind)) {
      addBoundedRef(task.refs, observation);
    }
    if (task.instruction_refs.length === 0 && task.refs.length === 0 && observation.kind) {
      addBoundedRef(task.refs, observation);
    }
  }
  const result = [...tasks.values()]
    .map((task) => {
      if (!task.status && task.instruction_refs.length > 0) {
        task.status = "unobserved";
      }
      return task;
    })
    .sort((a, b) => b.last_cloud_seq - a.last_cloud_seq || a.task_id.localeCompare(b.task_id));
  return result.slice(0, limit);
}

function authorizedTaskOperations(observations, taskId) {
  const operations = new Map();
  for (const observation of observations) {
    if (observation.taskId !== taskId
        || !observation.operationId
        || !observation.hostId
        || !observation.sessionId) continue;
    const key = operationTaskKey(observation);
    operations.set(key, {
      operationId: observation.operationId,
      hostId: observation.hostId,
      sessionId: observation.sessionId,
    });
  }
  return [...operations.values()];
}

function operationTaskKey(observation) {
  if (!observation.operationId || !observation.hostId || !observation.sessionId) return null;
  return JSON.stringify([observation.hostId, observation.sessionId, observation.operationId]);
}

function addBoundedRef(refs, observation) {
  if (refs.length >= MAX_REFS) return;
  if (!observation.observationId || refs.some((ref) => ref.observation_id === observation.observationId)) return;
  refs.push({
    observation_id: observation.observationId,
    cloud_seq: observation.cloudSeq,
    source_revision: observation.sourceRevision,
    host_id: observation.hostId,
    session_id: observation.sessionId,
    kind: observation.kind,
    observed_at: observation.observedAt,
  });
}

async function readKnowledgeProjection(db, ownerId, repositoryKey, filters, producerVersion, taskOperations = []) {
  if (!producerVersion) return emptyKnowledgeProjection();
  const now = new Date().toISOString();
  const allOperationScopes = [...new Set(taskOperations.map((operation) => `operation:${operation.operationId}`))].sort();
  const operationScopes = allOperationScopes.slice(0, MAX_OPERATION_SCOPES);
  const operationScopesTruncated = allOperationScopes.length > operationScopes.length;
  const operationScopeClause = operationScopes.length > 0
    ? ` OR (? IS NOT NULL AND scope_type = 'execution' AND scope_id IN (${operationScopes.map(() => "?").join(", ")}))`
    : "";
  const knowledgeSql = READ_KNOWLEDGE.replace(
    "OR (? IS NOT NULL AND scope_type = 'task' AND scope_id = ?))",
    `OR (? IS NOT NULL AND scope_type = 'task' AND scope_id = ?)${operationScopeClause})`,
  );
  const rows = await allRows(
    db,
    knowledgeSql,
    ownerId,
    repositoryKey,
    producerVersion,
    repositoryKey,
    filters.workspaceId ?? null,
    filters.workspaceId ?? null,
    filters.taskId ?? null,
    filters.taskId ?? null,
    ...(operationScopes.length > 0 ? [filters.taskId, ...operationScopes] : []),
    now,
    now,
    filters.query ?? null,
    filters.query ?? null,
    filters.query ?? null,
    MAX_KNOWLEDGE_ROWS + 1,
  );
  const truncated = rows.length > MAX_KNOWLEDGE_ROWS;
  const itemsBeforeValidation = rows.slice(0, MAX_KNOWLEDGE_ROWS);
  const items = itemsBeforeValidation
    .map((row) => normalizeKnowledge(row, ownerId, repositoryKey, filters, now, operationScopes))
    .filter(Boolean);
  const invalidKnowledge = items.length < itemsBeforeValidation.length;
  if (items.length === 0) {
    return {
      items,
      truncated: truncated || operationScopesTruncated,
      missingSupport: invalidKnowledge,
      supportTruncated: false,
    };
  }

  const ids = items.map((item) => item.knowledge_id);
  const supportRead = await readSupportRefs(db, ownerId, repositoryKey, items, filters.taskId, taskOperations);
  const historyRows = await allRows(
    db,
    `${READ_SUPERSESSION_HISTORY} ${ids.map(() => "?").join(", ")}) ORDER BY edge.new_knowledge_id, edge.old_knowledge_id, edge.relationship LIMIT ?`,
    ownerId,
    repositoryKey,
    ...ids,
    MAX_SUPPORT_ROWS + 1,
  );
  const historyTruncated = historyRows.length > MAX_SUPPORT_ROWS;
  const currentById = new Map(items.map((item) => [item.knowledge_id, item]));
  const historyCandidates = historyRows.slice(0, MAX_SUPPORT_ROWS).filter((row) =>
    currentById.has(row.new_knowledge_id)
    && nonEmptyString(row.old_knowledge_id)
    && nonEmptyString(row.relationship));
  const oldItems = historyCandidates.map((row) => normalizeHistoricalKnowledge(
    row,
    currentById.get(row.new_knowledge_id),
    ownerId,
    repositoryKey,
  ));
  const oldItemsById = new Map(oldItems.filter(Boolean).map((item) => [item.knowledge_id, item]));
  const historySupportRead = await readSupportRefs(
    db,
    ownerId,
    repositoryKey,
    [...oldItemsById.values()],
    filters.taskId,
    taskOperations,
  );
  const historyByCurrentId = new Map();
  const supersedesById = new Map();
  for (let index = 0; index < historyCandidates.length; index += 1) {
    const edge = historyCandidates[index];
    const oldItem = oldItems[index];
    const oldSupport = oldItem ? historySupportRead.supportById.get(oldItem.knowledge_id) ?? [] : [];
    const list = historyByCurrentId.get(edge.new_knowledge_id) ?? [];
    if (list.length >= MAX_REFS) continue;
    list.push({
      knowledge_id: edge.old_knowledge_id,
      relationship: edge.relationship,
      available: Boolean(oldItem),
      kind: oldItem?.kind ?? null,
      status: oldItem?.status ?? null,
      scope_type: oldItem?.scope_type ?? null,
      scope_id: oldItem?.scope_id ?? null,
      text: oldItem?.text ?? null,
      text_omitted: Boolean(oldItem?.textOmitted),
      ...(oldItem ? { support_incomplete: oldItem.supportIncomplete } : {}),
      support_state: oldSupport.length > 0 ? "available" : "missing",
      support_refs: oldSupport,
      authority: "derived",
    });
    historyByCurrentId.set(edge.new_knowledge_id, list);
    const supersededList = supersedesById.get(edge.new_knowledge_id) ?? [];
    if (supersededList.length < MAX_REFS) {
      supersededList.push({ knowledge_id: edge.old_knowledge_id, relationship: edge.relationship });
    }
    supersedesById.set(edge.new_knowledge_id, supersededList);
  }

  let missingSupport = false;
  const supportedItems = items.flatMap((item) => {
    const supportRefs = supportRead.supportById.get(item.knowledge_id) ?? [];
    if (supportRefs.length === 0) {
      missingSupport = true;
      return [];
    }
    return [{
      ...item,
      support_refs: supportRefs,
      supersedes: supersedesById.get(item.knowledge_id) ?? [],
      supersession_history: historyByCurrentId.get(item.knowledge_id) ?? [],
    }];
  });
  return {
    items: supportedItems,
    truncated: truncated || invalidKnowledge || operationScopesTruncated,
    missingSupport: missingSupport || invalidKnowledge,
    historicalSupportMissing: historySupportRead.missingSupport,
    supportTruncated: supportRead.truncated || historySupportRead.truncated || historyTruncated,
  };
}

async function readSupportRefs(db, ownerId, repositoryKey, items, requestedTaskId = undefined, taskOperations = []) {
  if (items.length === 0) return { supportById: new Map(), truncated: false, missingSupport: false };
  const ids = [...new Set(items.map((item) => item.knowledge_id))];
  const rows = [];
  let truncated = false;
  for (let offset = 0; offset < ids.length && rows.length <= MAX_SUPPORT_ROWS; offset += 80) {
    const chunk = ids.slice(offset, offset + 80);
    const query = `${READ_SUPPORTS_FOR_KNOWLEDGE} ${chunk.map(() => "?").join(", ")})`
      + " ORDER BY support.knowledge_id, support.observation_cloud_seq, support.support_role LIMIT ?";
    const chunkRows = await allRows(
      db,
      query,
      ownerId,
      repositoryKey,
      ...chunk,
      MAX_SUPPORT_ROWS + 1 - rows.length,
    );
    rows.push(...chunkRows);
    truncated ||= rows.length > MAX_SUPPORT_ROWS;
  }
  const itemById = new Map(items.map((item) => [item.knowledge_id, item]));
  const supportById = new Map();
  for (const support of rows.slice(0, MAX_SUPPORT_ROWS)) {
    const item = itemById.get(support.knowledge_id);
    const normalized = normalizeSupport(support);
    if (!item || !supportMatchesScope(normalized, item, requestedTaskId, taskOperations)) continue;
    const list = supportById.get(item.knowledge_id) ?? [];
    if (list.length < MAX_SUPPORTS_PER_ITEM) list.push(normalized);
    supportById.set(item.knowledge_id, list);
  }
  return {
    supportById,
    truncated,
    missingSupport: items.some((item) => (supportById.get(item.knowledge_id) ?? []).length === 0),
  };
}

function normalizeHistoricalKnowledge(row, current, ownerId, repositoryKey) {
  if (!current
      || row.owner_id !== ownerId
      || row.repository_key !== repositoryKey
      || row.scope_type !== current.scope_type
      || row.scope_id !== current.scope_id
      || !KINDS.has(row.kind)
      || !["supported", "current", "superseded", "retracted"].includes(row.status)
      || typeof row.text !== "string") {
    return null;
  }
  const textBytes = utf8Bytes(row.text);
  return {
    knowledge_id: row.old_knowledge_id,
    scope_type: row.scope_type,
    scope_id: row.scope_id,
    kind: row.kind,
    status: row.status,
    text: textBytes <= 4096 ? row.text : null,
    textOmitted: textBytes > 4096,
    supportIncomplete: incompleteFlag(row.support_incomplete),
  };
}

function normalizeKnowledge(row, ownerId, repositoryKey, filters, now, operationScopes = []) {
  if (!nonEmptyString(row.knowledge_id)
      || row.owner_id !== ownerId
      || row.repository_key !== repositoryKey
      || !["repository", "workspace", "task", "execution"].includes(row.scope_type)
      || !nonEmptyString(row.scope_id)
      || !KINDS.has(row.kind)
      || !KNOWLEDGE_STATUSES.has(row.status)
      || typeof row.text !== "string"
      || utf8Bytes(row.text) > MAX_KNOWLEDGE_TEXT_BYTES
      || !nonEmptyString(row.producer_version)
      || !validTimestampOrNull(row.produced_at)) {
    return null;
  }
  if ((row.scope_type === "repository" && row.scope_id !== repositoryKey)
      || (row.scope_type === "workspace" && row.scope_id !== filters.workspaceId)
      || (row.scope_type === "task" && row.scope_id !== filters.taskId)
      || (row.scope_type === "execution" && !operationScopes.includes(row.scope_id))) {
    return null;
  }
  if (row.valid_until !== null && row.valid_until !== undefined
      && (!validTimestampOrNull(row.valid_until) || row.valid_until <= now)) {
    return null;
  }
  if (row.valid_from !== null && row.valid_from !== undefined
      && (!validTimestampOrNull(row.valid_from) || row.valid_from > now)) {
    return null;
  }
  const confidence = row.confidence === null || row.confidence === undefined
    ? null
    : Number(row.confidence);
  if (confidence !== null && (!Number.isFinite(confidence) || confidence < 0 || confidence > 1)) {
    return null;
  }
  return {
    knowledge_id: row.knowledge_id,
    scope_type: row.scope_type,
    scope_id: row.scope_id,
    kind: row.kind,
    text: row.text,
    status: row.status,
    confidence,
    valid_from: validTimestampOrNull(row.valid_from),
    valid_until: validTimestampOrNull(row.valid_until),
    producer: nonEmptyString(row.producer),
    producer_version: row.producer_version,
    produced_at: row.produced_at,
    source_through_cloud_seq: nonNegativeNumber(row.source_through_cloud_seq),
    support_incomplete: incompleteFlag(row.support_incomplete),
  };
}

function incompleteFlag(value) {
  if (value === true || Number(value ?? 0) !== 0) return true;
  return false;
}

function normalizeSupport(row) {
  return {
    observation_id: stringOrNull(row.observation_id),
    cloud_seq: nonNegativeNumber(row.observation_cloud_seq),
    source_revision: nonNegativeNumber(row.source_revision),
    host_id: stringOrNull(row.host_id),
    session_id: stringOrNull(row.session_id),
    task_id: stringOrNull(row.effective_task_id ?? row.task_id),
    operation_id: stringOrNull(row.operation_id),
    workspace_id: stringOrNull(row.workspace_id),
    observation_kind: stringOrNull(row.kind),
    role: stringOrNull(row.support_role),
  };
}

function supportMatchesScope(support, item, requestedTaskId, taskOperations) {
  if (item.scope_type === "workspace") return support.workspace_id === item.scope_id;
  if (item.scope_type === "task") return support.task_id === item.scope_id;
  if (item.scope_type === "execution") {
    if (!requestedTaskId || !item.scope_id.startsWith("operation:")) return false;
    const operationId = item.scope_id.slice("operation:".length);
    return support.operation_id === operationId
      && support.task_id === requestedTaskId
      && taskOperations.some((operation) => operation.operationId === operationId
        && operation.hostId === support.host_id
        && operation.sessionId === support.session_id);
  }
  return item.scope_type === "repository";
}

function emptyKnowledgeProjection() {
  return { items: [], truncated: false, missingSupport: false, supportTruncated: false };
}

function assembleContext({ repositoryKey, filters, freshness, worker, observationResult, knowledgeResult, sourceSource }) {
  const tasks = observationResult.tasks;
  const unresolved = tasks
    .filter((task) => NEEDS_ATTENTION_STATES.has(task.status ?? "unobserved"))
    .map((task) => ({
      task_id: task.task_id,
      status: task.status ?? "unobserved",
      reason: task.status === "unobserved"
        ? "instruction or accepted operation has no observed state"
        : "latest replicated host state needs attention",
      refs: task.state ? task.refs.slice(0, 1) : task.instruction_refs.slice(0, 1),
      authority: "replicated_observed",
    }));
  const summaries = knowledgeResult.items.filter((item) => item.kind === "summary");
  const currentSummary = {
    observations: freshness.observation_count,
    latest_cloud_seq: freshness.latest_cloud_seq,
    tasks_total: tasks.length,
    tasks_active: tasks.filter((task) => task.status && !TERMINAL_TASK_STATES.has(task.status)).length,
    tasks_terminal: tasks.filter((task) => task.status && TERMINAL_TASK_STATES.has(task.status)).length,
    last_observed_at: tasks.map((task) => task.last_observed_at).filter(Boolean).sort().at(-1) ?? null,
    knowledge_summary: summaries[0]?.text ?? null,
    knowledge_summary_refs: summaries[0]?.support_refs ?? [],
    knowledge_summary_support_incomplete: summaries[0]?.support_incomplete ?? null,
  };
  if (summaries[0]) {
    currentSummary.knowledge_summary = summaries[0].text;
    if (summaries[0].support_refs.length > 0) currentSummary.knowledge_summary_refs = summaries[0].support_refs;
  }
  const byKind = (kind) => knowledgeResult.items.filter((item) => item.kind === kind);
  const knowledgeUnresolved = byKind("unresolved").map((item) => ({
    knowledge_id: item.knowledge_id,
    text: item.text,
    status: item.status,
    scope_type: item.scope_type,
    scope_id: item.scope_id,
    support_refs: item.support_refs,
    support_incomplete: item.support_incomplete,
    supersedes: item.supersedes,
    supersession_history: item.supersession_history,
    authority: "derived",
  }));
  const refs = [];
  for (const task of tasks) {
    for (const ref of [...task.instruction_refs, ...task.refs]) {
      if (refs.length >= MAX_REFS) break;
      if (!refs.some((candidate) => candidate.observation_id === ref.observation_id)) refs.push(ref);
    }
  }
  const partialReasons = [];
  if (freshness.partial) partialReasons.push("source_incomplete");
  if (observationResult.truncated) partialReasons.push("observations_truncated");
  if (knowledgeResult.truncated) partialReasons.push("knowledge_truncated");
  if (knowledgeResult.missingSupport) partialReasons.push("unsupported_knowledge_omitted");
  if (knowledgeResult.historicalSupportMissing) partialReasons.push("historical_support_missing");
  if (knowledgeResult.supportTruncated) partialReasons.push("support_truncated");
  if (knowledgeResult.items.some((item) => item.support_incomplete
      || item.supersession_history.some((history) => history.support_incomplete === true))) {
    partialReasons.push("knowledge_support_incomplete");
  }
  const context = {
    context_schema_version: 1,
    scope: scopeProjection(repositoryKey, filters),
    authority: { live: false, observations: "replicated_observed", knowledge: "derived" },
    current_summary: currentSummary,
    relevant_facts: byKind("fact").map(projectKnowledge),
    relevant_decisions: byKind("decision").map(projectKnowledge),
    constraints: byKind("constraint").map(projectKnowledge),
    unresolved: [...unresolved, ...knowledgeUnresolved],
    known_failure_patterns: byKind("failure_pattern").map(projectKnowledge),
    recent_related_tasks: tasks,
    refs,
    freshness: {
      ...freshness,
      ...workerFreshness(worker),
      knowledge_stale: worker.stale,
    },
    memory: worker,
    provenance: {
      knowledge_source: worker.active_producer_version ? "active_derived_projection" : "none",
      ...(worker.active_producer_version ? { producer_version: worker.active_producer_version } : {}),
    },
    partial: {
      value: partialReasons.length > 0,
      reasons: partialReasons,
    },
    response_truncated: false,
    deterministic: true,
  };
  if (sourceSource && sourceSource.repository_key === null) {
    context.partial.value = true;
    context.partial.reasons.push("repository_identity_unresolved");
  }
  return context;
}

function projectKnowledge(item) {
  return {
    knowledge_id: item.knowledge_id,
    text: item.text,
    status: item.status,
    scope_type: item.scope_type,
    scope_id: item.scope_id,
    confidence: item.confidence,
    valid_from: item.valid_from,
    valid_until: item.valid_until,
    producer: item.producer,
    producer_version: item.producer_version,
    produced_at: item.produced_at,
    source_through_cloud_seq: item.source_through_cloud_seq,
    support_refs: item.support_refs,
    support_incomplete: item.support_incomplete,
    supersedes: item.supersedes,
    supersession_history: item.supersession_history,
    authority: "derived",
  };
}

function fitBudget(context, budgetBytes) {
  if (utf8Bytes(JSON.stringify(context)) <= budgetBytes) return context;
  const orderedArrays = [
    "relevant_facts",
    "relevant_decisions",
    "constraints",
    "unresolved",
    "known_failure_patterns",
    "recent_related_tasks",
    "refs",
  ];
  const candidates = new Map(orderedArrays.map((field) => [field, [...context[field]]]));
  let truncated = false;
  for (const field of orderedArrays) context[field] = [];
  for (const field of orderedArrays) {
    for (const value of candidates.get(field)) {
      context[field].push(value);
      if (utf8Bytes(JSON.stringify(context)) > budgetBytes - 80) {
        context[field].pop();
        truncated = true;
      }
    }
  }
  if (truncated) {
    context.response_truncated = true;
    context.partial.value = true;
    if (!context.partial.reasons.includes("response_budget_exceeded")) {
      context.partial.reasons.push("response_budget_exceeded");
    }
  }
  if (utf8Bytes(JSON.stringify(context)) > budgetBytes) {
    context.current_summary.knowledge_summary = null;
    context.current_summary.knowledge_summary_refs = [];
    while (context.freshness.source_cursors.length > 0
      && utf8Bytes(JSON.stringify(context)) > budgetBytes - 80) {
      context.freshness.source_cursors.pop();
    }
    if (context.freshness.source_cursors.length < context.freshness.source_count) {
      context.freshness.source_cursors_truncated = true;
      context.partial.value = true;
      if (!context.partial.reasons.includes("source_cursor_response_budget")) {
        context.partial.reasons.push("source_cursor_response_budget");
      }
    }
    delete context.provenance;
  }
  if (utf8Bytes(JSON.stringify(context)) > budgetBytes) {
    for (const field of [...orderedArrays].reverse()) {
      while (context[field].length > 0 && utf8Bytes(JSON.stringify(context)) > budgetBytes) {
        context[field].pop();
      }
    }
    context.response_truncated = true;
    context.partial.value = true;
    if (!context.partial.reasons.includes("response_budget_exceeded")) {
      context.partial.reasons.push("response_budget_exceeded");
    }
  }
  if (utf8Bytes(JSON.stringify(context)) > budgetBytes) {
    const freshness = context.freshness;
    const reasons = [...new Set([...context.partial.reasons, "response_budget_exceeded"])];
    const compact = {
      context_schema_version: context.context_schema_version,
      scope: context.scope,
      authority: context.authority,
      current_summary: {
        observations: context.current_summary.observations,
        latest_cloud_seq: context.current_summary.latest_cloud_seq,
      },
      relevant_facts: [],
      relevant_decisions: [],
      constraints: [],
      unresolved: [],
      known_failure_patterns: [],
      recent_related_tasks: [],
      refs: [],
      freshness: {
        latest_cloud_seq: freshness.latest_cloud_seq,
        source_head_revision: freshness.source_head_revision,
        source_acked_revision: freshness.source_acked_revision,
        source_gap_count: freshness.source_gap_count,
        source_count: freshness.source_count,
        source_cursors: [],
        source_cursors_truncated: true,
        observation_count: freshness.observation_count,
        cloud_observation_stale: freshness.cloud_observation_stale,
        worker_state: freshness.worker_state,
        worker_last_cloud_seq: freshness.worker_last_cloud_seq,
        worker_lag: freshness.worker_lag,
        knowledge_stale: freshness.knowledge_stale,
      },
      memory: { state: context.memory.state, stale: context.memory.stale },
      partial: { value: true, reasons },
      response_truncated: true,
      deterministic: context.deterministic,
    };
    if (utf8Bytes(JSON.stringify(compact)) > budgetBytes) return null;
    return compact;
  }
  return context;
}

function scopeProjection(repositoryKey, filters) {
  return Object.fromEntries(Object.entries({
    repository: repositoryKey ?? null,
    session_id: filters.sessionId ?? null,
    host_id: filters.hostId ?? null,
    task_id: filters.taskId ?? null,
    workspace_id: filters.workspaceId ?? null,
  }).filter(([, value]) => value !== null));
}

async function firstRow(db, sql, ...values) {
  const statement = db.prepare(sql).bind(...values);
  if (typeof statement.first === "function") return statement.first();
  const rows = await allRows(db, sql, ...values);
  return rows[0] ?? null;
}

async function allRows(db, sql, ...values) {
  const result = await db.prepare(sql).bind(...values).all();
  if (result?.success === false) throw new Error("D1 query failed");
  if (!Array.isArray(result?.results)) throw new Error("D1 query returned invalid rows");
  return result.results;
}

function contextError(error, code, detail = undefined, data = undefined) {
  const fields = { handled: true, error, code };
  if (detail) fields.data = { detail };
  if (data) fields.data = data;
  return fields;
}

function trustedOwnerId(env) {
  const value = env?.OBSERVATION_OWNER_ID;
  return validBoundedString(value, MAX_OWNER_ID) && /^[A-Za-z0-9._:@/-]+$/.test(value) ? value : null;
}

function isD1(db) {
  return Boolean(db && typeof db.prepare === "function");
}

function parseEvidenceRefs(value) {
  if (typeof value !== "string") return [];
  try {
    const decoded = JSON.parse(value);
    return Array.isArray(decoded) ? decoded.slice(0, MAX_REFS) : [];
  } catch {
    return [];
  }
}

function validTimestampOrNull(value) {
  if (typeof value !== "string" || value.length > 64 || !Number.isFinite(Date.parse(value))) return null;
  return value;
}

function safeErrorCode(value) {
  return typeof value === "string" && /^[A-Za-z0-9_.-]{1,64}$/.test(value) ? value : null;
}

function parseBoolean(value, fallback) {
  if (typeof value === "boolean") return value;
  if (typeof value !== "string") return fallback;
  if (["true", "1", "yes", "on"].includes(value.toLowerCase())) return true;
  if (["false", "0", "no", "off"].includes(value.toLowerCase())) return false;
  return fallback;
}

function validBoundedString(value, maxLength) {
  return typeof value === "string" && value.trim() === value && value.length > 0 && value.length <= maxLength;
}

function nonEmptyString(value) {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function stringOrNull(value) {
  return typeof value === "string" ? value : null;
}

function nonNegativeNumber(value) {
  const number = Number(value ?? 0);
  return Number.isSafeInteger(number) && number >= 0 ? number : 0;
}

function nullableNonNegativeNumber(value) {
  if (value === null || value === undefined) return null;
  const number = Number(value);
  return Number.isSafeInteger(number) && number >= 0 ? number : null;
}

function utf8Bytes(value) {
  return new TextEncoder().encode(value).byteLength;
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
