import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import test from "node:test";

import {
  loadMemoryConfiguration,
} from "../src/memory/config.js";
import {
  boundedInput,
  extractKnowledge,
  toExtractorObservation,
} from "../src/memory/extractor.js";
import {
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  MEMORY_TEST_REPOSITORY,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const PROVIDER_ENDPOINT = "https://extractor.example/v1/chat/completions";
const FIXTURE_CONFIG = await loadMemoryConfiguration({
  MEMORY_ENABLED: "true",
  MEMORY_EXTRACTOR: "fixture",
});
const PROVIDER_CONFIG = await loadMemoryConfiguration({
  MEMORY_ENABLED: "true",
  MEMORY_EXTRACTOR: "openai_compatible",
  MEMORY_ENDPOINT: PROVIDER_ENDPOINT,
  MEMORY_MODEL: "budget-test-model",
  MEMORY_API_KEY: "test-only-provider-key",
});
// The workerd harness defaults MEMORY_INPUT_BUDGET_BYTES to 8192, so the
// extraction input window is 8192 - PROMPT_OVERHEAD_BYTES(4096) = 4096 bytes.
// Unit tests pass the same window so they exercise the identical boundary.
const BUDGET = 4096;

const POLICY_JSON = "Report output format must be JSON.";
const POLICY_TEXT = [
  "For this repository, the repository-level policy is:",
  POLICY_JSON,
].join("\n");

let observationCounter = 0;
function observation(overrides = {}) {
  observationCounter += 1;
  return {
    cloud_seq: observationCounter,
    owner_id: "owner-budget",
    host_id: "host-budget",
    session_id: "session-budget",
    observation_id: `observation-budget-${observationCounter}`,
    source_revision: observationCounter,
    repository_key: "github:temote-tests/budget",
    workspace_id: null,
    task_id: "task-budget",
    execution_id: null,
    operation_id: null,
    kind: "instruction",
    action: "task_start",
    target_backend: "codex",
    content_kind: "text",
    content_preview: "nothing",
    state_status: null,
    state_revision: null,
    evidence_refs: "[]",
    observed_at: 1_790_000_000,
    ...overrides,
  };
}

// Sized so the filler item leaves enough room for the tail observation's
// metadata (pre-fix selects and truncates it) but not its full extraction
// input (post-fix defers it to the next batch). The two builders differ in
// field lengths, so each gets its own filler size.
const UNIT_FILLER_PREVIEW = "Filler observation body repeats. ".repeat(94).slice(0, 2700);
const D1_FILLER_PREVIEW = "Filler observation body repeats. ".repeat(94).slice(0, 2500);

test("a tail observation whose full input does not fit is deferred, not emptied", async () => {
  const filler = observation({ cloud_seq: 1, content_preview: UNIT_FILLER_PREVIEW });
  const policy = observation({ cloud_seq: 2, content_preview: POLICY_TEXT });

  const alone = boundedInput([policy], BUDGET);
  assert.equal(alone.observations.length, 1, "the policy observation fits a fresh batch");
  assert.equal(alone.observations[0].content_preview, POLICY_TEXT);
  assert.equal(alone.observations[0].allowed_repository_clauses.length, 1);

  const bounded = boundedInput([filler, policy], BUDGET);
  assert.equal(bounded.observations.length, 1,
    "the tail policy observation must wait for the next batch instead of losing its preview");
  assert.equal(bounded.observations[0].cloud_seq, 1);
  assert.equal(bounded.observations[0].content_preview, UNIT_FILLER_PREVIEW,
    "the committed observation keeps its complete sanitized preview");
});

test("deferral never splits UTF-8 multibyte text or JSON escaping", async () => {
  const multibyte = "日本語の方針テキスト。「引用符」と\\パス区切りを含む。".repeat(12);
  const filler = observation({ cloud_seq: 1, content_preview: UNIT_FILLER_PREVIEW.slice(0, 2300) });
  const tail = observation({ cloud_seq: 2, content_preview: multibyte });

  const bounded = boundedInput([filler, tail], BUDGET);
  assert.equal(bounded.observations.length, 1);

  const alone = boundedInput([tail], BUDGET);
  assert.equal(alone.observations.length, 1);
  assert.equal(alone.observations[0].content_preview, multibyte,
    "multibyte and escape-heavy previews pass through byte-exact or not at all");
});

test("clause and predecessor metadata count toward the batch fit", async () => {
  const changedPolicy = [
    "For this repository, the repository-level policy has changed:",
    "Report output format must be TOML.",
    `Previous repository-level policy to replace: ${POLICY_JSON}`,
  ].join("\n");
  const filler = observation({ cloud_seq: 1, content_preview: UNIT_FILLER_PREVIEW.slice(0, 2550) });
  const change = observation({ cloud_seq: 2, content_preview: changedPolicy });

  const bounded = boundedInput([filler, change], BUDGET);
  assert.equal(bounded.observations.length, 1,
    "canonical clause/predecessor payload is part of the measured input, not free");

  const alone = boundedInput([change], BUDGET);
  assert.equal(alone.observations.length, 1);
  const selected = alone.observations[0];
  assert.equal(selected.allowed_repository_clauses.length, 1);
  assert.equal(selected.allowed_repository_clauses[0].changed, true);
  assert.deepEqual(selected.repository_change_predecessors, [POLICY_JSON]);
});

test("an observation that cannot fit alone selects nothing for bounded failure", async () => {
  const oversized = observation({
    cloud_seq: 1,
    content_preview: "x".repeat(BUDGET + 512),
  });
  const bounded = boundedInput([oversized], BUDGET);
  assert.equal(bounded.observations.length, 0,
    "oversized input stays an explicit bounded failure, not a silent skip");
});

test("an observation that never had a preview keeps its existing selection behavior", async () => {
  const stateOnly = observation({
    cloud_seq: 1,
    kind: "execution_state",
    action: "task_get",
    content_kind: "none",
    content_preview: null,
    state_status: "running",
    state_revision: 1,
  });
  const bounded = boundedInput([stateOnly], BUDGET);
  assert.equal(bounded.observations.length, 1);
  assert.equal(bounded.observations[0].content_preview, null,
    "a null preview is never fabricated");

  const result = await extractKnowledge(bounded.observations, FIXTURE_CONFIG, "budget-null-preview");
  assert.equal(result.inputCount, 1);
});

test("the serialized provider request stays inside the configured input budget", async () => {
  const filler = observation({ cloud_seq: 1, content_preview: UNIT_FILLER_PREVIEW });
  const policy = observation({ cloud_seq: 2, content_preview: POLICY_TEXT });
  const bounded = boundedInput([filler, policy], BUDGET);
  assert.equal(bounded.observations.length, 1);

  let requestBody;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input, init) => {
    requestBody = JSON.parse(init.body);
    return Response.json({
      choices: [{ finish_reason: "stop", message: { content: JSON.stringify({ items: [] }) } }],
    });
  };
  try {
    const result = await extractKnowledge(bounded.observations, PROVIDER_CONFIG, "budget-request");
    assert.equal(result.inputCount, 1);
  } finally {
    globalThis.fetch = originalFetch;
  }
  const userContent = requestBody.messages.find((message) => message.role === "user").content;
  const payloadStart = userContent.lastIndexOf("\n{");
  const payloadBytes = Buffer.byteLength(userContent.slice(payloadStart + 1));
  assert.ok(payloadBytes <= BUDGET,
    "the serialized observation payload must remain inside the extraction input window");
  assert.ok(Buffer.byteLength(userContent) <= PROVIDER_CONFIG.inputBudgetBytes,
    "the final provider request must remain inside the configured input budget");
});

function syncBody(sessionId, records, headRevision) {
  return JSON.stringify({
    schema_version: 1,
    session_id: sessionId,
    repository_key: MEMORY_TEST_REPOSITORY,
    source_base_revision: 0,
    source_head_revision: headRevision,
    journal_degraded: false,
    gap_count: 0,
    records,
  });
}

function record(revision, observation) {
  return { source_revision: revision, observation };
}

function instructionObservation(sessionId, revision, preview) {
  const now = Math.floor(Date.now() / 1000) - 120;
  return {
    id: randomUUID(),
    schema_version: 1,
    observed_at: now,
    session_id: sessionId,
    session_instance: { started_at: now, process_id: 1 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "codex" },
    action: "task_start",
    kind: "instruction",
    task_id: `task-${sessionId}`,
    operation_id: `operation-${sessionId}`,
    content: {
      kind: "text",
      preview,
      total_bytes: Buffer.byteLength(preview),
      sha256: createHash("sha256").update(preview).digest("hex"),
      truncated: false,
    },
    evidence_refs: [],
    provenance: { tool: "codex_task_start", source: "orchestration" },
    revision,
    dedupe_key: `${sessionId}:${revision}`,
  };
}

async function sync(runtime, sessionId, observations) {
  const records = observations.map((observation) => record(observation.revision, observation));
  const response = await runtime.fetch(
    `https://memory-test.local/v1/hosts/${MEMORY_TEST_HOST_ID}/observations/sync`,
    {
      method: "POST",
      headers: {
        authorization: `Bearer ${MEMORY_TEST_HOST_TOKEN}`,
        "content-type": "application/json",
        "x-temote-host-id": MEMORY_TEST_HOST_ID,
      },
      body: syncBody(sessionId, records, Math.max(...observations.map((observation) => observation.revision))),
    },
  );
  assert.equal(response.status, 200, "observations must be durably replicated");
  return response.json();
}

async function waitForCheckpoint(runtime, seq) {
  await runtime.waitFor(async () => {
    const rows = await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    return rows.length > 0 && Number(rows[0].last_cloud_seq) >= Number(seq);
  }, { timeoutMs: 15_000, intervalMs: 25 });
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

async function assertPolicyProjected(runtime) {
  const constraints = await runtime.querySql(
    "SELECT text, status FROM knowledge_items WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint'",
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
  );
  assert.ok(constraints.some((item) => item.text === POLICY_JSON && item.status === "current"),
    "the deferred policy must still be extracted and reach current");
  const context = await repositoryContext(runtime);
  assert.ok(context.constraints.some((item) => item.text === POLICY_JSON && item.status === "current"),
    "context_resolve must surface the policy committed by the follow-up batch");
}

test("tail-budget deferral commits the prefix and extracts the policy on the next batch", {
  timeout: 45_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const sessionId = "budget-tail-session";
    const synced = await sync(runtime, sessionId, [
      instructionObservation(sessionId, 1, D1_FILLER_PREVIEW),
      instructionObservation(sessionId, 2, POLICY_TEXT),
    ]);
    await waitForCheckpoint(runtime, synced.cloud_head_seq);

    const stored = await runtime.querySql(
      "SELECT cloud_seq, content_preview FROM observations WHERE owner_id = ? AND repository_key = ? ORDER BY cloud_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(stored[1].content_preview, POLICY_TEXT,
      "D1 keeps the complete sanitized preview; extraction may not consume a truncated copy");

    const runs = await runtime.querySql(
      "SELECT from_seq, to_seq, input_count, outcome, status, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY to_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(runs.length, 2, "the deferred observation must produce a follow-up run");
    assert.equal(runs[0].to_seq, 1,
      "the first run commits only the fully-fitting prefix of the batch");
    assert.equal(runs[0].status, "completed");
    assert.equal(runs[1].to_seq, 2);
    assert.equal(runs[1].status, "completed");

    await assertPolicyProjected(runtime);

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
    const runsAfter = await runtime.querySql(
      "SELECT COUNT(*) AS count FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(runsAfter[0].count, 2,
      "queue replay must not re-consume or drop the already committed observation");
    await assertPolicyProjected(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("the real provider adapter path defers and re-extracts the same way", {
  timeout: 45_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "budget-provider-test",
    memoryApiKey: "test-only-provider-key",
    memoryTestProviderMode: "echo_allowed_repository_clauses",
    maxQueueRetries: 0,
  });
  try {
    const sessionId = "budget-provider-session";
    const synced = await sync(runtime, sessionId, [
      instructionObservation(sessionId, 1, D1_FILLER_PREVIEW),
      instructionObservation(sessionId, 2, POLICY_TEXT),
    ]);
    await waitForCheckpoint(runtime, synced.cloud_head_seq);

    const runs = await runtime.querySql(
      "SELECT from_seq, to_seq, input_count, outcome, status FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY to_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(runs.length, 2);
    assert.equal(runs[0].to_seq, 1);
    assert.equal(runs[1].to_seq, 2);
    await assertPolicyProjected(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("a larger input budget yields the same final policy in one batch", {
  timeout: 45_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: { MEMORY_INPUT_BUDGET_BYTES: "16384" },
  });
  try {
    const sessionId = "budget-wide-session";
    const synced = await sync(runtime, sessionId, [
      instructionObservation(sessionId, 1, D1_FILLER_PREVIEW),
      instructionObservation(sessionId, 2, POLICY_TEXT),
    ]);
    await waitForCheckpoint(runtime, synced.cloud_head_seq);
    const runs = await runtime.querySql(
      "SELECT to_seq, status FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY to_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(runs.length, 1, "both observations fit a single batch under the larger budget");
    assert.equal(runs[0].to_seq, 2);
    await assertPolicyProjected(runtime);
  } finally {
    await runtime.dispose();
  }
});

test("an oversized-alone observation fails explicitly without advancing the checkpoint", {
  timeout: 45_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: { MEMORY_MAX_ATTEMPTS: "2" },
  });
  try {
    const sessionId = "budget-oversized-session";
    // The preview itself stays inside the ingest limit, but its serialized
    // extraction item (preview + canonical clause metadata) exceeds the input
    // window even alone — the existing bounded-failure contract applies.
    const preview = Array.from({ length: 30 }, (_, index) => [
      "For this repository, the repository-level policy is:",
      `Report item ${index + 1} format must be JSON.`,
    ].join("\n")).join("\n");
    const synced = await sync(runtime, sessionId, [
      instructionObservation(sessionId, 1, preview),
    ]);
    // The first attempt already records the explicit bounded failure; a
    // retry through the real scheduled outbox sweep must not advance the
    // checkpoint either. Clearing queued_at models the redelivery window
    // (OUTBOX_REDELIVERY_SECONDS) having elapsed — the same UPDATE the
    // production deferral path writes after a commit.
    await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, error_code, attempt_count FROM memory_runs WHERE owner_id = ? AND repository_key = ? AND error_code = 'input_too_large'",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length > 0;
    }, { timeoutMs: 15_000, intervalMs: 25 });
    await runtime.executeSql(
      "UPDATE memory_outbox SET queued_at = NULL, next_attempt_at = 0 WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    const sweep = await runtime.runScheduled();
    assert.equal(sweep.success, true);
    await runtime.waitFor(async () => {
      const runs = await runtime.querySql(
        "SELECT status, error_code, attempt_count FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return runs.length > 0
        && runs[0].status === "failed"
        && runs[0].error_code === "input_too_large"
        && Number(runs[0].attempt_count) >= 2;
    }, { timeoutMs: 15_000, intervalMs: 25 });

    const checkpoints = await runtime.querySql(
      "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(Number(checkpoints[0]?.last_cloud_seq ?? 0), 0,
      "the checkpoint must not advance past an observation that never produced extraction input");
    const knowledge = await runtime.querySql(
      "SELECT COUNT(*) AS count FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(knowledge[0].count, 0);
    const stored = await runtime.querySql(
      "SELECT LENGTH(content_preview) AS preview_bytes FROM observations WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(stored[0].preview_bytes, Buffer.byteLength(preview),
      "the retained observation keeps its full preview for a future larger budget or rebuild");
    assert.equal(synced.cloud_head_seq, 1);
  } finally {
    await runtime.dispose();
  }
});

test("an observation that never stored a preview still processes normally", {
  timeout: 45_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
  });
  try {
    const sessionId = "budget-null-preview-session";
    const now = Math.floor(Date.now() / 1000) - 120;
    const observation = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: now,
      session_id: sessionId,
      session_instance: { started_at: now, process_id: 1 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_get",
      kind: "execution_state",
      task_id: `task-${sessionId}`,
      execution_id: `execution-${sessionId}`,
      operation_id: randomUUID(),
      content: { kind: "none" },
      state_ref: { task_id: `task-${sessionId}`, status: "running", revision: 1, generation: 1 },
      evidence_refs: [],
      provenance: { tool: "codex_task_get", source: "orchestration" },
      revision: 1,
      dedupe_key: `${sessionId}:1`,
    };
    const synced = await sync(runtime, sessionId, [observation]);
    await waitForCheckpoint(runtime, synced.cloud_head_seq);
    const runs = await runtime.querySql(
      "SELECT status, outcome FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.deepEqual(runs.map((run) => run.status), ["completed"],
      "a natively preview-less observation completes as before");
  } finally {
    await runtime.dispose();
  }
});

const POLICY_TOML = "Report output format must be TOML.";
const POLICY_STAGING = "Deployment target must be staging.";
// MEMORY_INPUT_BUDGET_BYTES=16384 yields extraction input window
// 16384 - PROMPT_OVERHEAD_BYTES(4096) = 12288; MEMORY_BATCH_SIZE=32 admits
// the whole corpus by bytes and count.
const ITEM_BOUND_WINDOW = 12288;
const ITEM_BOUND_BINDINGS = {
  MEMORY_INPUT_BUDGET_BYTES: "16384",
  MEMORY_BATCH_SIZE: "32",
};

function changedPolicyText(quote) {
  return [
    "For this repository, the repository-level policy has changed:",
    quote,
  ].join("\n");
}

// Same shape as instructionObservation but with short ids so the serialized
// extraction payload byte-fits the window — the item bound, not the byte
// bound, must be what splits the corpus.
function compactInstructionObservation(sessionId, revision, preview) {
  const now = Math.floor(Date.now() / 1000) - 120;
  return {
    id: randomUUID(),
    schema_version: 1,
    observed_at: now,
    session_id: sessionId,
    session_instance: { started_at: now, process_id: 1 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "codex" },
    action: "task_start",
    kind: "instruction",
    task_id: `t-${sessionId}`,
    operation_id: `op-${sessionId}`,
    content: {
      kind: "text",
      preview,
      total_bytes: Buffer.byteLength(preview),
      sha256: createHash("sha256").update(preview).digest("hex"),
      truncated: false,
    },
    evidence_refs: [],
    provenance: { tool: "codex_task_start", source: "orchestration" },
    revision,
    dedupe_key: `${sessionId}:${revision}`,
  };
}

function changedPolicySession(sessionId, count, extraClauses = new Map()) {
  return Array.from({ length: count }, (_, index) => {
    const parts = [changedPolicyText(index + 1 === count ? POLICY_JSON : POLICY_TOML)];
    if (extraClauses.has(index + 1)) parts.push(...extraClauses.get(index + 1));
    return compactInstructionObservation(sessionId, index + 1, parts.join("\n"));
  });
}

test("a byte-fitting backlog is still bounded by the extraction item limit", () => {
  const observations = Array.from({ length: 13 }, (_, index) => observation({
    cloud_seq: index + 1,
    source_revision: index + 1,
    content_preview: changedPolicyText(index + 1 === 13 ? POLICY_JSON : POLICY_TOML),
  }));
  const payloadBytes = Buffer.byteLength(JSON.stringify({
    observations: observations.map(toExtractorObservation),
  }));
  assert.ok(payloadBytes <= ITEM_BOUND_WINDOW,
    `all 13 observations byte-fit the extraction window (measured ${payloadBytes} bytes)`);
  const bounded = boundedInput(observations, ITEM_BOUND_WINDOW);
  assert.equal(bounded.observations.length, 12,
    "the deterministic item bound splits what the byte budget admits");
  assert.equal(bounded.observations.at(-1).cloud_seq, 12);
});

test("the extraction item bound is exact at twelve items", () => {
  for (const count of [11, 12]) {
    const observations = Array.from({ length: count }, (_, index) => observation({
      cloud_seq: index + 1,
      source_revision: index + 1,
      content_preview: changedPolicyText(index + 1 === count ? POLICY_JSON : POLICY_TOML),
    }));
    const bounded = boundedInput(observations, ITEM_BOUND_WINDOW);
    assert.equal(bounded.observations.length, count,
      `${count} changed clauses stay inside the item bound`);
  }
});

test("a multi-clause observation defers whole at the item bound", () => {
  const observations = Array.from({ length: 11 }, (_, index) => observation({
    cloud_seq: index + 1,
    source_revision: index + 1,
    content_preview: changedPolicyText(POLICY_TOML),
  }));
  observations.push(observation({
    cloud_seq: 12,
    source_revision: 12,
    content_preview: [changedPolicyText(POLICY_TOML), changedPolicyText(POLICY_STAGING)].join("\n"),
  }));
  observations.push(observation({
    cloud_seq: 13,
    source_revision: 13,
    content_preview: changedPolicyText(POLICY_JSON),
  }));
  const bounded = boundedInput(observations, ITEM_BOUND_WINDOW);
  assert.equal(bounded.observations.length, 11,
    "an observation whose clauses would cross the bound defers whole, never partially");
});

async function memoryRuns(runtime) {
  return runtime.querySql(
    `SELECT from_seq, to_seq, input_count, outcome, status, error_code
     FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY to_seq, rowid`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
  );
}

async function assertConvergedBacklog(runtime, tomSupportSeqs) {
  const constraints = await runtime.querySql(
    `SELECT knowledge_id, text, status FROM knowledge_items
     WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' AND scope_type = 'repository'`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
  );
  const json = constraints.find((item) => item.text === POLICY_JSON);
  const tom = constraints.find((item) => item.text === POLICY_TOML);
  assert.ok(json, "the final changed policy is projected");
  assert.equal(json.status, "current");
  assert.ok(tom, "the superseded TOML policy remains as history");
  assert.equal(tom.status, "superseded");
  const supportSeqs = async (knowledgeId) => (await runtime.querySql(
    `SELECT observation_cloud_seq FROM knowledge_support
     WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ? ORDER BY observation_cloud_seq`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, knowledgeId],
  )).map((row) => Number(row.observation_cloud_seq));
  assert.deepEqual(await supportSeqs(tom.knowledge_id), tomSupportSeqs,
    "every earlier TOML revision remains attached as support");
  assert.deepEqual(await supportSeqs(json.knowledge_id), [tomSupportSeqs.length + 1],
    "the last observation supports the promoted JSON policy");
  const edges = await runtime.querySql(
    `SELECT new_knowledge_id, old_knowledge_id FROM knowledge_supersession
     WHERE owner_id = ? AND repository_key = ?`,
    [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
  );
  const promotionEdges = edges.filter((edge) => edge.new_knowledge_id === json.knowledge_id);
  assert.equal(promotionEdges.length, 1,
    "one explicit change supersedes the TOML policy");
  assert.equal(promotionEdges[0].old_knowledge_id, tom.knowledge_id);
  assert.ok(edges.every((edge) => edge.new_knowledge_id !== edge.old_knowledge_id),
    "supersession must never be self-referential");
  const context = await repositoryContext(runtime);
  assert.ok(context.constraints.some((item) => item.text === POLICY_JSON && item.status === "current"),
    "context_resolve surfaces the promoted policy");
  assert.equal(context.current_summary.knowledge_summary, POLICY_JSON);
}

test("the extraction-item bound splits a byte-fitting backlog across runs (fixture)", {
  timeout: 60_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: ITEM_BOUND_BINDINGS,
  });
  try {
    const sessionId = "ibf";
    const synced = await sync(runtime, sessionId, changedPolicySession(sessionId, 13));
    assert.equal(Number(synced.cloud_head_seq), 13);
    const stored = await runtime.querySql(
      "SELECT * FROM observations WHERE owner_id = ? AND repository_key = ? ORDER BY cloud_seq",
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]);
    const payloadBytes = Buffer.byteLength(JSON.stringify({
      observations: stored.map(toExtractorObservation),
    }));
    assert.ok(payloadBytes <= ITEM_BOUND_WINDOW,
      `the whole corpus must byte-fit the input window (got ${payloadBytes})`);
    await waitForCheckpoint(runtime, synced.cloud_head_seq);

    const runs = await memoryRuns(runtime);
    assert.equal(runs.length, 2, "the backlog completes over two runs, not one silent claim");
    assert.deepEqual(runs.map((run) => run.status), ["completed", "completed"]);
    assert.equal(runs[0].from_seq, 0);
    assert.equal(runs[0].to_seq, 12);
    assert.equal(runs[0].input_count, 12,
      "the first run claims only the processable prefix");
    assert.equal(runs[1].to_seq, 13);
    assert.equal(runs[1].input_count, 1);
    assert.ok(runs.every((run) => run.error_code == null),
      "normal backlog splitting is not a bounded failure");
    await assertConvergedBacklog(runtime, Array.from({ length: 12 }, (_, index) => index + 1));

    const queueBefore = await runtime.queueDispatchCount();
    await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: 13,
    });
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > queueBefore && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });
    assert.equal((await memoryRuns(runtime)).length, 2,
      "replaying the committed range starts no new runs");
    await assertConvergedBacklog(runtime, Array.from({ length: 12 }, (_, index) => index + 1));
  } finally {
    await runtime.dispose();
  }
});

test("the provider adapter path splits canonical items across runs", {
  timeout: 60_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryEndpoint: "https://memory-provider.invalid/v1/chat/completions",
    memoryModel: "item-bound-provider-test",
    memoryApiKey: "test-only-provider-key",
    memoryTestProviderMode: "echo_allowed_repository_clauses",
    maxQueueRetries: 0,
    bindings: ITEM_BOUND_BINDINGS,
  });
  try {
    const sessionId = "ibp";
    const synced = await sync(runtime, sessionId, changedPolicySession(sessionId, 13));
    await waitForCheckpoint(runtime, synced.cloud_head_seq);

    const runs = await memoryRuns(runtime);
    assert.equal(runs.length, 2,
      "the deterministic bound splits before projection_too_large can wedge the run");
    assert.deepEqual(runs.map((run) => run.status), ["completed", "completed"]);
    assert.equal(runs[0].to_seq, 12);
    assert.equal(runs[0].input_count, 12);
    assert.equal(runs[1].to_seq, 13);
    assert.equal(runs[1].input_count, 1);
    await assertConvergedBacklog(runtime, Array.from({ length: 12 }, (_, index) => index + 1));
  } finally {
    await runtime.dispose();
  }
});

test("split item-bound backlogs converge on the same final policy", {
  timeout: 90_000,
}, async () => {
  const splits = [
    { name: "tail", chunks: [12, 1], expectToSeqs: [12, 13] },
    { name: "middle", chunks: [6, 7], expectToSeqs: [6, 13] },
    { name: "each", chunks: Array.from({ length: 13 }, () => 1), expectToSeqs: Array.from({ length: 13 }, (_, i) => i + 1) },
  ];
  for (const split of splits) {
    const runtime = await startMemoryRuntime({
      memoryExtractor: "fixture",
      memoryEnabled: true,
      maxQueueRetries: 0,
      bindings: ITEM_BOUND_BINDINGS,
    });
    try {
      const sessionId = `ibs-${split.name}`;
      let head = 0;
      let offset = 0;
      for (const chunk of split.chunks) {
        const observations = changedPolicySession(sessionId, 13)
          .slice(offset, offset + chunk)
          .map((observation, index) => ({
            ...observation,
            revision: offset + index + 1,
          }));
        const synced = await sync(runtime, sessionId, observations);
        head = synced.cloud_head_seq;
        await waitForCheckpoint(runtime, head);
        offset += chunk;
      }
      assert.equal(Number(head), 13);
      const runs = await memoryRuns(runtime);
      assert.deepEqual(runs.map((run) => Number(run.to_seq)), split.expectToSeqs,
        `${split.name} split consumes the whole backlog`);
      assert.ok(runs.every((run) => run.status === "completed" && run.error_code == null));
      await assertConvergedBacklog(runtime, Array.from({ length: 12 }, (_, index) => index + 1));
    } finally {
      await runtime.dispose();
    }
  }
});

test("the extraction item bound is exact at the 11/12/13 boundary in D1", {
  timeout: 90_000,
}, async () => {
  const expectations = [
    { count: 11, toSeqs: [11] },
    { count: 12, toSeqs: [12] },
    { count: 13, toSeqs: [12, 13] },
  ];
  for (const expectation of expectations) {
    const runtime = await startMemoryRuntime({
      memoryExtractor: "fixture",
      memoryEnabled: true,
      maxQueueRetries: 0,
      bindings: ITEM_BOUND_BINDINGS,
    });
    try {
      const sessionId = `ibb-${expectation.count}`;
      const synced = await sync(
        runtime,
        sessionId,
        changedPolicySession(sessionId, expectation.count),
      );
      await waitForCheckpoint(runtime, synced.cloud_head_seq);
      const runs = await memoryRuns(runtime);
      assert.deepEqual(runs.map((run) => Number(run.to_seq)), expectation.toSeqs,
        `${expectation.count} items split at the exact extraction bound`);
      await assertConvergedBacklog(
        runtime,
        Array.from({ length: expectation.count - 1 }, (_, index) => index + 1),
      );
    } finally {
      await runtime.dispose();
    }
  }
});

test("a multi-clause observation defers whole and re-extracts on the next run", {
  timeout: 60_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: ITEM_BOUND_BINDINGS,
  });
  try {
    const sessionId = "ibm";
    const extras = new Map([[12, [changedPolicyText(POLICY_STAGING)]]]);
    const synced = await sync(runtime, sessionId, changedPolicySession(sessionId, 13, extras));
    await waitForCheckpoint(runtime, synced.cloud_head_seq);

    const runs = await memoryRuns(runtime);
    assert.equal(runs.length, 2);
    assert.equal(runs[0].to_seq, 11,
      "the two-clause observation defers whole instead of being consumed halfway");
    assert.equal(runs[1].to_seq, 13);
    const staging = await runtime.querySql(
      `SELECT knowledge_id, status FROM knowledge_items
       WHERE owner_id = ? AND repository_key = ? AND kind = 'constraint' AND text = ?`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, POLICY_STAGING],
    );
    assert.equal(staging.length, 1);
    const stagingSupport = await runtime.querySql(
      `SELECT observation_cloud_seq FROM knowledge_support
       WHERE owner_id = ? AND repository_key = ? AND knowledge_id = ?`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY, staging[0].knowledge_id],
    );
    assert.deepEqual(stagingSupport.map((row) => Number(row.observation_cloud_seq)), [12],
      "the deferred observation's second clause is extracted in full, not partially");
    const edges = await runtime.querySql(
      `SELECT COUNT(*) AS count FROM knowledge_supersession WHERE owner_id = ? AND repository_key = ?`,
      [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
    );
    assert.equal(Number(edges[0].count), 2,
      "TOML -> staging -> JSON supersedes in source order, exactly once each");
    const context = await repositoryContext(runtime);
    assert.ok(context.constraints.some((item) => item.text === POLICY_JSON && item.status === "current"));
    assert.equal(context.current_summary.knowledge_summary, POLICY_JSON);
  } finally {
    await runtime.dispose();
  }
});

test("batch-size splitting of the same backlog reaches the same final policy", {
  timeout: 60_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    maxQueueRetries: 0,
    bindings: { ...ITEM_BOUND_BINDINGS, MEMORY_BATCH_SIZE: "6" },
  });
  try {
    const sessionId = "ibsb";
    const synced = await sync(runtime, sessionId, changedPolicySession(sessionId, 13));
    await waitForCheckpoint(runtime, synced.cloud_head_seq);
    const runs = await memoryRuns(runtime);
    assert.deepEqual(runs.map((run) => Number(run.to_seq)), [6, 12, 13],
      "a smaller batch size processes contiguous complete prefixes");
    await assertConvergedBacklog(runtime, Array.from({ length: 12 }, (_, index) => index + 1));
  } finally {
    await runtime.dispose();
  }
});
