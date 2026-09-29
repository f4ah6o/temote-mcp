import assert from "node:assert/strict";
import test from "node:test";

import { resolveCloudContext } from "../src/context/index.js";
import { memoryConfiguration } from "../src/memory/config.js";

const OWNER = "owner-a";
const REPOSITORY_A = "forge:example/repo-a";
const REPOSITORY_B = "forge:example/repo-b";
const PRODUCER = (await memoryConfiguration({
  MEMORY_ENABLED: "true",
  MEMORY_EXTRACTOR: "fixture",
})).producerVersion;
const OTHER_PRODUCER = "producer-staging";
const STAMP = "2026-09-20T00:00:00.000Z";

function source({ owner = OWNER, host, session, repository = REPOSITORY_A, head = 5, acked = 5, cloud = 4, gaps = 0, degraded = 0 }) {
  return {
    owner_id: owner,
    host_id: host,
    session_id: session,
    repository_key: repository,
    source_base_revision: 0,
    source_head_revision: head,
    acked_through_revision: acked,
    cloud_head_seq: cloud,
    journal_degraded: degraded,
    gap_count: gaps,
    last_synced_at: STAMP,
  };
}

function observation({
  owner = OWNER,
  host = "host-a",
  session = "session-a",
  repo = REPOSITORY_A,
  seq,
  id,
  revision = seq,
  kind,
  task = null,
  operation = null,
  state = null,
  action = "task_get",
  preview = null,
}) {
  return {
    owner_id: owner,
    host_id: host,
    session_id: session,
    repository_key: repo,
    cloud_seq: seq,
    observation_id: id,
    source_revision: revision,
    workspace_id: "workspace-a",
    task_id: task,
    operation_id: operation,
    kind,
    action,
    target_backend: "codex",
    content_kind: preview ? "text" : "none",
    content_preview: preview,
    content_digest: preview ? "a".repeat(64) : null,
    state_status: state,
    state_revision: state ? 1 : null,
    evidence_refs: "[]",
    observed_at: STAMP,
    ingested_at: STAMP,
  };
}

function knowledge({
  owner = OWNER,
  repo = REPOSITORY_A,
  id,
  scopeType = "repository",
  scopeId = repo,
  kind = "fact",
  status = "current",
  text,
  version = PRODUCER,
  validUntil = null,
  supportIncomplete = false,
}) {
  return {
    knowledge_id: id,
    owner_id: owner,
    repository_key: repo,
    scope_type: scopeType,
    scope_id: scopeId,
    kind,
    semantic_key: id,
    text,
    status,
    confidence: 0.9,
    valid_from: STAMP,
    valid_until: validUntil,
    producer: "fixture",
    producer_version: version,
    produced_at: STAMP,
    source_through_cloud_seq: 4,
    support_incomplete: supportIncomplete ? 1 : 0,
  };
}

function fixtureDb(overrides = {}) {
  const rows = {
    sources: [
      source({ host: "host-a", session: "session-a", head: 5, acked: 5, cloud: 3 }),
      source({ host: "host-b", session: "session-b", head: 4, acked: 3, cloud: 8, gaps: 1 }),
      source({ owner: "owner-b", host: "host-x", session: "foreign", repository: REPOSITORY_A }),
    ],
    observations: [
      observation({ seq: 1, id: "ob-instruction", kind: "instruction", operation: "op-a", preview: "PRIVATE TASK SECRET - never inline" }),
      observation({ seq: 2, id: "ob-accepted", kind: "operation_accepted", operation: "op-a", task: "task-a" }),
      observation({ seq: 3, id: "ob-state", kind: "execution_state", task: "task-a", state: "completed" }),
      observation({ seq: 4, id: "ob-other-host", host: "host-b", session: "session-b", task: "task-b", operation: "op-a", state: "running" }),
      observation({ seq: 7, id: "ob-colliding-op-instruction", host: "host-b", session: "session-b", kind: "instruction", operation: "op-a", preview: "other task instruction" }),
      observation({ seq: 8, id: "ob-colliding-op-accepted", host: "host-b", session: "session-b", kind: "operation_accepted", operation: "op-a", task: "task-b" }),
      observation({ seq: 9, id: "ob-repo-b", repo: REPOSITORY_B, task: "repo-b-task", state: "failed" }),
      observation({ owner: "owner-b", seq: 10, id: "ob-other-owner", task: "other-owner-task", state: "failed" }),
    ],
    heads: [{
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      requested_producer_version: PRODUCER,
      requested_generation: 1,
      active_producer_version: PRODUCER,
      active_generation: 1,
      epoch: 7,
    }],
    checkpoints: [{
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      worker_id: "temote-memory-v1",
      producer_version: PRODUCER,
      last_cloud_seq: 8,
      last_success_at: STAMP,
      last_error_at: null,
      last_error_code: null,
      stale: 0,
      projection_epoch: 7,
    }],
    knowledge: [
      knowledge({ id: "fact-a", text: "The repository uses Rust." }),
      knowledge({ id: "decision-a", kind: "decision", text: "Keep the public gateway session-independent." }),
      knowledge({ id: "constraint-a", kind: "constraint", text: "Do not expose local observation bodies." }),
      knowledge({ id: "summary-a", kind: "summary", text: "A compact repository summary." }),
      knowledge({ id: "old-a", kind: "constraint", status: "superseded", text: "Old constraint." }),
      knowledge({ id: "expired-a", kind: "fact", text: "Expired fact.", validUntil: "2026-01-01T00:00:00.000Z" }),
      knowledge({ id: "staged-a", text: "Staged projection must stay hidden.", version: OTHER_PRODUCER }),
      knowledge({ id: "foreign-repo", repo: REPOSITORY_B, id: "foreign-repo", text: "Repository B secret." }),
      knowledge({ id: "foreign-owner", owner: "owner-b", id: "foreign-owner", text: "Other owner secret." }),
      knowledge({ id: "unsupported-a", text: "This has no support and must be omitted." }),
    ],
    supports: [
      { owner_id: OWNER, repository_key: REPOSITORY_A, knowledge_id: "fact-a", observation_cloud_seq: 3, observation_id: "ob-state", support_role: "observed_state" },
      { owner_id: OWNER, repository_key: REPOSITORY_A, knowledge_id: "decision-a", observation_cloud_seq: 2, observation_id: "ob-accepted", support_role: "instruction" },
      { owner_id: OWNER, repository_key: REPOSITORY_A, knowledge_id: "constraint-a", observation_cloud_seq: 1, observation_id: "ob-instruction", support_role: "instruction" },
      { owner_id: OWNER, repository_key: REPOSITORY_A, knowledge_id: "summary-a", observation_cloud_seq: 3, observation_id: "ob-state", support_role: "observed_state" },
      { owner_id: OWNER, repository_key: REPOSITORY_A, knowledge_id: "old-a", observation_cloud_seq: 1, observation_id: "ob-instruction", support_role: "historical_instruction" },
      { owner_id: "owner-b", repository_key: REPOSITORY_A, knowledge_id: "fact-a", observation_cloud_seq: 6, observation_id: "ob-other-owner", support_role: "cross_owner" },
      { owner_id: OWNER, repository_key: REPOSITORY_B, knowledge_id: "fact-a", observation_cloud_seq: 5, observation_id: "ob-repo-b", support_role: "cross_repo" },
    ],
    supersessions: [
      { owner_id: OWNER, repository_key: REPOSITORY_A, new_knowledge_id: "constraint-a", old_knowledge_id: "old-a", relationship: "explicit_change" },
    ],
  };
  Object.assign(rows, overrides);
  return new FakeD1(rows);
}

class FakeD1 {
  constructor(rows) {
    this.rows = rows;
    this.maxBoundParameterCount = 0;
  }

  prepare(sql) {
    const db = this;
    return {
      bind: (...values) => {
        db.maxBoundParameterCount = Math.max(db.maxBoundParameterCount, values.length);
        return {
          all: async () => ({ results: db.query(sql, values) }),
          first: async () => db.query(sql, values)[0] ?? null,
        };
      },
    };
  }

  async batch() {
    return [];
  }

  query(sql, values) {
    if (sql.includes("FROM observation_sources") && sql.includes("session_id = ?")) {
      const [owner, session, host] = values;
      let result = this.rows.sources.filter((row) => row.owner_id === owner && row.session_id === session);
      if (sql.includes("AND host_id = ?")) result = result.filter((row) => row.host_id === host);
      return result.sort((a, b) => a.host_id.localeCompare(b.host_id));
    }
    if (sql.includes("FROM observation_sources") && sql.includes("repository_key = ?")) {
      const [owner, repo, limit] = values;
      return this.rows.sources
        .filter((row) => row.owner_id === owner && row.repository_key === repo)
        .sort((a, b) => a.host_id.localeCompare(b.host_id) || a.session_id.localeCompare(b.session_id))
        .slice(0, limit);
    }
    if (sql.includes("AS latest_cloud_seq")) {
      const filtered = sql.includes("repository_key = ?")
        ? this.rows.observations.filter((row) => row.owner_id === values[0] && row.repository_key === values[1])
        : this.rows.observations.filter((row) => row.owner_id === values[0] && row.host_id === values[1] && row.session_id === values[2]);
      return [{
        latest_cloud_seq: filtered.reduce((max, row) => Math.max(max, row.cloud_seq), 0),
        observation_count: filtered.length,
      }];
    }
    if (sql.includes("FROM memory_projection_heads AS head")) {
      const [, owner, repo] = values;
      const head = this.rows.heads.find((row) => row.owner_id === owner && row.repository_key === repo);
      if (!head) return [];
      const checkpoint = this.rows.checkpoints.find((row) => row.owner_id === owner
        && row.repository_key === repo
        && row.worker_id === values[0]
        && row.producer_version === head.active_producer_version);
      return [{ ...head, ...(checkpoint ?? {}) }];
    }
    if (sql.includes("FROM memory_projection_heads")) {
      return this.rows.heads.filter((row) => row.owner_id === values[0] && row.repository_key === values[1]);
    }
    if (sql.includes("FROM memory_checkpoints")) {
      const versionIndex = sql.includes("worker_id = ? AND producer_version = ?") ? 3 : 2;
      return this.rows.checkpoints.filter((row) => row.owner_id === values[0]
        && row.repository_key === values[1]
        && row.producer_version === values[versionIndex]);
    }
    if (sql.includes("COUNT(*) AS knowledge_count")) {
      const count = this.rows.knowledge.filter((row) => row.owner_id === values[0]
        && row.repository_key === values[1]
        && row.producer_version === values[2]
        && ["current", "supported"].includes(row.status)).length;
      return [{ knowledge_count: count }];
    }
    if (sql.includes("COUNT(*) AS pending_count")) {
      const [owner, repo, cursor] = values;
      return [{ pending_count: this.rows.observations.filter((row) => row.owner_id === owner
        && row.repository_key === repo
        && row.cloud_seq > cursor).length }];
    }
    if (sql.includes("FROM knowledge_items")) return this.knowledgeRows(sql, values);
    if (sql.includes("FROM knowledge_support")) return this.supportRows(values);
    if (sql.includes("FROM knowledge_supersession")) return this.supersessionRows(values);
    if (sql.includes("FROM observations")) return this.observationRows(sql, values);
    throw new Error(`unexpected SQL: ${sql}`);
  }

  knowledgeRows(sql, values) {
    const owner = values[0];
    const repo = values[1];
    const producer = values[2];
    const repoScope = values[3];
    const workspaceForCheck = values[4];
    const workspace = values[5];
    const taskForCheck = values[6];
    const task = values[7];
    const operationScopeCount = Number(sql.match(/scope_id IN \(([^)]*)\)/)?.[1].match(/\?/g)?.length ?? 0);
    const operationCheck = operationScopeCount > 0 ? values[8] : null;
    const operationScopes = values.slice(operationScopeCount > 0 ? 9 : 8, operationScopeCount > 0 ? 9 + operationScopeCount : 8);
    const tailIndex = 8 + (operationScopeCount > 0 ? 1 + operationScopeCount : 0);
    const now = values[tailIndex];
    const query = values[tailIndex + 2];
    const limit = values.at(-1);
    const matched = this.rows.knowledge.filter((row) => {
      if (row.owner_id !== owner || row.repository_key !== repo || row.producer_version !== producer) return false;
      if (!["current", "supported"].includes(row.status)) return false;
      const scopeMatches = row.scope_type === "repository" && row.scope_id === repoScope
        || workspaceForCheck !== null && row.scope_type === "workspace" && row.scope_id === workspace
        || taskForCheck !== null && row.scope_type === "task" && row.scope_id === task
        || operationCheck !== null && row.scope_type === "execution" && operationScopes.includes(row.scope_id);
      if (!scopeMatches) return false;
      if (row.valid_from && row.valid_from > now) return false;
      if (row.valid_until && row.valid_until <= now) return false;
      if (query && !row.text.toLowerCase().includes(query.toLowerCase())
          && !row.semantic_key.toLowerCase().includes(query.toLowerCase())) return false;
      return true;
    }).sort((a, b) => (a.status === "current" ? 0 : 1) - (b.status === "current" ? 0 : 1)
      || a.kind.localeCompare(b.kind)
      || b.produced_at.localeCompare(a.produced_at)
      || a.knowledge_id.localeCompare(b.knowledge_id)).slice(0, limit);
    return matched;
  }

  supportRows(values) {
    const [owner, repo, ...rest] = values;
    const limit = rest.at(-1);
    const ids = rest.slice(0, -1);
    return this.rows.supports.filter((support) => support.owner_id === owner
      && support.repository_key === repo
      && ids.includes(support.knowledge_id)
      && this.rows.observations.some((observation) => observation.owner_id === owner
        && observation.repository_key === repo
        && observation.cloud_seq === support.observation_cloud_seq
        && observation.observation_id === support.observation_id))
      .map((support) => {
        const observation = this.rows.observations.find((row) => row.owner_id === owner
          && row.repository_key === repo
          && row.cloud_seq === support.observation_cloud_seq
          && row.observation_id === support.observation_id);
        const linkedTaskIds = new Set(this.rows.observations.filter((row) => row.owner_id === owner
          && row.repository_key === repo
          && row.host_id === observation?.host_id
          && row.session_id === observation?.session_id
          && row.operation_id
          && row.operation_id === observation?.operation_id
          && row.task_id).map((row) => row.task_id));
        const linkedTask = observation?.task_id ?? (linkedTaskIds.size === 1 ? [...linkedTaskIds][0] : null);
        return {
          ...support,
          host_id: observation?.host_id,
          session_id: observation?.session_id,
          source_revision: observation?.source_revision,
          task_id: observation?.task_id,
          operation_id: observation?.operation_id,
          effective_task_id: linkedTask,
          workspace_id: observation?.workspace_id,
          kind: observation?.kind,
        };
      }).slice(0, limit);
  }

  supersessionRows(values) {
    const [owner, repo, ...rest] = values;
    const limit = rest.at(-1);
    const ids = rest.slice(0, -1);
    return this.rows.supersessions.filter((edge) => edge.owner_id === owner
      && edge.repository_key === repo
      && ids.includes(edge.new_knowledge_id))
      .map((edge) => ({
        ...edge,
        ...this.rows.knowledge.find((item) => item.owner_id === owner
          && item.repository_key === repo
          && item.knowledge_id === edge.old_knowledge_id),
      })).slice(0, limit);
  }

  observationRows(sql, values) {
    if (sql.includes("AS latest_cloud_seq")) return [];
    const rows = [...this.rows.observations];
    const owner = values[0];
    let selected = rows.filter((row) => row.owner_id === owner);
    let index = 1;
    if (sql.includes("WHERE source.owner_id = ? AND source.host_id = ? AND source.session_id = ?")) {
      const host = values[index++];
      const session = values[index++];
      selected = selected.filter((row) => row.host_id === host && row.session_id === session);
    } else {
      const repo = values[index++];
      selected = selected.filter((row) => row.repository_key === repo);
    }
    if (sql.includes("AND source.workspace_id = ?")) {
      const workspace = values[index++];
      selected = selected.filter((row) => row.workspace_id === workspace);
    }
    if (sql.includes("AND (source.task_id = ? OR (source.operation_id IS NOT NULL")) {
      const task = values[index++];
      const linkedTask = values[index++];
      selected = selected.filter((row) => row.task_id === task || this.rows.observations.some((linked) =>
        new Set(this.rows.observations.filter((candidate) => candidate.owner_id === row.owner_id
          && candidate.repository_key === row.repository_key
          && candidate.host_id === row.host_id
          && candidate.session_id === row.session_id
          && candidate.operation_id === row.operation_id
          && candidate.task_id).map((candidate) => candidate.task_id)).size === 1
        && linked.owner_id === row.owner_id
        && linked.repository_key === row.repository_key
        && linked.host_id === row.host_id
        && linked.session_id === row.session_id
        && linked.operation_id === row.operation_id
        && linked.task_id === linkedTask));
    }
    if (sql.includes("instr(lower(source.kind)")) {
      const query = values[index];
      selected = selected.filter((row) => [row.kind, row.action, row.task_id, row.state_status, row.target_backend, row.content_preview]
        .some((field) => typeof field === "string" && field.toLowerCase().includes(query.toLowerCase())));
      index += 6;
    }
    const limit = values.at(-1);
    return selected.sort((a, b) => b.cloud_seq - a.cloud_seq).slice(0, limit);
  }
}

function env(db, extras = {}) {
  return {
    OBSERVATION_DB: db,
    OBSERVATION_OWNER_ID: OWNER,
    MEMORY_ENABLED: "true",
    MEMORY_EXTRACTOR: "fixture",
    MEMORY_QUEUE: { send: async () => {} },
    ...extras,
  };
}

test("repository resolution works without an online host and returns active supported knowledge only", async () => {
  const db = fixtureDb();
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));

  assert.equal(result.handled, true);
  assert.equal(result.value.authority.live, false);
  assert.equal(result.value.authority.observations, "replicated_observed");
  assert.equal(result.value.authority.knowledge, "derived");
  assert.equal(result.value.memory.state, "ready");
  assert.equal(result.value.current_summary.knowledge_summary, "A compact repository summary.");
  assert.deepEqual(result.value.relevant_facts.map((item) => item.knowledge_id), ["fact-a"]);
  assert.deepEqual(result.value.relevant_decisions.map((item) => item.knowledge_id), ["decision-a"]);
  assert.deepEqual(result.value.constraints.map((item) => item.knowledge_id), ["constraint-a"]);
  assert.equal(result.value.constraints[0].supersedes[0].knowledge_id, "old-a");
  assert.equal(result.value.constraints[0].supersession_history[0].text, "Old constraint.");
  assert.equal(result.value.constraints[0].supersession_history[0].support_refs[0].observation_id, "ob-instruction");
  assert.equal(JSON.stringify(result.value).includes("Staged projection"), false);
  assert.equal(JSON.stringify(result.value).includes("Expired fact."), false);
  assert.equal(JSON.stringify(result.value).includes("PRIVATE TASK SECRET"), false);
  assert.equal(result.value.recent_related_tasks.find((task) => task.task_id === "task-a").status, "completed");
  assert.equal(result.value.recent_related_tasks.find((task) => task.task_id === "task-a").instruction_refs[0].observation_id, "ob-instruction");
  assert.equal(result.value.freshness.latest_cloud_seq, 8);
  assert.equal(result.value.memory.worker_lag, 0, "repository B's interleaved cloud sequence is not repository A worker lag");
  assert.deepEqual(result.value.recent_related_tasks.find((task) => task.task_id === "task-a").instruction_refs.map((ref) => ref.observation_id), ["ob-instruction"]);
  assert.deepEqual(result.value.recent_related_tasks.find((task) => task.task_id === "task-b").instruction_refs.map((ref) => ref.observation_id), ["ob-colliding-op-instruction"]);
  assert.equal(result.value.freshness.source_head_revision, null, "repository-wide freshness must not compare different source revision sequences");
  assert.equal(result.value.freshness.source_gap_count, 1);
  assert.equal(result.value.partial.value, true);
});

test("repository context with no owner-scoped sources is partial and stale", async () => {
  const db = fixtureDb({
    sources: [
      source({ owner: "owner-b", host: "foreign-owner-host", session: "foreign-owner-session" }),
      source({ host: "host-b", session: "session-b", repository: REPOSITORY_B }),
    ],
    observations: [],
    knowledge: [],
    supports: [],
    supersessions: [],
  });

  for (const repository of [REPOSITORY_A, "forge:example/repo-typo"]) {
    for (const name of ["context_resolve", "context_status"]) {
      const result = await resolveCloudContext(name, { repository }, env(db));
      assert.equal(result.handled, true);
      assert.equal(result.value.scope.repository, repository);
      assert.equal(result.value.freshness.source_count, 0);
      assert.equal(result.value.freshness.source_cursors.length, 0);
      assert.equal(result.value.freshness.cloud_observation_stale, true);
      assert.equal(result.value.freshness.partial, true);
      if (name === "context_resolve") {
        assert.equal(result.value.partial.value, true);
        assert.equal(result.value.partial.reasons.includes("source_incomplete"), true);
      } else {
        assert.equal(result.value.partial, true);
      }
      assert.doesNotMatch(JSON.stringify(result.value), /foreign-owner-host|session-b/);
    }
  }
});

test("repository context with a healthy owner-scoped source remains fresh", async () => {
  const base = fixtureDb().rows;
  const db = fixtureDb({
    sources: [source({ host: "healthy-host", session: "healthy-session", head: 0, acked: 0, cloud: 0 })],
    observations: [],
    knowledge: [],
    supports: [],
    supersessions: [],
    checkpoints: [{ ...base.checkpoints[0], last_cloud_seq: 0, stale: 0 }],
  });

  for (const name of ["context_resolve", "context_status"]) {
    const result = await resolveCloudContext(name, { repository: REPOSITORY_A }, env(db));
    assert.equal(result.handled, true);
    assert.equal(result.value.freshness.source_count, 1);
    assert.equal(result.value.freshness.cloud_observation_stale, false);
    assert.equal(result.value.freshness.partial, false);
    if (name === "context_resolve") assert.equal(result.value.partial.value, false);
    else assert.equal(result.value.partial, false);
  }
});

test("selected knowledge reports incomplete active and historical support provenance", async () => {
  const db = fixtureDb();
  db.rows.knowledge = db.rows.knowledge.map((item) =>
    ["summary-a", "constraint-a", "old-a"].includes(item.knowledge_id)
      ? { ...item, support_incomplete: 1 }
      : item);
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));

  assert.equal(result.value.current_summary.knowledge_summary, "A compact repository summary.");
  assert.equal(result.value.current_summary.knowledge_summary_support_incomplete, true);
  assert.equal(result.value.relevant_facts[0].support_incomplete, false);
  assert.equal(result.value.constraints[0].support_incomplete, true);
  assert.equal(result.value.constraints[0].supersession_history[0].support_incomplete, true);
  assert.equal(result.value.partial.value, true);
  assert.equal(result.value.partial.reasons.includes("knowledge_support_incomplete"), true);

  const historicalOnlyDb = fixtureDb();
  historicalOnlyDb.rows.knowledge = historicalOnlyDb.rows.knowledge.map((item) => item.knowledge_id === "old-a"
    ? { ...item, support_incomplete: 1 }
    : item);
  const historicalOnly = await resolveCloudContext(
    "context_resolve",
    { repository: REPOSITORY_A },
    env(historicalOnlyDb),
  );
  assert.equal(historicalOnly.value.constraints[0].support_incomplete, false);
  assert.equal(historicalOnly.value.constraints[0].supersession_history[0].support_incomplete, true);
  assert.equal(historicalOnly.value.partial.reasons.includes("knowledge_support_incomplete"), true);
});

test("repository and owner boundaries are applied before observation, support, and knowledge projection", async () => {
  const db = fixtureDb();
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_B }, env(db));
  assert.equal(result.handled, true);
  assert.equal(result.value.relevant_facts.length, 0);
  assert.equal(JSON.stringify(result.value).includes("The repository uses Rust"), false);
  assert.equal(JSON.stringify(result.value).includes("Other owner secret"), false);
  assert.equal(JSON.stringify(result.value).includes("Repository B secret"), false);

  const spoof = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A, owner: "owner-b" }, env(db));
  assert.equal(spoof.handled, true);
  assert.equal(spoof.error, "invalid_params");
  assert.equal(spoof.code, -32602);
});

test("session mapping is owner-scoped, ambiguous mappings fail closed, and missing mappings preserve host fallback", async () => {
  const db = fixtureDb({ sources: [
    source({ host: "host-a", session: "duplicate" }),
    source({ host: "host-b", session: "duplicate" }),
    source({ owner: "owner-b", host: "host-x", session: "foreign" }),
  ], observations: [
    observation({ seq: 1, id: "ob-instruction", session: "duplicate", kind: "instruction", operation: "op-a", preview: "private" }),
    observation({ seq: 2, id: "ob-accepted", session: "duplicate", kind: "operation_accepted", operation: "op-a", task: "task-a" }),
    observation({ seq: 3, id: "ob-state", session: "duplicate", kind: "execution_state", task: "task-a", state: "completed" }),
  ] });
  const ambiguous = await resolveCloudContext("context_resolve", { session_id: "duplicate" }, env(db));
  assert.equal(ambiguous.handled, true);
  assert.equal(ambiguous.error, "session_mapping_ambiguous");
  assert.equal(ambiguous.code, -32006);

  const selected = await resolveCloudContext("context_resolve", { session_id: "duplicate", host_id: "host-a" }, env(db));
  assert.equal(selected.handled, true);
  assert.equal(selected.value.scope.host_id, "host-a");
  assert.equal(selected.value.recent_related_tasks.length, 1);
  assert.equal(selected.value.recent_related_tasks[0].task_id, "task-a");

  const missing = await resolveCloudContext("context_resolve", { session_id: "not-synced" }, env(db));
  assert.deepEqual(missing, { handled: false });
  const foreignOwner = await resolveCloudContext("context_resolve", { session_id: "foreign" }, env(db));
  assert.deepEqual(foreignOwner, { handled: false });
});

test("source revisions are checked only within the selected source and missing minima are marked stale", async () => {
  const db = fixtureDb();
  const repository = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));
  assert.equal(repository.value.freshness.source_head_revision, null);
  assert.equal(repository.value.freshness.source_cursors.length, 2);

  const session = await resolveCloudContext("context_resolve", {
    session_id: "session-a",
    at_least_revision: 6,
  }, env(db));
  assert.equal(session.handled, true);
  assert.equal(session.value.freshness.source_head_revision, 5);
  assert.equal(session.value.freshness.source_acked_revision, 5);
  assert.equal(session.value.freshness.cloud_observation_stale, true);
  assert.equal(session.value.partial.value, true);
});

test("context response budget is enforced and records omissions as partial", async () => {
  const longText = "x".repeat(7000);
  const db = fixtureDb({ knowledge: [knowledge({ id: "large", text: longText })] });
  const result = await resolveCloudContext("context_resolve", {
    repository: REPOSITORY_A,
    budget_bytes: 1024,
  }, env(db));
  assert.equal(result.handled, true);
  const encoded = new TextEncoder().encode(JSON.stringify(result.value)).byteLength;
  assert.ok(encoded <= 1024, `response used ${encoded} bytes`);
  assert.equal(result.value.response_truncated, true);
  assert.equal(result.value.partial.value, true);
});

test("disabled worker state stays distinct from an empty successful projection", async () => {
  const db = fixtureDb();
  const disabled = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db, { MEMORY_ENABLED: "false" }));
  assert.equal(disabled.value.memory.state, "disabled");
  assert.equal(disabled.value.memory.stale, true);
  assert.equal(disabled.value.freshness.knowledge_stale, true);
  assert.equal(disabled.value.relevant_facts[0].knowledge_id, "fact-a", "disabled workers leave the active projection readable");

  const emptyDb = fixtureDb({ knowledge: [] });
  const empty = await resolveCloudContext("context_status", { repository: REPOSITORY_A }, env(emptyDb));
  assert.equal(empty.value.memory.state, "ready_empty");
  assert.equal(empty.value.memory.stale, false);
});

test("failed producer rebuild keeps the previous active projection readable and marks it stale", async () => {
  const db = fixtureDb({
    heads: [{
      ...fixtureDb().rows.heads[0],
      requested_generation: 2,
      requested_producer_version: "producer-next",
      epoch: 8,
    }],
    checkpoints: [
      { ...fixtureDb().rows.checkpoints[0], projection_epoch: 8 },
      {
        owner_id: OWNER,
        repository_key: REPOSITORY_A,
        worker_id: "temote-memory-v1",
        producer_version: "producer-next",
        last_cloud_seq: 0,
        last_success_at: null,
        last_error_at: STAMP,
        last_error_code: "provider_timeout",
        stale: 1,
        projection_epoch: 8,
      },
    ],
  });
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));
  assert.equal(result.value.memory.state, "failed");
  assert.equal(result.value.memory.active_producer_version, PRODUCER);
  assert.equal(result.value.memory.requested_producer_version, "producer-next");
  assert.equal(result.value.freshness.knowledge_stale, true);
  assert.equal(result.value.relevant_facts[0].knowledge_id, "fact-a");
});

test("requested worker failure without an active projection cannot expose staging knowledge", async () => {
  const base = fixtureDb().rows;
  const db = fixtureDb({
    heads: [{
      ...base.heads[0],
      active_generation: null,
      active_producer_version: null,
      requested_generation: 1,
      requested_producer_version: PRODUCER,
      epoch: 8,
    }],
    checkpoints: [{
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      worker_id: "temote-memory-v1",
      producer_version: PRODUCER,
      last_cloud_seq: 0,
      last_success_at: null,
      last_error_at: STAMP,
      last_error_code: "provider_invalid_response",
      stale: 1,
      projection_epoch: 8,
    }],
  });
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));
  assert.equal(result.value.memory.state, "failed");
  assert.equal(result.value.memory.active_producer_version, null);
  assert.equal(result.value.relevant_facts.length, 0);
});

test("a projection checkpoint from an older epoch stays readable but is reported lagging", async () => {
  const base = fixtureDb().rows;
  const db = fixtureDb({
    heads: [{ ...base.heads[0], epoch: 8 }],
    checkpoints: [{ ...base.checkpoints[0], projection_epoch: 7, stale: 0 }],
  });
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A }, env(db));
  assert.equal(result.value.memory.active_generation, 1);
  assert.equal(result.value.memory.requested_generation, 1);
  assert.equal(result.value.memory.state, "lagging");
  assert.equal(result.value.memory.stale, true);
  assert.equal(result.value.relevant_facts[0].knowledge_id, "fact-a", "epoch mismatch does not discard the still-active projection");
});

test("supersession support reads stay within D1's bound-parameter limit", async () => {
  const current = knowledge({ id: "current", kind: "constraint", text: "Current constraint." });
  const oldItems = Array.from({ length: 81 }, (_, index) => knowledge({
    id: `old-${index}`,
    kind: "constraint",
    status: "superseded",
    text: `Historical constraint ${index}.`,
  }));
  const supersessions = oldItems.map((item) => ({
    owner_id: OWNER,
    repository_key: REPOSITORY_A,
    new_knowledge_id: current.knowledge_id,
    old_knowledge_id: item.knowledge_id,
    relationship: "explicit_change",
  }));
  const supports = [
    {
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      knowledge_id: current.knowledge_id,
      observation_cloud_seq: 3,
      observation_id: "ob-state",
      support_role: "verified_constraint",
    },
    ...oldItems.map((item) => ({
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      knowledge_id: item.knowledge_id,
      observation_cloud_seq: 1,
      observation_id: "ob-instruction",
      support_role: "historical_instruction",
    })),
  ];
  const db = fixtureDb({ knowledge: [current, ...oldItems], supports, supersessions });
  const result = await resolveCloudContext("context_resolve", { repository: REPOSITORY_A, budget_bytes: 65536 }, env(db));
  assert.equal(result.value.constraints[0]?.knowledge_id, "current", JSON.stringify(result.value));
  assert.equal(result.value.constraints[0].supersession_history.length, 64);
  assert.ok(db.maxBoundParameterCount <= 100, `observed ${db.maxBoundParameterCount} D1 bind parameters`);
});

test("an instruction-only execution projection is available after a later task acceptance", async () => {
  const db = fixtureDb({
    knowledge: [knowledge({
      id: "operation-constraint",
      scopeType: "execution",
      scopeId: "operation:op-a",
      kind: "constraint",
      text: "Keep this operation within the specified scope.",
    })],
    supports: [{
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      knowledge_id: "operation-constraint",
      observation_cloud_seq: 1,
      observation_id: "ob-instruction",
      support_role: "instruction_quote",
    }],
    supersessions: [],
  });
  const result = await resolveCloudContext("context_resolve", {
    repository: REPOSITORY_A,
    task_id: "task-a",
  }, env(db));
  assert.equal(result.value.constraints.length, 1, JSON.stringify(result.value));
  assert.equal(result.value.constraints[0].scope_type, "execution");
  assert.equal(result.value.constraints[0].support_refs[0].task_id, "task-a");

  const otherTask = await resolveCloudContext("context_resolve", {
    repository: REPOSITORY_A,
    task_id: "task-b",
  }, env(db));
  assert.equal(otherTask.value.constraints.length, 0, "same operation id text in another host/session cannot authorize this support");
});

test("operation IDs associated with multiple tasks in one source are not correlated by first match", async () => {
  const base = fixtureDb().rows;
  const db = fixtureDb({
    observations: [...base.observations, observation({
      seq: 11,
      id: "ob-ambiguous-operation-reuse",
      host: "host-a",
      session: "session-a",
      kind: "operation_accepted",
      operation: "op-a",
      task: "task-other",
    })],
    knowledge: [knowledge({
      id: "ambiguous-operation-item",
      scopeType: "execution",
      scopeId: "operation:op-a",
      kind: "constraint",
      text: "Must not be attached to an ambiguous task operation.",
    })],
    supports: [{
      owner_id: OWNER,
      repository_key: REPOSITORY_A,
      knowledge_id: "ambiguous-operation-item",
      observation_cloud_seq: 1,
      observation_id: "ob-instruction",
      support_role: "instruction_quote",
    }],
    supersessions: [],
  });
  const taskA = await resolveCloudContext("context_resolve", {
    repository: REPOSITORY_A,
    task_id: "task-a",
  }, env(db));
  assert.deepEqual(taskA.value.recent_related_tasks[0].instruction_refs, []);
  assert.equal(taskA.value.constraints.length, 0);
});

test("many task operations are capped before the D1 bound-parameter ceiling", async () => {
  const base = fixtureDb().rows;
  const bulkOperations = Array.from({ length: 81 }, (_, index) => observation({
    seq: 20 + index,
    id: `ob-bulk-op-${index}`,
    host: "host-a",
    session: "session-a",
    kind: "operation_accepted",
    operation: `bulk-op-${index}`,
    task: "task-a",
  }));
  const db = fixtureDb({
    observations: [...base.observations, ...bulkOperations],
    knowledge: [],
    supports: [],
    supersessions: [],
  });
  const result = await resolveCloudContext("context_resolve", {
    repository: REPOSITORY_A,
    task_id: "task-a",
  }, env(db));
  assert.equal(result.value.partial.reasons.includes("knowledge_truncated"), true);
  assert.ok(db.maxBoundParameterCount <= 100, `observed ${db.maxBoundParameterCount} D1 bind parameters`);
});
