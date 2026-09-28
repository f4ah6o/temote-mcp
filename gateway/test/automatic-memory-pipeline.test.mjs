import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { EventEmitter } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  hostIsPresent,
  memoryExtractorSettings,
  projectSafeContextStatus,
  resolveExtractorReasoningEffort,
  safeHarnessErrorCode,
  stopDedicatedHostAndConfirmOffline,
  taskTerminalWaitBudgetMs,
  waitForHost,
  waitForTaskBTerminalBeforeProjection,
  writeLiveFailureManifest,
} from "../scripts/memory-dogfood.mjs";
import {
  MEMORY_TEST_CLIENT_TOKEN,
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  MEMORY_TEST_REPOSITORY,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const POLICY_QUOTE = "Report output format must be JSON.";
const UNRESOLVED_CLAUSE = "Open question: The required report field set remains undecided.";
const UNRESOLVED_TEXT = "The required report field set remains undecided.";
const CONTENT = [
  "For this repository, the repository-level policy is:",
  POLICY_QUOTE,
  UNRESOLVED_CLAUSE,
  "Implement report.py with render_report(fields), preserve the passed field mapping as-is, add one focused test, and do not change other files.",
].join("\n");
const SESSION = "automatic-memory-session";
const TASK = "automatic-memory-task";
const OPERATION = "550e8400-e29b-41d4-a716-446655440000";

function hostListRuntime(value, { status = 200, error = undefined, onCall = undefined } = {}) {
  return {
    async callMcp() {
      onCall?.();
      return {
        status,
        body: error ? { error } : {
          result: { content: [{ type: "text", text: JSON.stringify(value) }] },
        },
      };
    },
  };
}

class FakeChild extends EventEmitter {
  constructor({ exitOn = [], handledSignals = [] } = {}) {
    super();
    this.exitCode = null;
    this.signalCode = null;
    this.signals = [];
    this.exitOn = new Set(exitOn);
    this.handledSignals = new Set(handledSignals);
  }

  kill(signal) {
    this.signals.push(signal);
    if (this.exitOn.has(signal)) {
      queueMicrotask(() => {
        if (this.handledSignals.has(signal)) this.exitCode = 0;
        else this.signalCode = signal;
        this.emit("exit", null, signal);
      });
    }
    return true;
  }
}

test("host_list offline proof reads only a validated top-level online-host array", async () => {
  assert.equal(await hostIsPresent(hostListRuntime([{ host_id: "host-a" }]), "token", "host-a"), true);
  assert.equal(await hostIsPresent(hostListRuntime([{ host_id: "host-b" }]), "token", "host-a"), false);
  assert.equal(await hostIsPresent(hostListRuntime([
    { host_id: "host-b", retained_session: { host_id: "host-a" } },
  ]), "token", "host-a"), false, "nested historical host references must not count as online");
  await assert.rejects(
    hostIsPresent(hostListRuntime({ hosts: [{ host_id: "host-a" }] }), "token", "host-a"),
    (error) => error.code === "HOST_LIST_RESULT_INVALID",
  );
  await assert.rejects(
    hostIsPresent(hostListRuntime([{ session: { host_id: "host-a" } }]), "token", "host-a"),
    (error) => error.code === "HOST_LIST_RESULT_INVALID",
  );
});

test("host_list discovery errors and malformed replies never prove a host offline", async () => {
  await assert.rejects(
    waitForHost(hostListRuntime(null, { status: 401, error: "unauthorized" }), "token", "host-a", false, 5, 1),
    (error) => error.code === "HOST_LIST_UNAVAILABLE",
  );
  await assert.rejects(
    waitForHost(hostListRuntime({ hosts: [] }), "token", "host-a", false, 5, 1),
    (error) => error.code === "HOST_LIST_UNAVAILABLE",
  );
  await assert.rejects(
    waitForHost(hostListRuntime([{ host_id: "host-a" }]), "token", "host-a", false, 5, 1),
    (error) => error.code === "DEDICATED_HOST_OFFLINE_TIMEOUT",
  );
});

test("task terminal budget covers bounded MCP RPCs and rejects unsafe polling settings", () => {
  assert.equal(taskTerminalWaitBudgetMs(200, 1), 9_249_000);
  assert.throws(() => taskTerminalWaitBudgetMs(0, 1), (error) => error.code === "TASK_TERMINAL_BUDGET_INVALID");
  assert.throws(() => taskTerminalWaitBudgetMs(201, 1), (error) => error.code === "TASK_TERMINAL_BUDGET_INVALID");
  assert.throws(() => taskTerminalWaitBudgetMs(200, Infinity), (error) => error.code === "TASK_TERMINAL_BUDGET_INVALID");
  assert.throws(() => taskTerminalWaitBudgetMs(200, 1_000), (error) => error.code === "TASK_TERMINAL_BUDGET_INVALID");
});

test("Task B projection cannot start before the exact terminal marker", { timeout: 5_000 }, async () => {
  const runDir = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-task-b-terminal-"));
  const markerPath = path.join(runDir, "task-b-complete.proof");
  let projectionStarted = false;
  const driver = { exit: new Promise(() => {}) };
  try {
    const projected = waitForTaskBTerminalBeforeProjection({
      markerPath,
      timeoutMs: 2_000,
      intervalMs: 5,
      driver,
      project: async () => {
        assert.equal(await fs.readFile(markerPath, "utf8"), "temote-memory-task-b-complete-v1\n");
        projectionStarted = true;
        return "projection_started";
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(projectionStarted, false);
    await fs.writeFile(markerPath, "temote-memory-task-b-complete-v1\n", { mode: 0o600, flag: "wx" });
    assert.equal(await projected, "projection_started");
    assert.equal(projectionStarted, true);
  } finally {
    await fs.rm(runDir, { recursive: true, force: true });
  }
});

test("Task B marker timeout and driver exit are distinct and never begin projection", async () => {
  let projectionStarted = false;
  const project = async () => { projectionStarted = true; };
  await assert.rejects(
    waitForTaskBTerminalBeforeProjection({
      markerPath: "/path/which/does/not/exist/task-b-complete.proof",
      timeoutMs: 5,
      intervalMs: 1,
      driver: { exit: new Promise(() => {}) },
      project,
    }),
    (error) => error.code === "TASK_B_TERMINAL_TIMEOUT",
  );
  await assert.rejects(
    waitForTaskBTerminalBeforeProjection({
      markerPath: "/path/which/does/not/exist/task-b-complete.proof",
      timeoutMs: 5_000,
      intervalMs: 1,
      driver: { exit: Promise.resolve({ code: 1 }) },
      project,
    }),
    (error) => error.code === "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_B_TERMINAL",
  );
  assert.equal(projectionStarted, false);
});

test("driver exit cancels the long Task B marker poll, while a marker written at exit wins", { timeout: 5_000 }, async () => {
  let markerReads = 0;
  let projectionStarted = false;
  await assert.rejects(
    waitForTaskBTerminalBeforeProjection({
      markerPath: "/path/which/does/not/exist/task-b-complete.proof",
      timeoutMs: taskTerminalWaitBudgetMs(200, 1),
      intervalMs: 10,
      markerReader: async () => { markerReads += 1; return false; },
      driver: { exit: Promise.resolve({ code: 1 }) },
      project: async () => { projectionStarted = true; },
    }),
    (error) => error.code === "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_B_TERMINAL",
  );
  const readsAtExit = markerReads;
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(markerReads, readsAtExit, "the long polling timer must be aborted when the driver exits");
  assert.equal(projectionStarted, false);

  let racedReads = 0;
  const result = await waitForTaskBTerminalBeforeProjection({
    markerPath: "written-by-driver-before-exit",
    timeoutMs: taskTerminalWaitBudgetMs(200, 1),
    markerReader: async () => { racedReads += 1; return racedReads >= 2; },
    driver: { exit: Promise.resolve({ code: 0 }) },
    project: async () => { projectionStarted = true; return "projected"; },
  });
  assert.equal(result, "projected");
  assert.equal(projectionStarted, true);
});

test("offline proof follows SIGINT exit and a successful absence check", { timeout: 5_000 }, async () => {
  const runDir = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-offline-"));
  const events = [];
  const hostAgent = new FakeChild({ exitOn: ["SIGINT"], handledSignals: ["SIGINT"] });
  hostAgent.on("exit", () => events.push("agent_exit"));
  const runtime = hostListRuntime([], { onCall: () => {
    assert.equal(hostAgent.exitCode, 0, "the owned host process must handle SIGINT before host discovery");
    events.push("host_list_absent");
  } });
  const proofPath = path.join(runDir, "host-a-offline.proof");
  try {
    const result = await stopDedicatedHostAndConfirmOffline({
      host: { hostId: "host-a", hostAgent },
      runtime,
      token: "token",
      proofPath,
      budgetMs: 15_000,
    });
    events.push("proof_written");
    assert.deepEqual(hostAgent.signals, ["SIGINT"]);
    assert.deepEqual(result, { termination: "graceful", lease_fallback: false, offline_confirmed: true });
    assert.deepEqual(events, ["agent_exit", "host_list_absent", "proof_written"]);
    assert.equal(await fs.readFile(proofPath, "utf8"), "temote-memory-host-offline-v1\n");
    assert.equal((await fs.stat(proofPath)).mode & 0o777, 0o600);
  } finally {
    await fs.rm(runDir, { recursive: true, force: true });
  }
});

test("SIGINT timeout uses only the owned PID and waits within the lease fallback budget", { timeout: 5_000 }, async () => {
  const events = [];
  const hostAgent = new FakeChild({ exitOn: ["SIGKILL"] });
  const waitHost = async (_runtime, _token, hostId, present, timeoutMs) => {
    assert.equal(hostAgent.signalCode, "SIGKILL");
    assert.equal(hostId, "host-a");
    assert.equal(present, false);
    assert.equal(timeoutMs, 15, "fallback must budget the 10ms lease plus 5ms margin");
    events.push("host_list_absent");
  };
  const result = await stopDedicatedHostAndConfirmOffline({
    host: { hostId: "host-a", hostAgent },
    runtime: {},
    token: "token",
    proofPath: "ignored-by-test",
    budgetMs: 10_000,
    signalTimeoutMs: 1,
    leaseMs: 10,
    leaseMarginMs: 5,
    stopProcess: undefined,
    waitHost,
    writeProof: async () => events.push("proof_written"),
  });
  assert.deepEqual(hostAgent.signals, ["SIGINT", "SIGKILL"]);
  assert.deepEqual(result, { termination: "forced", lease_fallback: true, offline_confirmed: true });
  assert.deepEqual(events, ["host_list_absent", "proof_written"]);
});

test("graceful SIGINT with a still-leased host waits the bounded lease fallback before proof", async () => {
  const hostAgent = new FakeChild({ exitOn: ["SIGINT"], handledSignals: ["SIGINT"] });
  const hostStillOnline = hostListRuntime([{ host_id: "host-a" }]);
  const waits = [];
  const result = await stopDedicatedHostAndConfirmOffline({
    host: { hostId: "host-a", hostAgent },
    runtime: hostStillOnline,
    token: "token",
    proofPath: "ignored-by-test",
    budgetMs: 10_000,
    signalTimeoutMs: 100,
    gracefulAbsenceTimeoutMs: 3,
    leaseMs: 10,
    leaseMarginMs: 5,
    waitHost: async (runtime, token, hostId, present, timeoutMs) => {
      waits.push(timeoutMs);
      if (waits.length === 1) {
        return waitForHost(runtime, token, hostId, present, timeoutMs, 1);
      }
      assert.equal(present, false);
      assert.equal(timeoutMs, 15);
    },
    writeProof: async () => {},
  });
  assert.deepEqual(hostAgent.signals, ["SIGINT"]);
  assert.deepEqual(waits, [3, 15]);
  assert.deepEqual(result, { termination: "graceful", lease_fallback: true, offline_confirmed: true });
});

test("an unhandled SIGINT exit is not treated as a graceful disconnect", async () => {
  const hostAgent = new FakeChild({ exitOn: ["SIGINT"] });
  let proofWritten = false;
  const result = await stopDedicatedHostAndConfirmOffline({
    host: { hostId: "host-a", hostAgent },
    runtime: {},
    token: "token",
    proofPath: "ignored-by-test",
    budgetMs: 10_000,
    leaseMs: 20,
    leaseMarginMs: 5,
    waitHost: async (_runtime, _token, hostId, present, timeoutMs) => {
      assert.equal(hostAgent.signalCode, "SIGINT");
      assert.equal(hostId, "host-a");
      assert.equal(present, false);
      assert.equal(timeoutMs, 25);
    },
    writeProof: async () => { proofWritten = true; },
  });
  assert.deepEqual(hostAgent.signals, ["SIGINT"]);
  assert.equal(result.termination, "unexpected_exit");
  assert.equal(result.lease_fallback, true);
  assert.equal(proofWritten, true);
});

test("failed host discovery leaves the offline proof unwritten after owned-agent exit", async () => {
  const hostAgent = new FakeChild({ exitOn: ["SIGINT"], handledSignals: ["SIGINT"] });
  let proofWritten = false;
  await assert.rejects(
    stopDedicatedHostAndConfirmOffline({
      host: { hostId: "host-a", hostAgent },
      runtime: hostListRuntime(null, { status: 503, error: "discovery unavailable" }),
      token: "token",
      proofPath: "ignored-by-test",
      budgetMs: 15_000,
      signalTimeoutMs: 100,
      gracefulAbsenceTimeoutMs: 5,
      waitHost: (runtime, token, hostId, present, timeoutMs) => waitForHost(runtime, token, hostId, present, timeoutMs, 1),
      writeProof: async () => { proofWritten = true; },
    }),
    (error) => error.code === "HOST_LIST_UNAVAILABLE",
  );
  assert.equal(hostAgent.exitCode, 0);
  assert.equal(proofWritten, false);
});

test("generic live failure manifest records only hard markers and bounded stage evidence", async () => {
  const runDir = await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-failure-"));
  const metadata = {
    phase: "candidate",
    extractor: "live",
    selectors: { backend: "codex", model: "gpt-5.6-luna", effort: "max", extractor_profile: "opencode-go/glm-5.3-flash" },
    extractorConfig: memoryExtractorSettings("glm-5.3-flash", "low"),
    provenance: { scenario_fingerprint: "safe-fingerprint" },
  };
  try {
    await fs.writeFile(path.join(runDir, "task-a-complete.proof"), "temote-memory-task-a-complete-v1\n", { mode: 0o600 });
    await writeLiveFailureManifest(runDir, metadata, "HOST_LIST_UNAVAILABLE", {
      stage: "stopping_host_a",
      diagnosticHostLabel: "a",
      taskAProjectionPredicatePassed: true,
      taskBProjectionPredicatePassed: false,
      queueReplayPassed: false,
      offlineCheckAttempted: true,
    }, {
      source: { complete: true, acked_through_revision: 10, source_head_revision: 10, cloud_head_seq: 21, gap_count: 0 },
      worker_context_status: { state: "ready", worker_last_cloud_seq: 21, worker_lag: 0 },
    });
    const artifact = JSON.parse(await fs.readFile(path.join(runDir, "candidate-run-failure.json"), "utf8"));
    assert.equal(artifact.gates.task_a_terminal, "pass");
    assert.equal(artifact.gates.source_sync_complete, "pass");
    assert.equal(artifact.gates.task_a_projection_predicate, "pass");
    assert.equal(artifact.gates.task_a_public_context_assertions, "not_run");
    assert.equal(artifact.gates.offline_host, "blocked");
    assert.equal(artifact.events.stable_error_code, "HOST_LIST_UNAVAILABLE");
    assert.equal(JSON.stringify(artifact).includes("response body"), false);
    assert.equal((await fs.stat(path.join(runDir, "candidate-run-failure.json")).then((stat) => stat.mode & 0o777)), 0o600);
    assert.equal(safeHarnessErrorCode(new Error("provider response secret")), "MEMORY_DOGFOOD_FAILED");
  } finally {
    await fs.rm(runDir, { recursive: true, force: true });
  }
});

test("failure diagnostics keep only bounded context status fields and stable error codes", () => {
  const projected = projectSafeContextStatus({
    status: 200,
    body: { result: { content: [{
      type: "text",
      text: JSON.stringify({
        memory: {
          state: "failed",
          enabled: true,
          worker_last_cloud_seq: 4,
          latest_cloud_seq: 9,
          worker_lag: 5,
          last_error_code: "provider_timeout; sensitive output omitted",
          stale: true,
          provider_response: "should never be retained",
        },
        freshness: {
          source_count: 2,
          source_head_revision: 8,
          source_acked_revision: 8,
          source_gap_count: 0,
          observation_count: 9,
          worker_state: "failed",
          worker_last_cloud_seq: 4,
          worker_lag: 5,
          knowledge_stale: true,
          source_cursors: [{ session_id: "not copied into diagnostics" }],
        },
        task_body: "not copied into diagnostics",
      }),
    }] } },
  });
  assert.equal(projected.state, "failed");
  assert.equal(projected.last_error_code, "other");
  assert.equal(projected.worker_last_cloud_seq, 4);
  assert.equal(projected.latest_cloud_seq, 9);
  assert.equal(projected.worker_lag, 5);
  assert.equal(projected.freshness.source_acked_revision, 8);
  assert.equal(projected.freshness.observation_count, 9);
  assert.equal(JSON.stringify(projected).includes("sensitive output omitted"), false);
  assert.equal(JSON.stringify(projected).includes("provider_response"), false);
  assert.equal(JSON.stringify(projected).includes("task_body"), false);

  const missingCursor = projectSafeContextStatus({
    status: 200,
    body: { result: { content: [{
      type: "text",
      text: JSON.stringify({ memory: { state: "ready_empty", worker_last_cloud_seq: null }, freshness: {
        source_head_revision: null,
        source_acked_revision: null,
      } }),
    }] } },
  });
  assert.equal(missingCursor.worker_last_cloud_seq, null);
  assert.equal(missingCursor.freshness.source_head_revision, null);
  assert.equal(missingCursor.freshness.source_acked_revision, null);
});

test("extractor reasoning effort is optional, normalized, and bounded to provider-supported values", () => {
  assert.equal(resolveExtractorReasoningEffort({ extractor: "live" }), null);
  assert.equal(resolveExtractorReasoningEffort({
    extractor: "live",
    environmentValue: "  LOW  ",
  }), "low");
  assert.equal(resolveExtractorReasoningEffort({
    extractor: "live",
    explicitValue: "xhigh",
    environmentValue: "low",
  }), "xhigh");
  assert.throws(
    () => resolveExtractorReasoningEffort({ extractor: "live", explicitValue: "budget-100" }),
    (error) => error.code === "INVALID_EXTRACTOR_REASONING_EFFORT",
  );
  assert.throws(
    () => resolveExtractorReasoningEffort({ extractor: "fixture", explicitValue: "low" }),
    (error) => error.code === "EXTRACTOR_REASONING_EFFORT_REQUIRES_LIVE_EXTRACTOR",
  );
  assert.deepEqual(memoryExtractorSettings("glm-5.3-flash", "low"), {
    model: "glm-5.3-flash",
    reasoning_effort: "low",
    timeout_ms: 60_000,
    input_budget_bytes: 32_768,
    output_budget_bytes: 8_192,
    response_envelope_budget_bytes: 32_768,
    max_attempts: 3,
    batch_size: 16,
  });
  assert.equal(memoryExtractorSettings(null, null).reasoning_effort, null);
});

function observation() {
  const sha256 = createHash("sha256").update(CONTENT).digest("hex");
  return {
    id: randomUUID(),
    schema_version: 1,
    observed_at: Math.floor(Date.now() / 1000),
    session_id: SESSION,
    session_instance: { started_at: Math.floor(Date.now() / 1000), process_id: 1 },
    actor: { transport: "mcp-stdio" },
    target: { backend: "codex" },
    action: "task_start",
    kind: "instruction",
    task_id: TASK,
    operation_id: OPERATION,
    content: {
      kind: "text",
      preview: CONTENT,
      total_bytes: Buffer.byteLength(CONTENT),
      sha256,
      truncated: false,
    },
    evidence_refs: [],
    provenance: { tool: "codex_task_start", source: "orchestration" },
    revision: 1,
    dedupe_key: "instruction:" + SESSION + ":1",
  };
}

async function mcpValue(runtime, name, args) {
  const response = await runtime.callMcp(MEMORY_TEST_CLIENT_TOKEN, name, args, 17);
  assert.equal(response.status, 200, `${name} should return HTTP 200`);
  assert.equal(response.body.error, undefined, `${name} should not return an RPC error`);
  const text = response.body.result?.content?.find((item) => item.type === "text")?.text;
  assert.equal(typeof text, "string", `${name} should return a text result`);
  return JSON.parse(text);
}

test("host sync automatically drives the real D1 queue consumer into repository context", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "fixture",
    memoryEnabled: true,
    bindings: {
      OBSERVATION_OWNER_ID: MEMORY_TEST_OWNER,
      HOST_TOKENS_JSON: JSON.stringify({ [MEMORY_TEST_HOST_ID]: MEMORY_TEST_HOST_TOKEN }),
      CLIENT_TOKEN: MEMORY_TEST_CLIENT_TOKEN,
    },
  });

  try {
    const record = observation();
    const syncBody = {
      schema_version: 1,
      session_id: SESSION,
      repository_key: MEMORY_TEST_REPOSITORY,
      source_base_revision: 0,
      source_head_revision: 1,
      journal_degraded: false,
      gap_count: 0,
      records: [{ source_revision: 1, observation: record }],
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
        body: JSON.stringify(syncBody),
      },
    );
    assert.equal(response.status, 200, "authenticated host sync should commit the observation");
    const sync = await response.json();
    assert.equal(sync.acked_through_revision, 1);
    assert.equal(sync.complete, true);

    await runtime.waitFor(async () => {
      const checkpoints = await runtime.querySql(
        "SELECT last_cloud_seq, last_error_code FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      );
      return checkpoints.length === 1 && Number(checkpoints[0].last_cloud_seq) === Number(sync.cloud_head_seq)
        && checkpoints[0].last_error_code == null;
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const [sources, observations, runs, items, supports, outbox] = await Promise.all([
      runtime.querySql(
        "SELECT host_id, session_id, repository_key, acked_through_revision FROM observation_sources WHERE owner_id = ? AND session_id = ?",
        [MEMORY_TEST_OWNER, SESSION],
      ),
      runtime.querySql(
        "SELECT cloud_seq, observation_id, kind, task_id, operation_id FROM observations WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT status, outcome, input_count FROM memory_runs WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, kind, text, status, scope_type, scope_id FROM knowledge_items WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT knowledge_id, observation_cloud_seq, observation_id FROM knowledge_support WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
      runtime.querySql(
        "SELECT queued_at, attempt_count, last_error_code FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY],
      ),
    ]);

    assert.equal(sources.length, 1);
    assert.equal(sources[0].host_id, MEMORY_TEST_HOST_ID);
    assert.equal(sources[0].acked_through_revision, 1);
    assert.equal(observations.length, 1);
    assert.equal(observations[0].task_id, TASK);
    assert.equal(observations[0].operation_id, OPERATION);
    assert.deepEqual(runs.map((run) => run.status), ["completed"]);
    assert.equal(runs[0].outcome, "projected");
    const policy = items.find((item) => item.kind === "constraint" && item.text === POLICY_QUOTE);
    assert.ok(policy, "the normal instruction should produce its supported repository constraint");
    assert.equal(policy.scope_type, "repository");
    assert.equal(policy.scope_id, MEMORY_TEST_REPOSITORY);
    assert.equal(policy.status, "current");
    const policySupport = supports.find((support) => support.knowledge_id === policy.knowledge_id);
    assert.ok(policySupport, "the policy must have a concrete support ref");
    assert.equal(policySupport.observation_id, record.id);
    assert.equal(outbox.length, 1);
    assert.equal(outbox[0].queued_at != null, true);
    assert.equal(outbox[0].last_error_code, null);

    const context = await mcpValue(runtime, "context_resolve", {
      repository: MEMORY_TEST_REPOSITORY,
      limit: 8,
    });
    const values = JSON.stringify(context);
    assert.match(values, /replicated_observed|derived/);
    assert.match(values, /Report output format must be JSON\./);
    assert.match(values, new RegExp(UNRESOLVED_TEXT));
    assert.match(values, /current/);
    assert.match(values, new RegExp(record.id));

    const replay = await runtime.fetch(
      `https://memory-test.local/v1/hosts/${MEMORY_TEST_HOST_ID}/observations/sync`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${MEMORY_TEST_HOST_TOKEN}`,
          "content-type": "application/json",
          "x-temote-host-id": MEMORY_TEST_HOST_ID,
        },
        body: JSON.stringify(syncBody),
      },
    );
    assert.equal(replay.status, 200, "an exact repeated observation batch should be acknowledged");
    const replayBody = await replay.json();
    assert.equal(replayBody.acked_through_revision, sync.acked_through_revision);
    assert.equal(replayBody.cloud_head_seq, sync.cloud_head_seq);

    const [itemsBefore, supportsBefore, runsBefore, checkpointBefore, queueBefore] = await Promise.all([
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_items WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM memory_runs WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT MAX(last_cloud_seq) AS seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.queueDispatchCount(),
    ]);
    const duplicateWake = await runtime.enqueue({
      owner_id: MEMORY_TEST_OWNER,
      repository_key: MEMORY_TEST_REPOSITORY,
      through_cloud_seq: Number(sync.cloud_head_seq),
    });
    assert.equal(duplicateWake.success, true);
    await runtime.waitFor(async () => {
      const dispatches = await runtime.queueDispatches();
      return dispatches.length > queueBefore && dispatches.at(-1).state === "completed";
    }, { timeoutMs: 10_000, intervalMs: 25 });
    const [itemsAfter, supportsAfter, runsAfter, checkpointAfter] = await Promise.all([
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_items WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM knowledge_support WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT COUNT(*) AS count FROM memory_runs WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
      runtime.querySql("SELECT MAX(last_cloud_seq) AS seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?", [MEMORY_TEST_OWNER, MEMORY_TEST_REPOSITORY]),
    ]);
    assert.equal(itemsAfter[0].count, itemsBefore[0].count, "Queue redelivery must not add knowledge rows");
    assert.equal(supportsAfter[0].count, supportsBefore[0].count, "Queue redelivery must not add support rows");
    assert.equal(runsAfter[0].count, runsBefore[0].count, "Queue redelivery must not add completed runs");
    assert.equal(checkpointAfter[0].seq, checkpointBefore[0].seq, "Queue redelivery must not move the checkpoint");

    const unrelated = await mcpValue(runtime, "context_resolve", {
      repository: "github:temote-tests/memory-continuity-unrelated",
      query: "report output format",
      limit: 8,
    });
    assert.doesNotMatch(JSON.stringify(unrelated), /Report output format must be JSON\./);
  } finally {
    await runtime.dispose();
  }
});
