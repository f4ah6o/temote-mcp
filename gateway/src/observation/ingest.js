import { authorizeFederatedHost } from "../access.js";
import { jsonResponse, readJson, unauthorizedHost, withCors } from "../http.js";
import { validateHostId, validateSessionId } from "../protocol.js";
import {
  FABRIC_AUTHORITY,
  OBSERVATION_SCHEMA_VERSION,
  OBSERVATION_SYNC_CONTRACT,
} from "./schema.js";
import { ingestD1Batch } from "./d1.js";

export const MAX_OBSERVATION_SYNC_BODY_BYTES = 1024 * 1024;
export const MAX_OBSERVATION_SYNC_RECORDS = 256;

const MAX_ID = 256;
const MAX_REPOSITORY_KEY = 512;
const MAX_PREVIEW_BYTES = 8192;
const MAX_EVIDENCE_REFS = 64;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const SHA256 = /^[0-9a-f]{64}$/i;
const KINDS = new Set([
  "instruction",
  "operation_accepted",
  "execution_state",
  "evidence",
  "verification",
  "delivery",
  "reconciliation",
]);
const TOP_LEVEL_FIELDS = new Set([
  "id", "schema_version", "observed_at", "accepted_at", "session_id", "session_instance",
  "repository", "workspace_id", "task_id", "execution_id", "operation_id", "actor",
  "target", "action", "kind", "content", "state_ref", "evidence_refs", "provenance",
  "revision", "dedupe_key",
]);
const SENSITIVE_EXACT = new Set([
  "task", "input", "prompt", "auth", "api_key", "apikey", "bearer", "private_key",
]);
const SENSITIVE_PARTS = [
  "secret", "token", "password", "passwd", "credential", "authorization",
];

export function observationSyncHostId(pathname) {
  const match = /^\/v1\/hosts\/([^/]+)\/observations\/sync$/.exec(pathname);
  if (!match) return null;
  try {
    const hostId = decodeURIComponent(match[1]);
    return validateHostId(hostId) ? hostId : null;
  } catch {
    return null;
  }
}

export async function handleObservationSync(request, env, hostId) {
  if (request.method !== OBSERVATION_SYNC_CONTRACT.method) {
    return withCors(new Response(null, { status: 405 }));
  }

  const authenticatedHost = request.headers.get("x-temote-host-id");
  if (
    authenticatedHost !== hostId
    || !validateHostId(authenticatedHost)
    || !authorizeFederatedHost(request, env, authenticatedHost)
  ) {
    return unauthorizedHost();
  }

  const ownerId = trustedOwnerId(env);
  if (!ownerId) return reply({ error: "observation_owner_unconfigured" }, 503);
  if (!isD1(env?.OBSERVATION_DB)) return reply({ error: "observation_db_unavailable" }, 503);

  const parsed = await readJson(request, MAX_OBSERVATION_SYNC_BODY_BYTES);
  if (!parsed.ok) return reply({ error: "invalid_json", detail: parsed.error }, 400);

  const checked = await validateObservationSyncRequest(parsed.value);
  if (!checked.ok) {
    return reply({ error: checked.error, detail: checked.detail }, checked.status ?? 400);
  }

  try {
    const stored = await ingestD1Batch(env.OBSERVATION_DB, {
      ownerId,
      hostId,
      ...checked.value,
    });
    if (!stored.ok) {
      return reply({ error: "observation_conflict", detail: stored.detail }, 409);
    }

    const source = stored.source;
    const complete = Number(source.journal_degraded) === 0
      && Number(source.gap_count) === 0
      && Number(source.acked_through_revision) >= Number(source.source_head_revision);
    return reply({
      session_id: checked.value.sessionId,
      acked_through_revision: Number(source.acked_through_revision),
      cloud_head_seq: Number(source.cloud_head_seq),
      complete,
      authority: FABRIC_AUTHORITY.replicatedObservation,
    });
  } catch (error) {
    // Never log observation bodies. Bounded error text is enough for operator diagnosis.
    console.error("observation ingest failed", boundedError(error));
    return reply({ error: "observation_ingest_failed" }, 500);
  }
}

export async function validateObservationSyncRequest(body) {
  if (!object(body)) return invalid("invalid_sync_request", "request must be an object");

  const allowed = new Set([...OBSERVATION_SYNC_CONTRACT.requestRequired, "repository_key"]);
  const unknown = Object.keys(body).find((key) => !allowed.has(key));
  if (unknown) return invalid("invalid_sync_request", "unknown request field: " + unknown);

  for (const field of OBSERVATION_SYNC_CONTRACT.requestRequired) {
    if (!Object.hasOwn(body, field)) return invalid("invalid_sync_request", "missing " + field);
  }
  if (body.schema_version !== OBSERVATION_SYNC_CONTRACT.schemaVersion) {
    return invalid("unsupported_schema_version", "unsupported schema_version", 422);
  }
  if (!validateSessionId(body.session_id)) return invalid("invalid_session_id", "invalid session_id");
  if (!nonNegativeInt(body.source_base_revision) || !nonNegativeInt(body.source_head_revision)
      || body.source_base_revision > body.source_head_revision) {
    return invalid("invalid_revision_range", "invalid source revision range");
  }
  if (typeof body.journal_degraded !== "boolean") {
    return invalid("invalid_journal_degraded", "journal_degraded must be boolean");
  }
  if (!nonNegativeInt(body.gap_count)) return invalid("invalid_gap_count", "invalid gap_count");
  if (body.repository_key !== undefined
      && (!bounded(body.repository_key, MAX_REPOSITORY_KEY) || body.repository_key.trim() !== body.repository_key)) {
    return invalid("invalid_repository_key", "invalid repository_key");
  }
  if (!Array.isArray(body.records) || body.records.length > MAX_OBSERVATION_SYNC_RECORDS) {
    return invalid("invalid_records", "records exceed bounded batch");
  }

  const revisions = new Set();
  const observationIds = new Set();
  const records = [];
  for (const record of body.records) {
    if (!object(record)
        || Object.keys(record).length !== 2
        || !Object.hasOwn(record, "source_revision")
        || !Object.hasOwn(record, "observation")) {
      return invalid("invalid_record", "record must contain source_revision and observation");
    }
    if (!Number.isSafeInteger(record.source_revision)
        || record.source_revision <= body.source_base_revision
        || record.source_revision > body.source_head_revision) {
      return invalid("invalid_source_revision", "source_revision outside declared range");
    }
    if (revisions.has(record.source_revision)) {
      return invalid("duplicate_source_revision", "duplicate source_revision in request");
    }

    const observation = validateObservation(
      record.observation,
      body.session_id,
      record.source_revision,
    );
    if (!observation.ok) return observation;
    if (observationIds.has(record.observation.id)) {
      return invalid("duplicate_observation_id", "duplicate observation id in request");
    }

    revisions.add(record.source_revision);
    observationIds.add(record.observation.id);
    records.push({
      sourceRevision: record.source_revision,
      observationId: record.observation.id,
      payloadDigest: await digest(record.observation),
      observation: observation.value,
    });
  }

  return {
    ok: true,
    value: {
      sessionId: body.session_id,
      repositoryKey: body.repository_key ?? null,
      sourceBaseRevision: body.source_base_revision,
      sourceHeadRevision: body.source_head_revision,
      journalDegraded: body.journal_degraded,
      gapCount: body.gap_count,
      records,
    },
  };
}

function validateObservation(value, sessionId, sourceRevision) {
  if (!object(value)) return invalid("invalid_observation", "observation must be an object");
  const unknown = Object.keys(value).find((key) => !TOP_LEVEL_FIELDS.has(key));
  if (unknown) return invalid("invalid_observation", "unknown observation field: " + unknown);

  for (const field of [
    "id", "schema_version", "observed_at", "session_id", "session_instance", "actor",
    "target", "action", "kind", "content", "evidence_refs", "provenance", "revision", "dedupe_key",
  ]) {
    if (!Object.hasOwn(value, field)) return invalid("invalid_observation", "missing observation." + field);
  }

  if (!UUID.test(value.id)) return invalid("invalid_observation", "observation.id must be a UUID");
  if (value.schema_version !== OBSERVATION_SCHEMA_VERSION) {
    return invalid("unsupported_observation_schema", "unsupported observation schema", 422);
  }
  if (!unixSecond(value.observed_at)) return invalid("invalid_observation", "invalid observed_at");
  if (value.session_id !== sessionId) return invalid("invalid_observation", "session_id mismatch");
  if (value.revision !== sourceRevision) return invalid("invalid_observation", "revision mismatch");
  if (!KINDS.has(value.kind)) return invalid("invalid_observation", "invalid observation kind");
  if (!bounded(value.action, 128) || !bounded(value.dedupe_key, 1024)) {
    return invalid("invalid_observation", "invalid action or dedupe_key");
  }

  if (!object(value.session_instance)
      || !nonNegativeInt(value.session_instance.started_at)
      || !nonNegativeInt(value.session_instance.process_id)) {
    return invalid("invalid_observation", "invalid session_instance");
  }
  if (!object(value.actor) || !bounded(value.actor.transport, 128)
      || !optionalBounded(value.actor.principal, MAX_ID)) {
    return invalid("invalid_observation", "invalid actor");
  }
  if (!object(value.target) || !bounded(value.target.backend, 128)) {
    return invalid("invalid_observation", "invalid target");
  }
  if (!object(value.provenance) || !bounded(value.provenance.tool, 128)
      || value.provenance.source !== "orchestration") {
    return invalid("invalid_observation", "invalid provenance");
  }

  for (const field of ["workspace_id", "task_id", "execution_id", "operation_id"]) {
    if (!optionalBounded(value[field], MAX_ID)) {
      return invalid("invalid_observation", "invalid " + field);
    }
  }

  if (!Array.isArray(value.evidence_refs) || value.evidence_refs.length > MAX_EVIDENCE_REFS) {
    return invalid("invalid_observation", "invalid evidence_refs");
  }
  if (hasSensitiveStructuredKey(value.content)) {
    return invalid("sensitive_observation_field", "secret-bearing structured field rejected", 422);
  }

  const content = normalizeContent(value.content);
  if (!content.ok) return content;
  const state = normalizeState(value.state_ref);
  if (!state.ok) return state;

  return {
    ok: true,
    value: {
      schemaVersion: value.schema_version,
      observedAt: new Date(value.observed_at * 1000).toISOString(),
      workspaceId: value.workspace_id ?? null,
      taskId: value.task_id ?? null,
      executionId: value.execution_id ?? null,
      operationId: value.operation_id ?? null,
      kind: value.kind,
      action: value.action,
      actorTransport: value.actor.transport,
      actorPrincipal: value.actor.principal ?? null,
      targetBackend: value.target.backend,
      contentKind: content.value.kind,
      contentPreview: content.value.preview,
      contentDigest: content.value.digest,
      stateStatus: state.value.status,
      stateRevision: state.value.revision,
      evidenceRefs: JSON.stringify(value.evidence_refs),
    },
  };
}

function normalizeContent(value) {
  if (!object(value) || !bounded(value.kind, 32)) {
    return invalid("invalid_observation", "invalid content");
  }
  if (value.kind === "none") return { ok: true, value: { kind: "none", preview: null, digest: null } };
  if (value.kind === "text") {
    if (typeof value.preview !== "string" || bytes(value.preview) > 4096 || !SHA256.test(value.sha256)) {
      return invalid("invalid_observation", "invalid text content");
    }
    return { ok: true, value: { kind: "text", preview: value.preview, digest: value.sha256 } };
  }
  if (value.kind === "error") {
    if (typeof value.preview !== "string" || bytes(value.preview) > 512) {
      return invalid("invalid_observation", "invalid error content");
    }
    return { ok: true, value: { kind: "error", preview: value.preview, digest: null } };
  }
  if (value.kind === "view") {
    if (!Object.hasOwn(value, "view")) return invalid("invalid_observation", "missing view content");
    const serialized = JSON.stringify(value.view);
    if (bytes(serialized) > MAX_PREVIEW_BYTES) return invalid("invalid_observation", "view content exceeds bound");
    return { ok: true, value: { kind: "view", preview: serialized, digest: null } };
  }
  if (value.kind === "view_digest") {
    if (!SHA256.test(value.sha256)) return invalid("invalid_observation", "invalid view digest");
    return { ok: true, value: { kind: "view_digest", preview: null, digest: value.sha256 } };
  }
  return invalid("invalid_observation", "unknown content kind");
}

function normalizeState(value) {
  if (value === undefined || value === null) {
    return { ok: true, value: { status: null, revision: null } };
  }
  if (!object(value) || !optionalBounded(value.status, MAX_ID)
      || (value.revision !== undefined && value.revision !== null && !nonNegativeInt(value.revision))) {
    return invalid("invalid_observation", "invalid state_ref");
  }
  return {
    ok: true,
    value: {
      status: value.status ?? null,
      revision: value.revision ?? null,
    },
  };
}

function trustedOwnerId(env) {
  const value = env?.OBSERVATION_OWNER_ID;
  return bounded(value, MAX_ID) && /^[A-Za-z0-9._:@/-]+$/.test(value) ? value : null;
}

function isD1(db) {
  return db && typeof db.prepare === "function" && typeof db.batch === "function";
}

function hasSensitiveStructuredKey(value) {
  if (Array.isArray(value)) return value.some(hasSensitiveStructuredKey);
  if (!object(value)) return false;
  return Object.entries(value).some(([key, child]) => {
    const lower = key.toLowerCase();
    return SENSITIVE_EXACT.has(lower)
      || SENSITIVE_PARTS.some((part) => lower.includes(part))
      || hasSensitiveStructuredKey(child);
  });
}

async function digest(value) {
  const bytesValue = new TextEncoder().encode(canonicalJson(value));
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytesValue));
  return Array.from(hash, (part) => part.toString(16).padStart(2, "0")).join("");
}

function canonicalJson(value) {
  if (Array.isArray(value)) return "[" + value.map(canonicalJson).join(",") + "]";
  if (object(value)) {
    return "{" + Object.keys(value).sort().map(
      (key) => JSON.stringify(key) + ":" + canonicalJson(value[key]),
    ).join(",") + "}";
  }
  return JSON.stringify(value);
}

function reply(value, status = 200) {
  return withCors(jsonResponse(value, status, { "cache-control": "no-store" }));
}

function invalid(error, detail, status) {
  return { ok: false, error, detail, ...(status ? { status } : {}) };
}

function bounded(value, max) {
  return typeof value === "string" && value.length > 0 && value.length <= max;
}

function optionalBounded(value, max) {
  return value === undefined || value === null || bounded(value, max);
}

function nonNegativeInt(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function unixSecond(value) {
  return nonNegativeInt(value) && value <= 253402300799;
}

function object(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function bytes(value) {
  return new TextEncoder().encode(value).byteLength;
}

function boundedError(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message.length <= 256 ? message : message.slice(0, 256);
}
