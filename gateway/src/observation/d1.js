// C1 D1 persistence boundary. D1Database.batch() is the only write primitive:
// Cloudflare guarantees one batch is an ordered transaction and rolls it back
// if any statement fails. Queue delivery is deliberately outside this module.

const READ_SOURCE = [
  "SELECT owner_id, host_id, session_id, repository_key, source_base_revision,",
  "source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count",
  "FROM observation_sources",
  "WHERE owner_id = ? AND host_id = ? AND session_id = ?",
].join(" ");

const READ_RECORD = [
  "SELECT observation_id, source_revision, payload_digest",
  "FROM observations",
  "WHERE owner_id = ? AND host_id = ? AND session_id = ?",
  "AND (observation_id = ? OR source_revision = ?)",
  "ORDER BY cloud_seq",
].join(" ");

const INSERT_SOURCE = [
  "INSERT INTO observation_sources (",
  "owner_id, host_id, session_id, repository_key, source_base_revision,",
  "source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count, last_synced_at",
  ") VALUES (?, ?, ?, ?, ?, 0, 0, 0, 0, 0, ?)",
].join(" ");

const INSERT_OBSERVATION = [
  "INSERT INTO observations (",
  "owner_id, host_id, session_id, observation_id, source_revision, schema_version, repository_key,",
  "workspace_id, task_id, execution_id, operation_id, kind, action, actor_transport, actor_principal_ref,",
  "target_backend, content_kind, content_preview, content_digest, content_ref, state_status, state_revision,",
  "evidence_refs, observed_at, ingested_at, payload_digest",
  ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
].join(" ");

const UPDATE_SOURCE = [
  "UPDATE observation_sources SET",
  "repository_key = CASE WHEN ? IS NULL THEN repository_key ELSE ? END,",
  "source_base_revision = MAX(source_base_revision, ?),",
  "source_head_revision = MAX(source_head_revision, ?),",
  "journal_degraded = CASE WHEN journal_degraded = 1 OR ? = 1 THEN 1 ELSE 0 END,",
  "gap_count = MAX(gap_count, ?),",
  "acked_through_revision = MAX(acked_through_revision, acked_through_revision + (",
  "WITH ordered AS (",
  "SELECT source_revision, ROW_NUMBER() OVER (ORDER BY source_revision) AS rn",
  "FROM observations",
  "WHERE owner_id = observation_sources.owner_id",
  "AND host_id = observation_sources.host_id",
  "AND session_id = observation_sources.session_id",
  "AND source_revision > observation_sources.acked_through_revision",
  "AND source_revision <= MAX(observation_sources.source_head_revision, ?)",
  ") SELECT COALESCE((",
  "SELECT MIN(rn) - 1 FROM ordered",
  "WHERE source_revision <> observation_sources.acked_through_revision + rn",
  "), (SELECT COUNT(*) FROM ordered))",
  ")),",
  "cloud_head_seq = MAX(cloud_head_seq, COALESCE((",
  "SELECT MAX(cloud_seq) FROM observations",
  "WHERE owner_id = observation_sources.owner_id",
  "AND host_id = observation_sources.host_id",
  "AND session_id = observation_sources.session_id",
  "), 0)),",
  "last_synced_at = ?",
  "WHERE owner_id = ? AND host_id = ? AND session_id = ?",
].join(" ");

export async function ingestD1Batch(db, batch) {
  // A concurrent retry can race between the read snapshot and write batch.
  // Retry once after any batch failure: the second preflight classifies a
  // concurrently committed exact replay as success and a conflicting replay
  // as 409. No write uses REPLACE or overwrite semantics.
  for (let attempt = 0; attempt < 2; attempt += 1) {
    const snapshot = await readSnapshot(db, batch);
    const replay = classifyReplay(snapshot, batch);
    if (!replay.ok) return replay;
    try {
      await commit(db, batch, snapshot.source, replay.missing);
      const source = await readSource(db, batch.ownerId, batch.hostId, batch.sessionId);
      if (!source) throw new Error("observation source missing after D1 commit");
      return {
        ok: true,
        source,
      };
    } catch (error) {
      if (attempt === 0) continue;
      throw error;
    }
  }
  throw new Error("observation D1 retry exhausted");
}

async function readSnapshot(db, batch) {
  const statements = [
    db.prepare(READ_SOURCE).bind(batch.ownerId, batch.hostId, batch.sessionId),
    ...batch.records.map((record) => db.prepare(READ_RECORD).bind(
      batch.ownerId,
      batch.hostId,
      batch.sessionId,
      record.observationId,
      record.sourceRevision,
    )),
  ];
  const results = await db.batch(statements);
  return {
    source: results[0]?.results?.[0] ?? null,
    records: results.slice(1).map((result) => result?.results ?? []),
  };
}

function classifyReplay(snapshot, batch) {
  const sourceRepository = snapshot.source?.repository_key ?? null;
  if (sourceRepository !== null && batch.repositoryKey !== null && sourceRepository !== batch.repositoryKey) {
    return { ok: false, detail: "repository_key conflicts with existing source" };
  }

  const missing = [];
  for (let index = 0; index < batch.records.length; index += 1) {
    const incoming = batch.records[index];
    const existing = snapshot.records[index];
    if (existing.length === 0) {
      missing.push(incoming);
      continue;
    }
    const exact = existing.length === 1
      && existing[0].observation_id === incoming.observationId
      && Number(existing[0].source_revision) === incoming.sourceRevision
      && existing[0].payload_digest === incoming.payloadDigest;
    if (!exact) {
      return {
        ok: false,
        detail: "conflicting replay at source_revision " + incoming.sourceRevision,
      };
    }
  }
  return { ok: true, missing };
}

async function commit(db, batch, source, missing) {
  const now = new Date().toISOString();
  const repositoryKey = batch.repositoryKey ?? source?.repository_key ?? null;
  const statements = [];

  if (!source) {
    statements.push(db.prepare(INSERT_SOURCE).bind(
      batch.ownerId,
      batch.hostId,
      batch.sessionId,
      repositoryKey,
      batch.sourceBaseRevision,
      now,
    ));
  }

  for (const record of missing) {
    const o = record.observation;
    statements.push(db.prepare(INSERT_OBSERVATION).bind(
      batch.ownerId,
      batch.hostId,
      batch.sessionId,
      record.observationId,
      record.sourceRevision,
      o.schemaVersion,
      repositoryKey,
      o.workspaceId,
      o.taskId,
      o.executionId,
      o.operationId,
      o.kind,
      o.action,
      o.actorTransport,
      o.actorPrincipal,
      o.targetBackend,
      o.contentKind,
      o.contentPreview,
      o.contentDigest,
      null,
      o.stateStatus,
      o.stateRevision,
      o.evidenceRefs,
      o.observedAt,
      now,
      record.payloadDigest,
    ));
  }

  statements.push(db.prepare(UPDATE_SOURCE).bind(
    repositoryKey,
    repositoryKey,
    batch.sourceBaseRevision,
    batch.sourceHeadRevision,
    batch.journalDegraded ? 1 : 0,
    batch.gapCount,
    batch.sourceHeadRevision,
    now,
    batch.ownerId,
    batch.hostId,
    batch.sessionId,
  ));

  await db.batch(statements);
}

function readSource(db, ownerId, hostId, sessionId) {
  return db.prepare(READ_SOURCE).bind(ownerId, hostId, sessionId).first();
}

export const C1_D1_SQL = Object.freeze({
  readSource: READ_SOURCE,
  readRecord: READ_RECORD,
  insertSource: INSERT_SOURCE,
  insertObservation: INSERT_OBSERVATION,
  updateSource: UPDATE_SOURCE,
});
