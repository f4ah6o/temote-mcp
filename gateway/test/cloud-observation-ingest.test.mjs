import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

import worker from "../src/index.js";
import {
  MAX_OBSERVATION_SYNC_BODY_BYTES,
  MAX_OBSERVATION_SYNC_RECORDS,
  validateObservationSyncRequest,
} from "../src/observation/ingest.js";
import { C1_D1_SQL } from "../src/observation/d1.js";
import { FABRIC_AUTHORITY } from "../src/observation/schema.js";

const HOST = "host-a";
const SESSION = "session-a";
const REPOSITORY = "github:f4ah6o/temote-mcp";
const TOKEN = "host-secret";
const wrangler = fs.readFileSync(
  new URL("../wrangler.toml", import.meta.url),
  "utf8",
);

function observation(revision, idSuffix = revision, overrides = {}) {
  const suffix = String(idSuffix).padStart(12, "0");
  return {
    id: "00000000-0000-4000-8000-" + suffix,
    schema_version: 1,
    observed_at: 1790000000 + revision,
    session_id: SESSION,
    session_instance: { started_at: 1790000000, process_id: 42 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "opencode" },
    action: "task_get",
    kind: "execution_state",
    content: { kind: "none" },
    evidence_refs: [],
    provenance: { tool: "opencode_task_get", source: "orchestration" },
    revision,
    dedupe_key: "state:" + SESSION + ":" + revision,
    ...overrides,
  };
}

function body(records, overrides = {}) {
  const revisions = records.map((record) => record.source_revision);
  return {
    schema_version: 1,
    session_id: SESSION,
    repository_key: REPOSITORY,
    source_base_revision: 0,
    source_head_revision: revisions.length ? Math.max(...revisions) : 0,
    journal_degraded: false,
    gap_count: 0,
    records,
    ...overrides,
  };
}

function record(revision, idSuffix = revision, overrides = {}) {
  return {
    source_revision: revision,
    observation: observation(revision, idSuffix, overrides),
  };
}

function env(db = new FakeD1()) {
  return {
    HOST_TOKENS_JSON: JSON.stringify({ [HOST]: TOKEN }),
    OBSERVATION_OWNER_ID: "owner-a",
    OBSERVATION_DB: db,
    GATEWAY_SESSIONS: {
      idFromName() { throw new Error("execution authority must not be touched"); },
    },
  };
}

function request(value, host = HOST, token = TOKEN, headers = {}) {
  return new Request("https://fabric.example/v1/hosts/" + host + "/observations/sync", {
    method: "POST",
    headers: {
      authorization: "Bearer " + token,
      "content-type": "application/json",
      "x-temote-host-id": host,
      ...headers,
    },
    body: JSON.stringify(value),
  });
}

async function sync(value, options = {}) {
  const database = options.db ?? new FakeD1();
  const response = await worker.fetch(
    request(
      value,
      options.pathHost ?? HOST,
      options.token ?? TOKEN,
      options.headers ?? {},
    ),
    options.env ?? env(database),
  );
  return { response, database, json: await response.json() };
}

test("C1 contract accepts the supported schema and rejects incompatible validation", async () => {
  const accepted = await validateObservationSyncRequest(body([record(1)]));
  assert.equal(accepted.ok, true);

  const unsupported = await validateObservationSyncRequest(body([record(1)], { schema_version: 2 }));
  assert.equal(unsupported.ok, false);
  assert.equal(unsupported.status, 422);

  const invalidRange = await validateObservationSyncRequest(body([], {
    source_base_revision: 3,
    source_head_revision: 2,
  }));
  assert.equal(invalidRange.error, "invalid_revision_range");

  const duplicate = await validateObservationSyncRequest(body([record(1), record(1, 2)]));
  assert.equal(duplicate.error, "duplicate_source_revision");

  const oversized = await validateObservationSyncRequest(body([], {
    source_head_revision: MAX_OBSERVATION_SYNC_RECORDS + 1,
    records: Array.from(
      { length: MAX_OBSERVATION_SYNC_RECORDS + 1 },
      (_, index) => record(index + 1),
    ),
  }));
  assert.equal(oversized.error, "invalid_records");

  const ownerInjection = await validateObservationSyncRequest({
    ...body([record(1)]),
    owner_id: "owner-b",
  });
  assert.equal(ownerInjection.error, "invalid_sync_request");
});

test("C1 deployment config declares the D1 binding and migration directory", () => {
  assert.match(wrangler, /OBSERVATION_OWNER_ID\s*=\s*"replace-with-owner-id"/);
  assert.match(wrangler, /\[\[d1_databases\]\][\s\S]*binding\s*=\s*"OBSERVATION_DB"/);
  assert.match(wrangler, /database_name\s*=\s*"temote-observation"/);
  assert.match(wrangler, /migrations_dir\s*=\s*"migrations"/);
});

test("host authentication and path host identity are mandatory", async () => {
  const database = new FakeD1();
  const noAuth = await worker.fetch(new Request(
    "https://fabric.example/v1/hosts/" + HOST + "/observations/sync",
    {
      method: "POST",
      headers: { "content-type": "application/json", "x-temote-host-id": HOST },
      body: JSON.stringify(body([record(1)])),
    },
  ), env(database));
  assert.equal(noAuth.status, 401);

  const mismatch = await worker.fetch(request(body([record(1)]), "host-b"), env(database));
  assert.equal(mismatch.status, 401);
  assert.equal(database.observations.length, 0);
});

test("bounded request body is enforced before ingest", async () => {
  const database = new FakeD1();
  const response = await worker.fetch(request(
    body([]),
    HOST,
    TOKEN,
    { "content-length": String(MAX_OBSERVATION_SYNC_BODY_BYTES + 1) },
  ), env(database));
  assert.equal(response.status, 400);
  assert.equal(database.observations.length, 0);
});

test("first ingest creates the source, persists repository identity, and returns cloud sequence", async () => {
  const database = new FakeD1();
  const { response, json } = await sync(body([record(1)]), { db: database });
  assert.equal(response.status, 200);
  assert.equal(json.acked_through_revision, 1);
  assert.equal(json.cloud_head_seq, 1);
  assert.equal(json.complete, true);
  assert.equal(json.authority, FABRIC_AUTHORITY.replicatedObservation);
  assert.equal(database.observations.length, 1);
  const source = database.source("owner-a", HOST, SESSION);
  assert.equal(source.repository_key, REPOSITORY);
  assert.equal(source.source_head_revision, 1);
});

test("exact replay is idempotent while conflicting revision or observation id is rejected", async () => {
  const database = new FakeD1();
  let result = await sync(body([record(1)]), { db: database });
  assert.equal(result.response.status, 200);
  result = await sync(body([record(1)]), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(database.observations.length, 1);

  result = await sync(body([record(1, 99)]), { db: database });
  assert.equal(result.response.status, 409);
  assert.equal(database.observations.length, 1);

  result = await sync(body([record(2, 1)], { source_head_revision: 2 }), { db: database });
  assert.equal(result.response.status, 409);
  assert.equal(database.observations.length, 1);
});

test("ack advances only through the committed contiguous prefix and never regresses", async () => {
  const database = new FakeD1();
  let result = await sync(body([record(1), record(3)], { source_head_revision: 3 }), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(result.json.acked_through_revision, 1);
  assert.equal(result.json.complete, false);

  result = await sync(body([record(2)], { source_head_revision: 3 }), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(result.json.acked_through_revision, 3);
  assert.equal(result.json.complete, true);

  result = await sync(body([], { source_head_revision: 2 }), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(result.json.acked_through_revision, 3);
});

test("repository identity can resolve once but cannot be rebound", async () => {
  const database = new FakeD1();
  let result = await sync(body([record(1)], { repository_key: undefined }), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(database.source("owner-a", HOST, SESSION).repository_key, null);

  result = await sync(body([record(2)], {
    source_head_revision: 2,
    repository_key: REPOSITORY,
  }), { db: database });
  assert.equal(result.response.status, 200);
  assert.equal(database.source("owner-a", HOST, SESSION).repository_key, REPOSITORY);

  result = await sync(body([], {
    source_head_revision: 2,
    repository_key: "github:other/repository",
  }), { db: database });
  assert.equal(result.response.status, 409);
  assert.equal(database.source("owner-a", HOST, SESSION).repository_key, REPOSITORY);
});

test("D1 commit failure returns no ACK and leaves the source uncommitted", async () => {
  const database = new FailingWriteD1();
  const result = await sync(body([record(1)]), { db: database });
  assert.equal(result.response.status, 500);
  assert.equal(Object.hasOwn(result.json, "acked_through_revision"), false);
  assert.equal(Object.hasOwn(result.json, "cloud_head_seq"), false);
  assert.equal(database.observations.length, 0);
  assert.equal(database.source("owner-a", HOST, SESSION), undefined);
});

test("known journal degradation is preserved and Fabric never fabricates missing revisions", async () => {
  const database = new FakeD1();
  let result = await sync(body([record(1), record(3)], {
    source_head_revision: 3,
    journal_degraded: true,
    gap_count: 1,
  }), { db: database });
  assert.equal(result.json.complete, false);
  assert.equal(result.json.acked_through_revision, 1);
  assert.deepEqual(database.observations.map((item) => item.source_revision), [1, 3]);

  result = await sync(body([record(2)], {
    source_head_revision: 3,
    journal_degraded: false,
    gap_count: 0,
  }), { db: database });
  assert.equal(result.json.acked_through_revision, 3);
  assert.equal(result.json.complete, false);
  const source = database.source("owner-a", HOST, SESSION);
  assert.equal(source.journal_degraded, 1);
  assert.equal(source.gap_count, 1);
});

test("secret-bearing structured content is rejected and execution authority is not touched", async () => {
  const database = new FakeD1();
  const unsafe = record(1, 1, {
    content: { kind: "view", view: { task_id: "t", api_token: "secret" } },
  });
  const result = await sync(body([unsafe]), { db: database });
  assert.equal(result.response.status, 422);
  assert.equal(database.observations.length, 0);

  const safe = await sync(body([record(1)]), { db: database });
  assert.equal(safe.response.status, 200);
  assert.equal(safe.json.authority, "replicated_observed");
});

test("C1 SQL uses insert-only replay semantics and computes a windowed contiguous prefix", () => {
  assert.equal(Object.values(C1_D1_SQL).some((sql) => /\bREPLACE\b/i.test(sql)), false);
  assert.match(C1_D1_SQL.updateSource, /ROW_NUMBER\(\) OVER \(ORDER BY source_revision\)/);
  assert.match(C1_D1_SQL.updateSource, /acked_through_revision = MAX/);
});

class FakeStatement {
  constructor(db, sql, args = []) {
    this.db = db;
    this.sql = sql;
    this.args = args;
  }

  bind(...args) {
    return new FakeStatement(this.db, this.sql, args);
  }

  async first() {
    return this.db.execute(this, this.db.state).results[0] ?? null;
  }
}

class FakeD1 {
  constructor() {
    this.state = { sources: new Map(), observations: [], nextSeq: 1 };
  }

  get observations() {
    return this.state.observations;
  }

  source(owner, host, session) {
    return this.state.sources.get(owner + "\n" + host + "\n" + session);
  }

  prepare(sql) {
    return new FakeStatement(this, sql, []);
  }

  async batch(statements) {
    const next = {
      sources: new Map(Array.from(this.state.sources, ([key, value]) => [key, { ...value }])),
      observations: this.state.observations.map((value) => ({ ...value })),
      nextSeq: this.state.nextSeq,
    };
    const results = statements.map((statement) => this.execute(statement, next));
    this.state = next;
    return results;
  }

  execute(statement, state) {
    const sql = statement.sql.trim();
    const args = statement.args;

    if (sql.startsWith("SELECT owner_id")) {
      const key = args[0] + "\n" + args[1] + "\n" + args[2];
      const source = state.sources.get(key);
      return { results: source ? [{ ...source }] : [] };
    }

    if (sql.startsWith("SELECT observation_id")) {
      const [owner, host, session, id, revision] = args;
      const results = state.observations.filter((item) =>
        item.owner_id === owner
        && item.host_id === host
        && item.session_id === session
        && (item.observation_id === id || item.source_revision === revision)
      ).map((item) => ({
        observation_id: item.observation_id,
        source_revision: item.source_revision,
        payload_digest: item.payload_digest,
      }));
      return { results };
    }

    if (sql.startsWith("INSERT INTO observation_sources")) {
      const [owner, host, session, repository, base, now] = args;
      const key = owner + "\n" + host + "\n" + session;
      if (state.sources.has(key)) throw new Error("UNIQUE observation_sources");
      state.sources.set(key, {
        owner_id: owner,
        host_id: host,
        session_id: session,
        repository_key: repository,
        source_base_revision: base,
        source_head_revision: 0,
        acked_through_revision: 0,
        cloud_head_seq: 0,
        journal_degraded: 0,
        gap_count: 0,
        last_synced_at: now,
      });
      return { results: [], meta: { changes: 1 } };
    }

    if (sql.startsWith("INSERT INTO observations")) {
      const [
        owner, host, session, id, revision, schemaVersion, repository,
        workspaceId, taskId, executionId, operationId, kind, action,
        actorTransport, actorPrincipal, targetBackend, contentKind,
        contentPreview, contentDigest, contentRef, stateStatus, stateRevision,
        evidenceRefs, observedAt, ingestedAt, payloadDigest,
      ] = args;
      const source = state.sources.get(owner + "\n" + host + "\n" + session);
      if (!source || source.repository_key !== repository) throw new Error("source repository mismatch");
      if (state.observations.some((item) =>
        item.owner_id === owner && item.host_id === host && item.session_id === session
        && (item.observation_id === id || item.source_revision === revision)
      )) throw new Error("UNIQUE observations");
      if (typeof payloadDigest !== "string" || payloadDigest.length !== 64) {
        throw new Error("payload_digest required");
      }
      state.observations.push({
        cloud_seq: state.nextSeq++,
        owner_id: owner,
        host_id: host,
        session_id: session,
        observation_id: id,
        source_revision: revision,
        schema_version: schemaVersion,
        repository_key: repository,
        workspace_id: workspaceId,
        task_id: taskId,
        execution_id: executionId,
        operation_id: operationId,
        kind,
        action,
        actor_transport: actorTransport,
        actor_principal_ref: actorPrincipal,
        target_backend: targetBackend,
        content_kind: contentKind,
        content_preview: contentPreview,
        content_digest: contentDigest,
        content_ref: contentRef,
        state_status: stateStatus,
        state_revision: stateRevision,
        evidence_refs: evidenceRefs,
        observed_at: observedAt,
        ingested_at: ingestedAt,
        payload_digest: payloadDigest,
      });
      return { results: [], meta: { changes: 1 } };
    }

    if (sql.startsWith("UPDATE observation_sources")) {
      const [repository, repositoryAgain, base, head, degraded, gapCount, _headAgain, now, owner, host, session] = args;
      const key = owner + "\n" + host + "\n" + session;
      const source = state.sources.get(key);
      if (!source) throw new Error("missing source");
      if (repository !== repositoryAgain) throw new Error("repository bind mismatch");
      if (repository !== null) {
        if (source.repository_key !== null && source.repository_key !== repository) {
          throw new Error("repository_key immutable");
        }
        source.repository_key = repository;
      }
      source.source_base_revision = Math.max(source.source_base_revision, base);
      source.source_head_revision = Math.max(source.source_head_revision, head);
      source.journal_degraded = source.journal_degraded || degraded ? 1 : 0;
      source.gap_count = Math.max(source.gap_count, gapCount);
      while (
        source.acked_through_revision < source.source_head_revision
        && state.observations.some((item) =>
          item.owner_id === owner
          && item.host_id === host
          && item.session_id === session
          && item.source_revision === source.acked_through_revision + 1
        )
      ) {
        source.acked_through_revision += 1;
      }
      source.cloud_head_seq = Math.max(
        source.cloud_head_seq,
        ...state.observations.filter((item) =>
          item.owner_id === owner && item.host_id === host && item.session_id === session
        ).map((item) => item.cloud_seq),
        0,
      );
      source.last_synced_at = now;
      return { results: [], meta: { changes: 1 } };
    }

    throw new Error("unexpected SQL in FakeD1: " + sql.slice(0, 80));
  }
}


class FailingWriteD1 extends FakeD1 {
  async batch(statements) {
    if (statements.some((statement) => /^(INSERT|UPDATE)\b/.test(statement.sql.trim()))) {
      throw new Error("simulated D1 write failure");
    }
    return super.batch(statements);
  }
}
