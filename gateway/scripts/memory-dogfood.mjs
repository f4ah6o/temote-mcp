#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { stop as stopEsbuild } from "esbuild";

import { startMemoryRuntime } from "../test/helpers/memory-runtime.mjs";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const REPOSITORY_KEY = "github:temote-tests/memory-continuity";
const UNRELATED_REPOSITORY = "github:temote-tests/memory-continuity-unrelated";
const OWNER_ID = "memory-dogfood-owner";
const OFFLINE_MARKER = "temote-memory-host-offline-v1\n";
const TASK_A_MARKER = "temote-memory-task-a-complete-v1\n";
const TASK_B_READY_MARKER = "temote-memory-task-b-ready-v1\n";
const TASK_B_COMPLETE_MARKER = "temote-memory-task-b-complete-v1\n";
const DEFAULT_BASELINE = path.join(ROOT, "dogfood/runs/memory-20260928/baseline-bin/temote-mcp");
const DEFAULT_BASELINE_GATEWAY = path.join(ROOT, "dogfood/runs/memory-20260928/baseline-full-source/gateway");
const DEFAULT_CANDIDATE = path.join(ROOT, "target/debug/temote-mcp");
const DEFAULT_ENDPOINT = "https://opencode.ai/zen/go/v1/chat/completions";
const DEFAULT_EXTRACTOR_MODEL = "glm-5.3-flash";
const DEFAULT_TASK_MODEL = "gpt-5.6-luna";
const DEFAULT_TASK_EFFORT = "max";
const DEFAULT_BACKEND = "codex";
const MEMORY_TIMEOUT_MS = 60_000;
const MEMORY_INPUT_BUDGET_BYTES = 32_768;
const MEMORY_OUTPUT_BUDGET_BYTES = 8_192;
const MEMORY_PROVIDER_ENVELOPE_BUDGET_BYTES = Math.min(
  80 * 1_024,
  2 * MEMORY_OUTPUT_BUDGET_BYTES + 16_384,
);
const MEMORY_MAX_ATTEMPTS = 3;
const MEMORY_BATCH_SIZE = 16;
const MEMORY_REASONING_EFFORTS = new Set(["low", "medium", "high", "minimal", "none", "max", "xhigh"]);
const HOST_LEASE_MS = 90_000;
const HOST_LEASE_MARGIN_MS = 5_000;
const HOST_GRACEFUL_OFFLINE_WAIT_MS = 15_000;
const MCP_RPC_TIMEOUT_MS = 45_000;
const TASK_TERMINAL_WAIT_MARGIN_MS = 5_000;
const MAX_TASK_TERMINAL_WAIT_MS = 10_000_000;
const MAX_ARTIFACT_BYTES = 1_048_576;
const MEMORY_ERROR_CODES = new Set([
  "db_unavailable", "extractor_not_configured", "provider_not_configured", "provider_timeout",
  "provider_unavailable", "provider_rejected",
  "provider_invalid_response", "provider_incomplete_response", "provider_envelope_too_large",
  "provider_output_too_large", "provider_configuration_invalid", "invalid_output", "invalid_support",
  "invalid_scope", "invalid_verification_support", "invalid_supersession", "input_too_large",
  "producer_generation_conflict", "producer_generation_stale", "run_retry_exhausted",
  "projection_too_large", "commit_rejected", "worker_internal", "queue_not_configured", "queue_send_failed",
]);
const SAFE_HARNESS_ERROR_CODES = new Set([
  "ARTIFACT_PATH_MUST_BE_IGNORED_RUNS", "AUTOMATIC_MEMORY_FIXTURE_TEST_FAILED",
  "CHILD_COMMAND_FAILED", "CODING_BACKEND_SELECTOR_UNSUPPORTED", "CONTEXT_STATUS_FAILED",
  "CONTEXT_STATUS_RESULT_INVALID", "DEDICATED_HOST_CONNECT_TIMEOUT", "DEDICATED_HOST_OFFLINE_TIMEOUT",
  "DOGFOOD_ARTIFACT_IDENTITY_INVALID", "DOGFOOD_DRIVER_EXITED_BEFORE_HANDSHAKE",
  "DOGFOOD_DRIVER_EXITED_BEFORE_PROJECTION", "DOGFOOD_DRIVER_TIMEOUT", "DOGFOOD_FIXTURE_DRIVER_FAILED",
  "DOGFOOD_HANDSHAKE_TIMEOUT", "DOGFOOD_SCENARIO_FAILED", "DRIVER_START_FAILED", "DUPLICATE_ARGUMENT",
  "EXTRACTOR_CREDENTIAL_UNAVAILABLE", "EXTRACTOR_REASONING_EFFORT_REQUIRES_LIVE_EXTRACTOR",
  "FIXTURE_MODE_CANNOT_USE_LIVE_EXTRACTOR", "HOST_AGENT_STOP_TIMEOUT", "HOST_LIST_FAILED",
  "HOST_LIST_RESULT_INVALID", "HOST_LIST_UNAVAILABLE", "HOST_OFFLINE_BUDGET_EXCEEDED",
  "INVALID_ARGUMENT", "INVALID_EXTRACTOR_REASONING_EFFORT", "INVALID_PHASE_MODE_OR_EXTRACTOR", "INVALID_PIPELINE_WAIT",
  "INVALID_PROJECTION_TABLE", "MEMORY_DOGFOOD_FAILED", "MEMORY_OUTBOX_MISSING", "MISSING_ARGUMENT_VALUE",
  "QUEUE_HAS_UNSETTLED_DISPATCH", "QUEUE_REPLAY_CHANGED_PROJECTION", "QUEUE_REPLAY_NOT_CONSUMED",
  "SCENARIO_CONTRACT_INVALID", "SCENARIO_UNAVAILABLE", "TEMOTE_BINARY_NOT_FOUND", "UNKNOWN_ARGUMENT",
  "TASK_A_TERMINAL_TIMEOUT", "TASK_B_TERMINAL_TIMEOUT", "TASK_TERMINAL_BUDGET_INVALID",
  "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_A_TERMINAL", "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_B_TERMINAL",
]);

class HarnessError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

function safeCount(value) {
  if (value == null || typeof value === "boolean") return null;
  const count = Number(value);
  return Number.isSafeInteger(count) && count >= 0 ? count : null;
}

export function resolveExtractorReasoningEffort({ explicitValue, environmentValue, extractor }) {
  if (explicitValue !== undefined && extractor !== "live") {
    throw new HarnessError("EXTRACTOR_REASONING_EFFORT_REQUIRES_LIVE_EXTRACTOR");
  }
  const requested = explicitValue ?? (extractor === "live" ? environmentValue : undefined);
  if (requested === undefined) return null;
  if (typeof requested !== "string") throw new HarnessError("INVALID_EXTRACTOR_REASONING_EFFORT");
  const normalized = requested.trim().toLowerCase();
  if (normalized === "") return null;
  if (!MEMORY_REASONING_EFFORTS.has(normalized)) {
    throw new HarnessError("INVALID_EXTRACTOR_REASONING_EFFORT");
  }
  return normalized;
}

export function memoryExtractorSettings(model, reasoningEffort) {
  return {
    model,
    reasoning_effort: reasoningEffort,
    timeout_ms: MEMORY_TIMEOUT_MS,
    input_budget_bytes: MEMORY_INPUT_BUDGET_BYTES,
    output_budget_bytes: MEMORY_OUTPUT_BUDGET_BYTES,
    response_envelope_budget_bytes: MEMORY_PROVIDER_ENVELOPE_BUDGET_BYTES,
    max_attempts: MEMORY_MAX_ATTEMPTS,
    batch_size: MEMORY_BATCH_SIZE,
  };
}

export function taskTerminalWaitBudgetMs(maxPolls, pollIntervalSeconds) {
  if (!Number.isSafeInteger(maxPolls) || maxPolls < 1 || maxPolls > 200
      || typeof pollIntervalSeconds !== "number" || !Number.isFinite(pollIntervalSeconds)
      || pollIntervalSeconds < 0) {
    throw new HarnessError("TASK_TERMINAL_BUDGET_INVALID");
  }
  const budgetMs = Math.ceil((maxPolls + 1) * MCP_RPC_TIMEOUT_MS
    + Math.max(0, maxPolls - 1) * pollIntervalSeconds * 1_000
    + TASK_TERMINAL_WAIT_MARGIN_MS);
  if (!Number.isSafeInteger(budgetMs) || budgetMs > MAX_TASK_TERMINAL_WAIT_MS) {
    throw new HarnessError("TASK_TERMINAL_BUDGET_INVALID");
  }
  return budgetMs;
}

function taskTerminalWaitBudgetFromFlags(flags) {
  const maxPollsRaw = flags["task-polls"] ?? "120";
  const pollIntervalRaw = flags["poll-interval"] ?? "1";
  if (!/^\d+$/.test(maxPollsRaw)
      || !/^(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(pollIntervalRaw)) {
    throw new HarnessError("TASK_TERMINAL_BUDGET_INVALID");
  }
  const maxPolls = Number(maxPollsRaw);
  const pollIntervalSeconds = Number(pollIntervalRaw);
  return {
    maxPolls,
    pollIntervalSeconds,
    waitMs: taskTerminalWaitBudgetMs(maxPolls, pollIntervalSeconds),
  };
}

function projectionWaitMsFromFlags(flags) {
  const seconds = Number(flags["pipeline-wait-seconds"] ?? "240");
  if (!Number.isSafeInteger(seconds) || seconds < 1 || seconds > 600) {
    throw new HarnessError("INVALID_PIPELINE_WAIT");
  }
  return seconds * 1_000;
}

function safeErrorCode(value) {
  return value == null ? null : MEMORY_ERROR_CODES.has(value) ? value : "other";
}

function safeTimestamp(value) {
  return typeof value === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z$/.test(value)
    ? value
    : null;
}

function safeState(value) {
  return new Set(["disabled", "not_configured", "failed", "lagging", "ready", "ready_empty"]).has(value)
    ? value
    : "unknown";
}

export function projectSafeContextStatus(rpcResponse) {
  if (!rpcResponse || rpcResponse.status !== 200) {
    return { state: "unavailable", error_code: "context_status_unavailable" };
  }
  const encoded = rpcResponse.body?.result?.content?.find((item) => item?.type === "text")?.text;
  if (typeof encoded !== "string" || Buffer.byteLength(encoded) > 32_768) {
    return { state: "unavailable", error_code: "context_status_invalid_response" };
  }
  let context;
  try {
    context = JSON.parse(encoded);
  } catch {
    return { state: "unavailable", error_code: "context_status_invalid_response" };
  }
  const memory = context?.memory && typeof context.memory === "object" ? context.memory : {};
  const freshness = context?.freshness && typeof context.freshness === "object" ? context.freshness : {};
  const boolOrNull = (value) => typeof value === "boolean" ? value : null;
  return {
    state: safeState(memory.state),
    enabled: boolOrNull(memory.enabled),
    active_producer_version: typeof memory.active_producer_version === "string"
      && memory.active_producer_version.length <= 128 ? memory.active_producer_version : null,
    requested_producer_version: typeof memory.requested_producer_version === "string"
      && memory.requested_producer_version.length <= 128 ? memory.requested_producer_version : null,
    worker_last_cloud_seq: safeCount(memory.worker_last_cloud_seq),
    latest_cloud_seq: safeCount(memory.latest_cloud_seq),
    worker_lag: safeCount(memory.worker_lag),
    last_success_at: safeTimestamp(memory.last_success_at),
    last_error_at: safeTimestamp(memory.last_error_at),
    last_error_code: safeErrorCode(memory.last_error_code),
    stale: boolOrNull(memory.stale),
    freshness: {
      source_count: safeCount(freshness.source_count),
      source_head_revision: safeCount(freshness.source_head_revision),
      source_acked_revision: safeCount(freshness.source_acked_revision),
      source_gap_count: safeCount(freshness.source_gap_count),
      observation_count: safeCount(freshness.observation_count),
      cloud_observation_stale: boolOrNull(freshness.cloud_observation_stale),
      worker_state: safeState(freshness.worker_state),
      worker_last_cloud_seq: safeCount(freshness.worker_last_cloud_seq),
      worker_lag: safeCount(freshness.worker_lag),
      knowledge_stale: boolOrNull(freshness.knowledge_stale),
    },
  };
}

function safeStatusRow(row, fields) {
  return Object.fromEntries(fields.map((field) => {
    if (field === "last_error_code" || field === "error_code") return [field, safeErrorCode(row?.[field])];
    if (field === "last_success_at" || field === "last_error_at" || field === "queued_at") {
      return [field, safeTimestamp(row?.[field])];
    }
    if (field === "status" && row?.status != null) {
      return [field, new Set(["pending", "running", "completed", "failed", "candidate", "supported", "current", "superseded", "retracted"]).has(row.status) ? row.status : "unknown"];
    }
    if (field === "state" && row?.state != null) {
      return [field, new Set(["running", "completed", "failed"]).has(row.state) ? row.state : "unknown"];
    }
    if (field === "kind" && row?.kind != null) {
      return [field, new Set(["fact", "decision", "constraint", "observation", "failure_pattern", "unresolved", "summary"]).has(row.kind) ? row.kind : "unknown"];
    }
    if (field === "outcome" && row?.outcome != null) {
      return [field, new Set(["projected", "empty"]).has(row.outcome) ? row.outcome : "unknown"];
    }
    if (field === "producer_version" || field.endsWith("_producer_version")) {
      const value = row?.[field];
      return [field, typeof value === "string" && /^[A-Za-z0-9_.:-]{1,128}$/.test(value) ? value : null];
    }
    if (field === "stale" || field === "journal_degraded") {
      return [field, row?.[field] == null ? null : Number(row[field]) === 1];
    }
    return [field, safeCount(row?.[field])];
  }));
}

async function diagnosticQuery(runtime, sql, params, fields, { aggregate = false, limit = 16 } = {}) {
  try {
    const rows = await runtime.querySql(sql, params);
    if (aggregate) return safeStatusRow(rows[0] ?? {}, fields);
    return rows.slice(0, limit).map((row) => safeStatusRow(row, fields));
  } catch {
    return { error_code: "diagnostic_query_failed" };
  }
}

async function collectProjectionDiagnostics(runtime, clientToken, host) {
  let workerContextStatus;
  try {
    const response = await runtime.callMcp(
      clientToken,
      "context_status",
      { repository: REPOSITORY_KEY },
      991,
    );
    workerContextStatus = projectSafeContextStatus(response);
  } catch {
    workerContextStatus = { state: "unavailable", error_code: "context_status_unavailable" };
  }
  const [sourceRows, sourceTerminal, observations, runs, checkpoints, projectionHead, outbox, queueDispatches, knowledgeCounts] = await Promise.all([
    diagnosticQuery(runtime,
      "SELECT source_head_revision, acked_through_revision, cloud_head_seq, journal_degraded, gap_count FROM observation_sources WHERE owner_id = ? AND host_id = ? AND session_id = ? AND repository_key = ?",
      [OWNER_ID, host.hostId, host.sessionId, REPOSITORY_KEY],
      ["source_head_revision", "acked_through_revision", "cloud_head_seq", "journal_degraded", "gap_count"],
      { limit: 1 },
    ),
    diagnosticQuery(runtime,
      "SELECT COUNT(*) AS terminal_observation_count FROM observations WHERE owner_id = ? AND host_id = ? AND session_id = ? AND repository_key = ? AND kind = 'execution_state' AND state_status IN ('completed', 'failed', 'interrupted')",
      [OWNER_ID, host.hostId, host.sessionId, REPOSITORY_KEY], ["terminal_observation_count"], { aggregate: true },
    ),
    diagnosticQuery(runtime,
      "SELECT COUNT(*) AS observation_count, COALESCE(MAX(cloud_seq), 0) AS latest_cloud_seq FROM observations WHERE owner_id = ? AND repository_key = ?",
      [OWNER_ID, REPOSITORY_KEY], ["observation_count", "latest_cloud_seq"], { aggregate: true },
    ),
    diagnosticQuery(runtime,
      "SELECT status, outcome, attempt_count, from_seq, to_seq, error_code FROM memory_runs WHERE owner_id = ? AND repository_key = ? ORDER BY started_at DESC LIMIT 16",
      [OWNER_ID, REPOSITORY_KEY],
      ["status", "outcome", "attempt_count", "from_seq", "to_seq", "error_code"],
    ),
    diagnosticQuery(runtime,
      "SELECT producer_version, last_cloud_seq, last_success_at, last_error_at, last_error_code, stale, fence, projection_epoch FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ? ORDER BY last_cloud_seq DESC LIMIT 8",
      [OWNER_ID, REPOSITORY_KEY],
      ["producer_version", "last_cloud_seq", "last_success_at", "last_error_at", "last_error_code", "stale", "fence", "projection_epoch"],
    ),
    diagnosticQuery(runtime,
      "SELECT requested_generation, requested_producer_version, active_generation, active_producer_version, epoch FROM memory_projection_heads WHERE owner_id = ? AND repository_key = ? LIMIT 1",
      [OWNER_ID, REPOSITORY_KEY],
      ["requested_generation", "requested_producer_version", "active_generation", "active_producer_version", "epoch"],
      { limit: 1 },
    ),
    diagnosticQuery(runtime,
      "SELECT through_cloud_seq, queued_at, attempt_count, next_attempt_at, last_error_code FROM memory_outbox WHERE owner_id = ? AND repository_key = ? LIMIT 1",
      [OWNER_ID, REPOSITORY_KEY],
      ["through_cloud_seq", "queued_at", "attempt_count", "next_attempt_at", "last_error_code"],
      { limit: 1 },
    ),
    diagnosticQuery(runtime,
      "SELECT state, COUNT(*) AS dispatch_count, SUM(message_count) AS message_count, SUM(CASE WHEN completed_at IS NOT NULL THEN 1 ELSE 0 END) AS completed_count FROM __memory_test_queue_dispatches GROUP BY state",
      [], ["state", "dispatch_count", "message_count", "completed_count"],
    ),
    diagnosticQuery(runtime,
      "SELECT kind, status, COUNT(*) AS item_count FROM knowledge_items WHERE owner_id = ? AND repository_key = ? GROUP BY kind, status ORDER BY kind, status LIMIT 32",
      [OWNER_ID, REPOSITORY_KEY], ["kind", "status", "item_count"],
    ),
  ]);
  const source = Array.isArray(sourceRows) ? sourceRows[0] ?? null : null;
  return {
    schema_version: 1,
    diagnostic_type: "automatic_memory_projection_timeout",
    host_label: host.label,
    worker_context_status: workerContextStatus,
    source: source ? {
      ...source,
      terminal_observation_count: sourceTerminal?.terminal_observation_count ?? null,
      complete: Number.isSafeInteger(source.acked_through_revision)
        && Number.isSafeInteger(source.source_head_revision)
        && source.acked_through_revision >= source.source_head_revision
        && source.journal_degraded === false && source.gap_count === 0,
    } : { present: false },
    observations,
    memory_runs: runs,
    checkpoints,
    projection_head: Array.isArray(projectionHead) ? projectionHead[0] ?? null : projectionHead,
    outbox: Array.isArray(outbox) ? outbox[0] ?? null : outbox,
    queue_dispatches: queueDispatches,
    knowledge_counts: knowledgeCounts,
  };
}

async function writeProjectionDiagnostics(runtime, clientToken, host, runDir) {
  try {
    const result = await Promise.race([
      collectProjectionDiagnostics(runtime, clientToken, host),
      delay(5_000).then(() => null),
    ]);
    const snapshot = result ?? {
      schema_version: 1,
      diagnostic_type: "automatic_memory_projection_timeout",
      host_label: host.label,
      diagnostic_capture: "timed_out",
    };
    await fs.writeFile(
      path.join(runDir, `projection-${host.label}-diagnostics.json`),
      JSON.stringify(snapshot, null, 2) + "\n",
      { mode: 0o600, flag: "wx" },
    );
    return snapshot;
  } catch {
    // Diagnostics are best-effort and must never replace the pipeline failure.
    return null;
  }
}

async function writeLiveFailureDiagnostics(runtime, clientToken, host, phase, runDir) {
  try {
    const result = await Promise.race([
      collectProjectionDiagnostics(runtime, clientToken, host),
      delay(5_000).then(() => null),
    ]);
    const snapshot = result ?? {
      schema_version: 1,
      diagnostic_type: "automatic_memory_run_failure",
      host_label: host?.label ?? "unknown",
      diagnostic_capture: "timed_out",
    };
    snapshot.diagnostic_type = "automatic_memory_run_failure";
    await fs.writeFile(
      path.join(runDir, `${phase}-run-diagnostics.json`),
      JSON.stringify(snapshot, null, 2) + "\n",
      { mode: 0o600, flag: "wx" },
    );
    return snapshot;
  } catch {
    // Diagnostics are best-effort and must never replace the pipeline failure.
    return null;
  }
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

async function collectRunProvenance(binary, scenario) {
  const sourceHead = runCommand("git", ["rev-parse", "HEAD"], { cwd: ROOT }).trim();
  const workingDiff = runCommand("git", ["diff", "--binary", "HEAD"], { cwd: ROOT });
  const inputs = {
    repository_key: scenario.repository_key,
    task_a: scenario.task_a.instruction,
    task_b: scenario.task_b.instruction,
    limits: scenario.limits,
  };
  const [binaryBytes, runnerBytes, helperBytes, scenarioBytes] = await Promise.all([
    fs.readFile(binary),
    fs.readFile(fileURLToPath(import.meta.url)),
    fs.readFile(path.join(ROOT, "target/debug/temote-linux-sandbox")).catch(() => Buffer.alloc(0)),
    fs.readFile(path.join(ROOT, "dogfood/scenarios/memory-continuity.json")),
  ]);
  const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
  const canonicalInputs = Buffer.from(canonicalJson(inputs));
  const canonicalScenario = Buffer.from(canonicalJson(scenario));
  return {
    source_head: sourceHead,
    working_diff_sha256: sha256(Buffer.from(workingDiff)),
    binary_sha256: sha256(binaryBytes),
    input_sha256: sha256(canonicalInputs),
    scenario_fingerprint: sha256(canonicalScenario),
    runner_sha256: sha256(runnerBytes),
    sandbox_helper_sha256: helperBytes.length > 0 ? sha256(helperBytes) : null,
    scenario_file_sha256: sha256(scenarioBytes),
  };
}

async function writeProjectionFailureManifest(runDir, metadata, host, diagnostics) {
  try {
    const markerPath = path.join(runDir, "task-a-complete.proof");
    let taskATerminal = false;
    try {
      taskATerminal = (await fs.readFile(markerPath, "utf8")) === TASK_A_MARKER;
    } catch {
      // A missing marker is represented as false, without reading task output.
    }
    const sourceComplete = diagnostics?.source?.complete === true;
    const manifest = {
      schema_version: 1,
      scenario_id: "memory-continuity",
      scenario_revision: 1,
      scenario_fingerprint: metadata.provenance.scenario_fingerprint,
      phase: metadata.phase,
      mode: "live",
      synthesis_mode: metadata.extractor === "live" ? "live" : "fixture",
      outcome: "blocked",
      live_synthesis: "NOT QUALIFIED",
      selectors: metadata.selectors,
      extractor_config: metadata.extractorConfig,
      provenance: metadata.provenance,
      gates: {
        task_a_terminal: taskATerminal ? "pass" : "not_run",
        source_sync_complete: sourceComplete ? "pass" : "blocked",
        task_a_knowledge_projection: host.label === "a" ? "blocked" : "not_run",
        head_switch: "not_run",
        offline_host: "not_run",
        task_b_supersession: "not_run",
        queue_replay: "not_run",
        tenancy: "not_run",
      },
      events: {
        projection_host: host.label,
        stable_error_code: `PROJECTION_${host.label.toUpperCase()}_TIMEOUT`,
        task_a_terminal_marker_verified: taskATerminal,
        source_acked_revision: diagnostics?.source?.acked_through_revision ?? null,
        source_head_revision: diagnostics?.source?.source_head_revision ?? null,
        source_cloud_head: diagnostics?.source?.cloud_head_seq ?? null,
        source_gap_count: diagnostics?.source?.gap_count ?? null,
        worker_status: diagnostics?.worker_context_status?.state ?? "unavailable",
        live_qualification: "not_qualified",
      },
    };
    await fs.writeFile(
      path.join(runDir, `${metadata.phase}-failure.json`),
      JSON.stringify(manifest, null, 2) + "\n",
      { mode: 0o600, flag: "wx" },
    );
  } catch {
    // Failure manifests must not mask the underlying projection gate failure.
  }
}

export function safeHarnessErrorCode(error) {
  if (!(error instanceof HarnessError)) return "MEMORY_DOGFOOD_FAILED";
  const code = error.code;
  const stableDynamicCode = /^PROJECTION_[AB]_TIMEOUT$/.test(code)
    || /^(HOST_AGENT|SUPERVISOR)_[AB]_(SOCKET_NAMESPACE_INVALID|GATEWAY_AUTH_FAILED|GATEWAY_CONNECT_FAILED|GATEWAY_URL_INVALID|CHILD_SIGNALED|CHILD_EXIT_\d{1,3}|CHILD_START_FAILED|START_FAILED)$/.test(code);
  return SAFE_HARNESS_ERROR_CODES.has(code) || stableDynamicCode ? code : "MEMORY_DOGFOOD_FAILED";
}

async function markerMatches(filePath, expected) {
  try {
    return (await fs.readFile(filePath, "utf8")) === expected;
  } catch {
    return false;
  }
}

export async function writeLiveFailureManifest(runDir, metadata, failureCode, progress, diagnostics) {
  try {
    const taskATerminal = await markerMatches(path.join(runDir, "task-a-complete.proof"), TASK_A_MARKER);
    const offlineProof = await markerMatches(path.join(runDir, "host-a-offline.proof"), OFFLINE_MARKER);
    const taskBReady = await markerMatches(path.join(runDir, "task-b-ready.proof"), TASK_B_READY_MARKER);
    const manifest = {
      schema_version: 1,
      scenario_id: "memory-continuity",
      scenario_revision: 1,
      scenario_fingerprint: metadata.provenance.scenario_fingerprint,
      phase: metadata.phase,
      mode: "live",
      synthesis_mode: metadata.extractor === "live" ? "live" : "fixture",
      outcome: "blocked",
      live_synthesis: "NOT QUALIFIED",
      selectors: metadata.selectors,
      extractor_config: metadata.extractorConfig,
      provenance: metadata.provenance,
      gates: {
        task_a_terminal: taskATerminal ? "pass" : progress.taskATerminalWaitStarted ? "blocked" : "not_run",
        source_sync_complete: diagnostics?.source?.complete === true ? "pass" : "blocked",
        task_a_projection_predicate: progress.taskAProjectionPredicatePassed ? "pass" : "not_run",
        task_a_public_context_assertions: "not_run",
        offline_host: offlineProof ? "pass" : progress.offlineCheckAttempted ? "blocked" : "not_run",
        task_b_ready: taskBReady ? "pass" : "not_run",
        task_b_terminal: await markerMatches(path.join(runDir, "task-b-complete.proof"), TASK_B_COMPLETE_MARKER)
          ? "pass" : progress.taskBTerminalWaitStarted ? "blocked" : "not_run",
        task_b_projection_predicate: progress.taskBProjectionPredicatePassed ? "pass" : "not_run",
        queue_replay: progress.queueReplayPassed ? "pass" : "not_run",
        scenario_assertions: "not_run",
      },
      progress: {
        last_completed_stage: progress.stage,
        diagnostic_host: progress.diagnosticHostLabel ?? null,
        task_terminal_wait_ms: progress.taskTerminalWaitMs ?? null,
        projection_wait_ms: progress.projectionWaitMs ?? null,
        task_polls: progress.taskPolls ?? null,
        poll_interval_seconds: progress.pollIntervalSeconds ?? null,
      },
      events: {
        stable_error_code: failureCode,
        task_a_terminal_marker_verified: taskATerminal,
        task_a_terminal_wait_started: progress.taskATerminalWaitStarted === true,
        host_a_offline_marker_verified: offlineProof,
        task_b_ready_marker_verified: taskBReady,
        task_b_terminal_marker_verified: await markerMatches(path.join(runDir, "task-b-complete.proof"), TASK_B_COMPLETE_MARKER),
        task_b_terminal_wait_started: progress.taskBTerminalWaitStarted === true,
        source_acked_revision: diagnostics?.source?.acked_through_revision ?? null,
        source_head_revision: diagnostics?.source?.source_head_revision ?? null,
        source_cloud_head: diagnostics?.source?.cloud_head_seq ?? null,
        source_gap_count: diagnostics?.source?.gap_count ?? null,
        worker_status: diagnostics?.worker_context_status?.state ?? "unavailable",
        worker_last_cloud_seq: diagnostics?.worker_context_status?.worker_last_cloud_seq ?? null,
        worker_lag: diagnostics?.worker_context_status?.worker_lag ?? null,
        live_qualification: "not_qualified",
      },
    };
    await fs.writeFile(
      path.join(runDir, `${metadata.phase}-run-failure.json`),
      JSON.stringify(manifest, null, 2) + "\n",
      { mode: 0o600, flag: "wx" },
    );
  } catch {
    // Failure manifests must not mask the original pipeline failure.
  }
}

function parseArgs(argv) {
  const result = { flags: {}, positional: [] };
  const boolean = new Set(["help"]);
  const valued = new Set([
    "phase", "mode", "extractor", "binary", "baseline-binary", "candidate-binary", "baseline-gateway-root",
    "output", "backend", "model", "effort", "extractor-profile", "extractor-endpoint",
    "extractor-model", "extractor-reasoning-effort", "auth-file", "python", "task-polls", "poll-interval",
    "host-wait-seconds", "pipeline-wait-seconds",
  ]);
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg.startsWith("--")) {
      result.positional.push(arg);
      continue;
    }
    const [key, inlineValue] = arg.slice(2).split("=", 2);
    if (boolean.has(key)) {
      if (inlineValue !== undefined) throw new HarnessError("INVALID_ARGUMENT");
      result.flags[key] = true;
      continue;
    }
    if (!valued.has(key)) throw new HarnessError("UNKNOWN_ARGUMENT");
    const value = inlineValue ?? argv[++i];
    if (typeof value !== "string" || value.startsWith("--")) throw new HarnessError("MISSING_ARGUMENT_VALUE");
    if (Object.hasOwn(result.flags, key)) throw new HarnessError("DUPLICATE_ARGUMENT");
    result.flags[key] = value;
  }
  return result.flags;
}

function printHelp() {
  process.stdout.write([
    "Usage:",
    "  node gateway/scripts/memory-dogfood.mjs --phase baseline|candidate --mode fixture|live",
    "    [--extractor disabled|fixture|live] [--baseline-binary PATH] [--candidate-binary PATH]",
    "    [--output RUN_DIR] [--backend codex] [--model MODEL] [--effort EFFORT]",
    "    [--extractor-profile PROFILE] [--extractor-endpoint URL] [--extractor-model MODEL]",
    "    [--extractor-reasoning-effort low|medium|high|minimal|none|max|xhigh]",
    "",
    "Fixture mode runs the deterministic real-D1/Queue integration test and is never live qualification.",
    "Live mode starts dedicated Temote supervisors and host agents in isolated XDG_STATE_HOME roots.",
    "Reasoning effort is sent only when explicitly set by this flag or TEMOTE_MCP_MEMORY_REASONING_EFFORT.",
  ].join("\n") + "\n");
}

function checkSafeRelativeArtifactPath(filePath) {
  const absolute = path.resolve(filePath);
  const runsRoot = path.join(ROOT, "dogfood/runs") + path.sep;
  if (!absolute.startsWith(runsRoot)) throw new HarnessError("ARTIFACT_PATH_MUST_BE_IGNORED_RUNS");
  return absolute;
}

function mkdirPrivate(directory) {
  return fs.mkdir(directory, { recursive: true, mode: 0o700 }).then(() => fs.chmod(directory, 0o700));
}

function safeEnv(source, { xdgStateHome, socketNamespace, hostId, gatewayUrl, hostToken, preview = false } = {}) {
  const env = { ...source, XDG_STATE_HOME: xdgStateHome };
  for (const name of Object.keys(env)) {
    if (name.startsWith("TEMOTE_MCP_GATEWAY_")) delete env[name];
  }
  delete env.TEMOTE_MCP_OBSERVATION_SYNC_PREVIEW;
  if (socketNamespace) env.TEMOTE_MCP_SOCKET_NAMESPACE = socketNamespace;
  if (hostId) env.TEMOTE_MCP_HOST_ID = hostId;
  if (gatewayUrl) env.TEMOTE_MCP_GATEWAY_URL = gatewayUrl;
  if (hostId) env.TEMOTE_MCP_GATEWAY_HOST_ID = hostId;
  if (hostToken) env.TEMOTE_MCP_GATEWAY_HOST_TOKEN = hostToken;
  if (preview) env.TEMOTE_MCP_OBSERVATION_SYNC_PREVIEW = "true";
  return env;
}

function runCommand(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd,
    env: options.env,
    encoding: "utf8",
    timeout: options.timeout ?? 30_000,
    maxBuffer: 2 * MAX_ARTIFACT_BYTES,
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (result.error || result.status !== 0) {
    throw new HarnessError(options.failureCode ?? "CHILD_COMMAND_FAILED");
  }
  return result.stdout ?? "";
}

function launch(command, args, options = {}) {
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: options.env,
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  child.stdoutTail = "";
  child.stderrTail = "";
  for (const [stream, property] of [[child.stdout, "stdoutTail"], [child.stderr, "stderrTail"]]) {
    stream?.on("data", (chunk) => {
      child[property] = (child[property] + chunk.toString("utf8")).slice(-8192);
    });
  }
  child.once("error", () => {});
  return child;
}

function childFailureClass(child) {
  const tail = `${child?.stderrTail ?? ""}\n${child?.stdoutTail ?? ""}`.toLowerCase();
  if (tail.includes("temote_mcp_socket_namespace")) return "SOCKET_NAMESPACE_INVALID";
  if (tail.includes("unauthorized") || /http status (401|403)/.test(tail)) return "GATEWAY_AUTH_FAILED";
  if (tail.includes("connection refused") || tail.includes("failed to connect")) return "GATEWAY_CONNECT_FAILED";
  if (tail.includes("invalid gateway url") || tail.includes("gateway url")) return "GATEWAY_URL_INVALID";
  if (child?.signalCode) return "CHILD_SIGNALED";
  if (typeof child?.exitCode === "number") return `CHILD_EXIT_${child.exitCode}`;
  return "CHILD_START_FAILED";
}

function childExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

async function waitUntil(predicate, timeoutMs, intervalMs = 100, signal = undefined) {
  const deadline = Date.now() + timeoutMs;
  while (!signal?.aborted && Date.now() <= deadline) {
    if (await predicate()) return true;
    try {
      await delay(intervalMs, undefined, signal ? { signal } : undefined);
    } catch (error) {
      if (signal?.aborted && error?.name === "AbortError") return false;
      throw error;
    }
  }
  return false;
}

async function stopOwnedProcess(child, timeoutMs = 5_000, initialSignal = "SIGTERM") {
  if (!child || childExited(child)) return "already_exited";
  child.kill(initialSignal);
  const exited = await waitUntil(() => childExited(child), timeoutMs, 50);
  if (exited || childExited(child)) {
    if (initialSignal !== "SIGINT") return "signaled";
    return child.exitCode === 0 ? "graceful" : "unexpected_exit";
  }
  if (!childExited(child)) {
    child.kill("SIGKILL");
    const killed = await waitUntil(() => childExited(child), 2_000, 50);
    return killed || childExited(child) ? "forced" : "still_running";
  }
  return "forced";
}

function parseMcpText(response, name) {
  if (response.status !== 200 || response.body?.error) throw new HarnessError(`${name.toUpperCase()}_FAILED`);
  const text = response.body?.result?.content?.find((part) => part?.type === "text")?.text;
  if (typeof text !== "string") throw new HarnessError(`${name.toUpperCase()}_RESULT_INVALID`);
  try {
    return JSON.parse(text);
  } catch {
    throw new HarnessError(`${name.toUpperCase()}_RESULT_INVALID`);
  }
}

export async function hostIsPresent(runtime, token, hostId) {
  const result = parseMcpText(
    await runtime.callMcp(token, "host_list", {}, Math.floor(Math.random() * 1_000_000_000)),
    "host_list",
  );
  if (!Array.isArray(result)) throw new HarnessError("HOST_LIST_RESULT_INVALID");
  if (result.some((host) => !host || typeof host !== "object" || Array.isArray(host)
      || typeof host.host_id !== "string" || host.host_id.length === 0)) {
    throw new HarnessError("HOST_LIST_RESULT_INVALID");
  }
  return result.some((host) => host.host_id === hostId);
}

export async function waitForHost(runtime, token, hostId, present, timeoutMs, pollIntervalMs = 250) {
  let lastDiscoveryError = null;
  const ok = await waitUntil(async () => {
    try {
      const actual = await hostIsPresent(runtime, token, hostId);
      lastDiscoveryError = null;
      return actual === present;
    } catch (error) {
      lastDiscoveryError = error;
      return false;
    }
  }, timeoutMs, pollIntervalMs);
  if (!ok) {
    if (lastDiscoveryError) throw new HarnessError("HOST_LIST_UNAVAILABLE");
    throw new HarnessError(present ? "DEDICATED_HOST_CONNECT_TIMEOUT" : "DEDICATED_HOST_OFFLINE_TIMEOUT");
  }
}

export async function stopDedicatedHostAndConfirmOffline({
  host,
  runtime,
  token,
  proofPath,
  budgetMs,
  signalTimeoutMs = 5_000,
  gracefulAbsenceTimeoutMs = HOST_GRACEFUL_OFFLINE_WAIT_MS,
  leaseMs = HOST_LEASE_MS,
  leaseMarginMs = HOST_LEASE_MARGIN_MS,
  stopProcess = stopOwnedProcess,
  waitHost = waitForHost,
  writeProof = (filePath) => fs.writeFile(filePath, OFFLINE_MARKER, { mode: 0o600, flag: "wx" }),
}) {
  if (!Number.isSafeInteger(budgetMs) || budgetMs <= 0) {
    throw new HarnessError("HOST_OFFLINE_BUDGET_EXCEEDED");
  }
  const startedAt = Date.now();
  const termination = await stopProcess(host.hostAgent, signalTimeoutMs, "SIGINT");
  if (termination === "still_running") throw new HarnessError("HOST_AGENT_STOP_TIMEOUT");
  const remainingBudget = () => Number.isSafeInteger(budgetMs) ? Math.max(0, budgetMs - (Date.now() - startedAt)) : 0;
  let leaseFallback = termination !== "graceful";
  if (!leaseFallback) {
    const gracefulWait = Math.min(gracefulAbsenceTimeoutMs, remainingBudget());
    if (gracefulWait <= 0) throw new HarnessError("HOST_OFFLINE_BUDGET_EXCEEDED");
    try {
      await waitHost(runtime, token, host.hostId, false, gracefulWait);
    } catch (error) {
      if (!(error instanceof HarnessError) || error.code !== "DEDICATED_HOST_OFFLINE_TIMEOUT") throw error;
      leaseFallback = true;
    }
  }
  if (leaseFallback) {
    const fallbackWait = Math.min(leaseMs + leaseMarginMs, remainingBudget());
    if (fallbackWait <= 0) throw new HarnessError("HOST_OFFLINE_BUDGET_EXCEEDED");
    try {
      await waitHost(runtime, token, host.hostId, false, fallbackWait);
    } catch (error) {
      if (error instanceof HarnessError && error.code === "DEDICATED_HOST_OFFLINE_TIMEOUT"
          && fallbackWait < leaseMs + leaseMarginMs) {
        throw new HarnessError("HOST_OFFLINE_BUDGET_EXCEEDED");
      }
      throw error;
    }
  }
  await writeProof(proofPath);
  return { termination, lease_fallback: leaseFallback, offline_confirmed: true };
}

async function loadExtractorKey(authFile) {
  let parsed;
  try {
    parsed = JSON.parse(await fs.readFile(authFile, "utf8"));
  } catch {
    throw new HarnessError("EXTRACTOR_CREDENTIAL_UNAVAILABLE");
  }
  const profile = parsed?.["opencode-go"];
  const key = profile?.key ?? profile?.apiKey ?? profile?.access;
  if (typeof key !== "string" || key.length < 8 || key.length > 8192) {
    throw new HarnessError("EXTRACTOR_CREDENTIAL_UNAVAILABLE");
  }
  return key;
}

async function resolveBinary(flags, phase) {
  const candidate = path.resolve(flags["candidate-binary"] ?? flags.binary ?? DEFAULT_CANDIDATE);
  const baseline = path.resolve(flags["baseline-binary"] ?? DEFAULT_BASELINE);
  const binary = phase === "baseline" ? baseline : candidate;
  try {
    const stat = await fs.stat(binary);
    if (!stat.isFile()) throw new HarnessError("TEMOTE_BINARY_NOT_FOUND");
    return binary;
  } catch {
    throw new HarnessError("TEMOTE_BINARY_NOT_FOUND");
  }
}

function execFile(file, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(file, args, {
      cwd: options.cwd,
      env: options.env,
      stdio: ["ignore", "ignore", "ignore"],
      windowsHide: true,
    });
    child.once("error", () => reject(new HarnessError(options.failureCode ?? "DRIVER_START_FAILED")));
    child.once("exit", (code) => resolve({ code: code ?? 1, child }));
    options.onStart?.(child);
  });
}

async function readScenario() {
  try {
    const scenario = JSON.parse(await fs.readFile(path.join(ROOT, "dogfood/scenarios/memory-continuity.json"), "utf8"));
    if (scenario.id !== "memory-continuity" || scenario.revision !== 1
        || scenario.repository_key !== REPOSITORY_KEY || !Array.isArray(scenario.assertions)) {
      throw new HarnessError("SCENARIO_CONTRACT_INVALID");
    }
    return scenario;
  } catch (error) {
    if (error instanceof HarnessError) throw error;
    throw new HarnessError("SCENARIO_UNAVAILABLE");
  }
}

async function createGitWorkspace(parent, name) {
  const workspace = path.join(parent, name);
  await mkdirPrivate(workspace);
  runCommand("git", ["init", "--quiet", workspace]);
  runCommand("git", ["-C", workspace, "config", "user.name", "Temote memory dogfood"]);
  runCommand("git", ["-C", workspace, "config", "user.email", "temote-memory-dogfood@localhost"]);
  await fs.writeFile(path.join(workspace, "README.md"), "Disposable memory continuity scenario workspace.\n", { mode: 0o600 });
  runCommand("git", ["-C", workspace, "add", "README.md"]);
  runCommand("git", ["-C", workspace, "commit", "--quiet", "-m", "scenario base"]);
  runCommand("git", ["-C", workspace, "remote", "add", "origin", "https://github.com/temote-tests/memory-continuity.git"]);
  return workspace;
}

async function createRunWorkspace(runDir) {
  const repoA = await createGitWorkspace(runDir, "host-a-repo");
  const repoB = await createGitWorkspace(runDir, "host-b-repo");
  return { repoA, repoB };
}

async function copyTaskAHeadToHostB(repoA, repoB) {
  const entries = await fs.readdir(repoA, { withFileTypes: true });
  for (const entry of entries) {
    if (entry.name === ".git") continue;
    const source = path.join(repoA, entry.name);
    const target = path.join(repoB, entry.name);
    await fs.rm(target, { recursive: true, force: true });
    await fs.cp(source, target, { recursive: true, preserveTimestamps: false, errorOnExist: false });
  }
}

async function createDedicatedHost({ binary, runtime, runDir, label, repo, hostId, tokens, processList, preview }) {
  const generatedHostId = hostId ?? `memory-dogfood-${label}-${randomBytes(6).toString("hex")}`;
  const suffix = generatedHostId.split("-").at(-1) ?? randomBytes(6).toString("hex");
  const sessionId = generatedHostId;
  const socketNamespace = `md${label}${suffix.slice(0, 8)}`;
  const stateHome = path.resolve(runDir, `.state-${label}`);
  await mkdirPrivate(stateHome);
  const supervisorEnv = safeEnv(process.env, { xdgStateHome: stateHome, socketNamespace, hostId: generatedHostId });

  const supervisor = launch(binary, ["supervisor"], { cwd: repo, env: supervisorEnv });
  processList.push(supervisor);
  const supervisorReady = await waitUntil(async () => {
    if (childExited(supervisor)) return false;
    try {
      runCommand(binary, ["session", "list"], { cwd: repo, env: supervisorEnv, timeout: 5_000 });
      return true;
    } catch {
      return false;
    }
  }, 20_000, 100);
  if (!supervisorReady) throw new HarnessError(`SUPERVISOR_${label.toUpperCase()}_START_FAILED`);

  runCommand(binary, ["start", sessionId], {
    cwd: repo,
    env: supervisorEnv,
    timeout: 60_000,
    failureCode: `SESSION_${label.toUpperCase()}_START_FAILED`,
  });

  const hostEnv = safeEnv(process.env, {
    xdgStateHome: stateHome,
    socketNamespace,
    hostId: generatedHostId,
    gatewayUrl: runtime.baseUrl,
    hostToken: tokens[generatedHostId],
    preview,
  });
  const hostAgent = launch(binary, [
    "gateway-agent", "--host-id", generatedHostId, "--gateway-url", runtime.baseUrl,
    "--platform", process.platform === "linux" ? "linux" : "auto",
    "--reconnect-delay-seconds", "1",
  ], { cwd: repo, env: hostEnv });
  processList.push(hostAgent);
  return { label, hostId: generatedHostId, sessionId, stateHome, socketNamespace, repo, supervisor, hostAgent, env: supervisorEnv };
}

async function startMcpReady(runtime, token, hosts, waitMs) {
  for (const host of hosts) {
    const ready = await waitUntil(async () => {
      if (childExited(host.hostAgent) || childExited(host.supervisor)) return false;
      try {
        return await hostIsPresent(runtime, token, host.hostId);
      } catch {
        return false;
      }
    }, waitMs, 250);
    if (!ready) {
      if (childExited(host.hostAgent)) {
        throw new HarnessError(`HOST_AGENT_${host.label.toUpperCase()}_${childFailureClass(host.hostAgent)}`);
      }
      if (childExited(host.supervisor)) {
        throw new HarnessError(`SUPERVISOR_${host.label.toUpperCase()}_${childFailureClass(host.supervisor)}`);
      }
      throw new HarnessError("DEDICATED_HOST_CONNECT_TIMEOUT");
    }
  }
}

async function startDriver({
  phase, mode, extractor, binary, flags, runtime, clientToken, hostA, hostB, runDir,
  extractorProfile, python,
}) {
  const artifactPath = path.join(runDir, `${phase}-scenario.json`);
  const args = [
    path.join(ROOT, "dogfood/memory_continuity.py"), phase,
    "--mode", mode,
    "--synthesis-mode", phase === "baseline" ? "not_run" : extractor === "live" ? "live" : "fixture",
    "--binary", binary,
    "--output", artifactPath,
    "--task-a-complete-file", path.join(runDir, "task-a-complete.proof"),
    "--task-b-ready-file", path.join(runDir, "task-b-ready.proof"),
    "--task-b-complete-file", path.join(runDir, "task-b-complete.proof"),
    "--fabric-url", runtime.endpoint,
    "--session-a", hostA.sessionId,
    "--session-b", hostB.sessionId,
    "--host-a", hostA.hostId,
    "--host-b", hostB.hostId,
    "--backend", flags.backend ?? DEFAULT_BACKEND,
    "--model", flags.model ?? DEFAULT_TASK_MODEL,
    "--effort", flags.effort ?? DEFAULT_TASK_EFFORT,
    "--extractor-profile", extractorProfile,
    "--max-polls", flags["task-polls"] ?? "120",
    "--poll-interval", flags["poll-interval"] ?? "1",
  ];
  if (phase === "baseline") {
    // This archive is the exact pre-change source snapshot retained for the
    // baseline. Its archived worktree is immutable and has no working diff.
    args.push(
      "--source-head", "e0d6c7674f4d8d43999c77979687ca37cdd04ea7",
      "--working-diff-sha256", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
  }
  if (phase === "candidate") {
    args.push(
      "--offline-proof-file", path.join(runDir, "host-a-offline.proof"),
      "--offline-proof-wait-seconds", flags["pipeline-wait-seconds"] ?? "240",
      "--queue-replay-manifest", path.join(runDir, "queue-replay.json"),
    );
  }
  const env = {
    ...process.env,
    TEMOTE_MCP_FABRIC_CLIENT_TOKEN: clientToken,
    TEMOTE_MCP_MEMORY_TASK_MODEL: flags.model ?? DEFAULT_TASK_MODEL,
    TEMOTE_MCP_MEMORY_TASK_EFFORT: flags.effort ?? DEFAULT_TASK_EFFORT,
  };
  const child = spawn(python, args, {
    cwd: ROOT,
    env,
    stdio: ["ignore", "ignore", "ignore"],
    windowsHide: true,
  });
  child.once("error", () => {});
  const exit = new Promise((resolve) => {
    if (childExited(child)) resolve({ code: child.exitCode ?? 1, signal: child.signalCode });
    else child.once("exit", (code, signal) => resolve({ code: code ?? 1, signal }));
  });
  return { child, artifactPath, exit };
}

export async function waitForMarker(
  filePath,
  expected,
  timeoutMs,
  driver,
  {
    timeoutCode = "DOGFOOD_HANDSHAKE_TIMEOUT",
    driverExitCode = "DOGFOOD_DRIVER_EXITED_BEFORE_HANDSHAKE",
    intervalMs = 50,
    markerReader = async (file, marker) => (await fs.readFile(file, "utf8")) === marker,
  } = {},
) {
  const abort = new AbortController();
  let result;
  try {
    result = await Promise.race([
      waitUntil(async () => {
        try {
          return await markerReader(filePath, expected);
        } catch {
          return false;
        }
      }, timeoutMs, intervalMs, abort.signal).then((ok) => ok ? "marker" : "timeout"),
      driver.exit.then(async () => {
        // The driver writes the terminal marker immediately before exit. A final
        // read preserves that proof if process exit wins the polling interval.
        try {
          return await markerReader(filePath, expected) ? "marker" : "driver_exit";
        } catch {
          return "driver_exit";
        }
      }),
    ]);
  } finally {
    // A long task-terminal budget must not leave its polling timer alive after
    // the driver has already terminated or the exact marker has arrived.
    abort.abort();
  }
  if (result !== "marker") throw new HarnessError(result === "timeout" ? timeoutCode : driverExitCode);
}

export async function waitForTaskBTerminalBeforeProjection({ markerPath, timeoutMs, driver, project, intervalMs = 50, markerReader }) {
  await waitForMarker(markerPath, TASK_B_COMPLETE_MARKER, timeoutMs, driver, {
    timeoutCode: "TASK_B_TERMINAL_TIMEOUT",
    driverExitCode: "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_B_TERMINAL",
    intervalMs,
    ...(markerReader ? { markerReader } : {}),
  });
  return project();
}

async function sourceStatus(runtime, hostId, sessionId) {
  const rows = await runtime.querySql(
    [
      "SELECT source_head_revision, acked_through_revision, repository_key, journal_degraded, gap_count",
      "FROM observation_sources WHERE owner_id = ? AND host_id = ? AND session_id = ?",
    ].join(" "),
    [OWNER_ID, hostId, sessionId],
  );
  if (rows.length !== 1) return null;
  const source = rows[0];
  const terminal = await runtime.querySql(
    [
      "SELECT COUNT(*) AS count FROM observations",
      "WHERE owner_id = ? AND host_id = ? AND session_id = ? AND repository_key = ?",
      "AND kind = 'execution_state' AND state_status IN ('completed', 'failed', 'interrupted')",
    ].join(" "),
    [OWNER_ID, hostId, sessionId, REPOSITORY_KEY],
  );
  return {
    source,
    terminalCount: Number(terminal[0]?.count ?? 0),
    complete: Number(source.acked_through_revision) >= Number(source.source_head_revision)
      && Number(source.journal_degraded) === 0 && Number(source.gap_count) === 0,
  };
}

async function knowledgeStatus(runtime, text) {
  const rows = await runtime.querySql(
    [
      "SELECT item.knowledge_id, item.status, item.scope_type, item.scope_id,",
      "(SELECT COUNT(*) FROM knowledge_support AS support",
      "WHERE support.owner_id = item.owner_id AND support.repository_key = item.repository_key",
      "AND support.knowledge_id = item.knowledge_id) AS support_count",
      "FROM knowledge_items AS item WHERE item.owner_id = ? AND item.repository_key = ?",
      "AND item.kind = 'constraint' AND item.text = ? AND item.status = 'current'",
    ].join(" "),
    [OWNER_ID, REPOSITORY_KEY, text],
  );
  return rows.some((row) => row.scope_type === "repository"
    && row.scope_id === REPOSITORY_KEY && Number(row.support_count) > 0);
}

async function repositoryCheckpointReady(runtime) {
  const rows = await runtime.querySql(
    [
      "SELECT checkpoint.last_cloud_seq, MAX(observations.cloud_seq) AS latest_cloud_seq",
      "FROM memory_checkpoints AS checkpoint JOIN observations",
      "ON observations.owner_id = checkpoint.owner_id AND observations.repository_key = checkpoint.repository_key",
      "WHERE checkpoint.owner_id = ? AND checkpoint.repository_key = ?",
      "GROUP BY checkpoint.last_cloud_seq ORDER BY checkpoint.last_cloud_seq DESC LIMIT 1",
    ].join(" "),
    [OWNER_ID, REPOSITORY_KEY],
  );
  return rows.length > 0 && Number(rows[0].last_cloud_seq) >= Number(rows[0].latest_cloud_seq);
}

async function waitForTaskProjection(runtime, host, constraint, timeoutMs, signal = undefined) {
  const end = Date.now() + timeoutMs;
  while (!signal?.aborted && Date.now() <= end) {
    const source = await sourceStatus(runtime, host.hostId, host.sessionId);
    if (signal?.aborted) throw new HarnessError("PROJECTION_WAIT_ABORTED");
    if (source?.complete && source.terminalCount > 0) {
      const checkpointReady = await repositoryCheckpointReady(runtime);
      if (signal?.aborted) throw new HarnessError("PROJECTION_WAIT_ABORTED");
      if (checkpointReady) {
        const hasKnowledge = await knowledgeStatus(runtime, constraint);
        if (signal?.aborted) throw new HarnessError("PROJECTION_WAIT_ABORTED");
        if (hasKnowledge) return source;
      }
    }
    if (signal?.aborted) throw new HarnessError("PROJECTION_WAIT_ABORTED");
    try {
      await delay(250, undefined, signal ? { signal } : undefined);
    } catch (error) {
      if (signal?.aborted && error?.name === "AbortError") {
        throw new HarnessError("PROJECTION_WAIT_ABORTED");
      }
      throw error;
    }
  }
  if (signal?.aborted) throw new HarnessError("PROJECTION_WAIT_ABORTED");
  throw new HarnessError(`PROJECTION_${host.label.toUpperCase()}_TIMEOUT`);
}

async function countFor(runtime, table) {
  const allowed = new Set(["knowledge_items", "knowledge_support", "knowledge_supersession", "memory_runs"]);
  if (!allowed.has(table)) throw new HarnessError("INVALID_PROJECTION_TABLE");
  const rows = await runtime.querySql(
    `SELECT COUNT(*) AS count FROM ${table} WHERE owner_id = ? AND repository_key = ?`,
    [OWNER_ID, REPOSITORY_KEY],
  );
  return Number(rows[0]?.count ?? 0);
}

async function checkpointFor(runtime) {
  const rows = await runtime.querySql(
    [
      "SELECT MAX(last_cloud_seq) AS last_cloud_seq FROM memory_checkpoints",
      "WHERE owner_id = ? AND repository_key = ?",
    ].join(" "),
    [OWNER_ID, REPOSITORY_KEY],
  );
  return Number(rows[0]?.last_cloud_seq ?? 0);
}

async function captureProjection(runtime) {
  const [knowledge, supports, supersessions, runs] = await Promise.all([
    countFor(runtime, "knowledge_items"),
    countFor(runtime, "knowledge_support"),
    countFor(runtime, "knowledge_supersession"),
    countFor(runtime, "memory_runs"),
  ]);
  return { knowledge, supports, supersessions, runs, checkpoint: await checkpointFor(runtime) };
}

async function replayQueueAndWriteManifest(runtime, runDir) {
  const settled = await waitUntil(async () => {
    const rows = await runtime.queueDispatches();
    return rows.every((row) => row.state === "completed" || row.state === "failed");
  }, 10_000, 50);
  if (!settled) throw new HarnessError("QUEUE_HAS_UNSETTLED_DISPATCH");
  const outbox = await runtime.querySql(
    "SELECT through_cloud_seq FROM memory_outbox WHERE owner_id = ? AND repository_key = ?",
    [OWNER_ID, REPOSITORY_KEY],
  );
  if (outbox.length !== 1) throw new HarnessError("MEMORY_OUTBOX_MISSING");
  const before = await captureProjection(runtime);
  const dispatchBefore = await runtime.queueDispatchCount();
  await runtime.enqueue({
    owner_id: OWNER_ID,
    repository_key: REPOSITORY_KEY,
    through_cloud_seq: Number(outbox[0].through_cloud_seq),
  });
  const dispatched = await waitUntil(async () => {
    const rows = await runtime.queueDispatches();
    const replayRows = rows.slice(dispatchBefore);
    return replayRows.length > 0
      && replayRows.every((row) => row.state === "completed" && row.completed_at != null);
  }, 15_000, 50);
  if (!dispatched) throw new HarnessError("QUEUE_REPLAY_NOT_CONSUMED");
  const dispatchAfter = await runtime.queueDispatchCount();
  const after = await captureProjection(runtime);
  const countsEqual = before.knowledge === after.knowledge
    && before.supports === after.supports
    && before.supersessions === after.supersessions
    && before.runs === after.runs
    && before.checkpoint === after.checkpoint;
  if (!countsEqual) throw new HarnessError("QUEUE_REPLAY_CHANGED_PROJECTION");
  const manifest = {
    schema_version: 1,
    repository_key: REPOSITORY_KEY,
    dispatch_count_before: dispatchBefore,
    dispatch_count_after: dispatchAfter,
    knowledge_count_before: before.knowledge,
    knowledge_count_after: after.knowledge,
    support_count_before: before.supports,
    support_count_after: after.supports,
    supersession_count_before: before.supersessions,
    supersession_count_after: after.supersessions,
    run_count_before: before.runs,
    run_count_after: after.runs,
    checkpoint_before: before.checkpoint,
    checkpoint_after: after.checkpoint,
  };
  const target = path.join(runDir, "queue-replay.json");
  await fs.writeFile(target, JSON.stringify(manifest) + "\n", { mode: 0o600, flag: "wx" });
}

async function waitForDriver(driver, timeoutMs) {
  const abort = new AbortController();
  let result;
  try {
    result = await Promise.race([
      driver.exit,
      delay(timeoutMs, null, { signal: abort.signal }).catch((error) => {
        if (error?.name === "AbortError") return "aborted";
        throw error;
      }),
    ]);
  } finally {
    abort.abort();
  }
  if (result === "aborted") result = null;
  if (result === null) {
    driver.child.kill("SIGTERM");
    await waitUntil(() => childExited(driver.child), 3_000, 50);
    if (!childExited(driver.child)) driver.child.kill("SIGKILL");
    throw new HarnessError("DOGFOOD_DRIVER_TIMEOUT");
  }
  return result.code;
}

async function waitForProjectionWhileDriverRuns(runtime, clientToken, host, constraint, timeoutMs, driver, runDir, metadata) {
  const abort = new AbortController();
  try {
    const result = await Promise.race([
      waitForTaskProjection(runtime, host, constraint, timeoutMs, abort.signal).then(() => "projected"),
      driver.exit.then(({ code }) => {
        abort.abort();
        return `driver_exit_${code}`;
      }),
    ]);
    if (result !== "projected") throw new HarnessError("DOGFOOD_DRIVER_EXITED_BEFORE_PROJECTION");
  } catch (error) {
    if (error instanceof HarnessError
        && (error.code.startsWith("PROJECTION_") || error.code === "DOGFOOD_DRIVER_EXITED_BEFORE_PROJECTION")) {
      const diagnostics = await writeProjectionDiagnostics(runtime, clientToken, host, runDir);
      await writeProjectionFailureManifest(runDir, metadata, host, diagnostics);
    }
    throw error;
  } finally {
    abort.abort();
  }
}

async function runFixture(phase, runDir) {
  const driverArtifact = path.join(runDir, `${phase}-fixture-status.json`);
  const python = process.env.PYTHON ?? "python3";
  const result = await execFile(python, [
    path.join(ROOT, "dogfood/memory_continuity.py"), phase, "--mode", "fixture", "--output", driverArtifact,
  ], { cwd: ROOT, failureCode: "DOGFOOD_FIXTURE_DRIVER_FAILED" });
  const fixtureArtifact = JSON.parse(await fs.readFile(driverArtifact, "utf8"));
  const expectedFixtureNotRun = result.code === 1
    && fixtureArtifact.phase === phase
    && fixtureArtifact.mode === "fixture"
    && fixtureArtifact.synthesis_mode === "not_run"
    && fixtureArtifact.outcome === "not_run"
    && Object.values(fixtureArtifact.gates ?? {}).every((state) => state === "not_run");
  if (!expectedFixtureNotRun) throw new HarnessError("DOGFOOD_FIXTURE_DRIVER_FAILED");
  const test = spawnSync(process.execPath, ["--test", "gateway/test/automatic-memory-pipeline.test.mjs"], {
    cwd: ROOT,
    encoding: "utf8",
    timeout: 60_000,
    maxBuffer: 2 * MAX_ARTIFACT_BYTES,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const summary = {
    schema_version: 1,
    phase,
    mode: "fixture",
    live_synthesis: "NOT RUN",
    fixture_status: fixtureArtifact.outcome,
    integration_test: test.status === 0 ? "pass" : "fail",
    qualification: "not_qualified",
    status_artifact: path.relative(ROOT, driverArtifact),
  };
  if (test.status !== 0) throw new HarnessError("AUTOMATIC_MEMORY_FIXTURE_TEST_FAILED");
  return summary;
}

async function runLive(flags, phase, extractor, runDir) {
  const scenario = await readScenario();
  const binary = await resolveBinary(flags, phase);
  const taskPollSettings = taskTerminalWaitBudgetFromFlags(flags);
  const projectionWaitMs = projectionWaitMsFromFlags(flags);
  const backend = flags.backend ?? DEFAULT_BACKEND;
  const model = flags.model ?? DEFAULT_TASK_MODEL;
  const effort = flags.effort ?? DEFAULT_TASK_EFFORT;
  const extractorEndpoint = flags["extractor-endpoint"] ?? process.env.TEMOTE_MCP_MEMORY_ENDPOINT ?? DEFAULT_ENDPOINT;
  const extractorModel = flags["extractor-model"] ?? process.env.TEMOTE_MCP_MEMORY_MODEL ?? DEFAULT_EXTRACTOR_MODEL;
  const extractorProfile = flags["extractor-profile"] ?? `opencode-go/${extractorModel}`;
  const reasoningEffort = resolveExtractorReasoningEffort({
    explicitValue: flags["extractor-reasoning-effort"],
    environmentValue: process.env.TEMOTE_MCP_MEMORY_REASONING_EFFORT,
    extractor,
  });
  if (backend !== "codex" || !model || !effort) throw new HarnessError("CODING_BACKEND_SELECTOR_UNSUPPORTED");
  const selectors = { backend, model, effort, extractor_profile: extractorProfile };
  const extractorConfig = memoryExtractorSettings(
    extractor === "live" ? extractorModel : null,
    extractor === "live" ? reasoningEffort : null,
  );
  const metadata = {
    phase, extractor, selectors, extractorConfig,
    timing: {
      task_terminal_wait_ms: taskPollSettings.waitMs,
      projection_wait_ms: projectionWaitMs,
      task_polls: taskPollSettings.maxPolls,
      poll_interval_seconds: taskPollSettings.pollIntervalSeconds,
      mcp_rpc_timeout_ms: MCP_RPC_TIMEOUT_MS,
    },
    provenance: await collectRunProvenance(binary, scenario),
  };

  const hostAId = `memory-dogfood-a-${randomBytes(6).toString("hex")}`;
  const hostBId = `memory-dogfood-b-${randomBytes(6).toString("hex")}`;
  const clientToken = randomBytes(32).toString("base64url");
  const hostTokens = {
    [hostAId]: randomBytes(32).toString("base64url"),
    [hostBId]: randomBytes(32).toString("base64url"),
  };
  const liveKey = extractor === "live"
    ? await loadExtractorKey(path.resolve(flags["auth-file"] ?? path.join(os.homedir(), ".local/share/opencode/auth.json")))
    : "fixture-provider-key-unused";
  const baselineGatewayRoot = path.resolve(flags["baseline-gateway-root"] ?? DEFAULT_BASELINE_GATEWAY);
  const baselineMigrations = [
    path.join(baselineGatewayRoot, "migrations/0001_observation_knowledge.sql"),
    path.join(baselineGatewayRoot, "migrations/0002_observation_ingest.sql"),
  ];
  const runtime = await startMemoryRuntime({
    memoryExtractor: extractor === "disabled" ? "fixture" : extractor === "live" ? "openai_compatible" : "fixture",
    memoryEnabled: phase === "candidate" && extractor !== "disabled",
    memoryEndpoint: extractor === "live" ? extractorEndpoint : "https://fixture.invalid/v1/chat/completions",
    memoryModel: extractor === "live" ? extractorModel : "fixture-strict-continuity-v1",
    memoryApiKey: liveKey,
    ...(phase === "baseline" ? {
      productionEntryPath: path.join(baselineGatewayRoot, "src/index.js"),
      migrations: baselineMigrations,
    } : {}),
    bindings: {
      CLIENT_TOKEN: clientToken,
      HOST_TOKENS_JSON: JSON.stringify(hostTokens),
      OBSERVATION_OWNER_ID: OWNER_ID,
      MEMORY_TIMEOUT_MS: String(MEMORY_TIMEOUT_MS),
      MEMORY_INPUT_BUDGET_BYTES: String(MEMORY_INPUT_BUDGET_BYTES),
      MEMORY_OUTPUT_BUDGET_BYTES: String(MEMORY_OUTPUT_BUDGET_BYTES),
      MEMORY_MAX_ATTEMPTS: String(MEMORY_MAX_ATTEMPTS),
      MEMORY_BATCH_SIZE: String(MEMORY_BATCH_SIZE),
      ...(extractor === "live" && reasoningEffort
        ? { MEMORY_REASONING_EFFORT: reasoningEffort }
        : {}),
    },
  });

  const children = [];
  let driver;
  let hosts = [];
  let successful = false;
  const progress = {
    stage: "runtime_started",
    diagnosticHostLabel: null,
    taskAProjectionPredicatePassed: false,
    taskBProjectionPredicatePassed: false,
    queueReplayPassed: false,
    offlineCheckAttempted: false,
    taskATerminalWaitStarted: false,
    taskBTerminalWaitStarted: false,
    taskTerminalWaitMs: taskPollSettings.waitMs,
    projectionWaitMs,
    taskPolls: taskPollSettings.maxPolls,
    pollIntervalSeconds: taskPollSettings.pollIntervalSeconds,
  };
  try {
    const { repoA, repoB } = await createRunWorkspace(runDir);
    const hostA = await createDedicatedHost({
      binary, runtime, runDir, label: "a", repo: repoA, hostId: hostAId, tokens: hostTokens,
      processList: children, preview: phase === "candidate",
    });
    hosts.push(hostA);
    const hostB = await createDedicatedHost({
      binary, runtime, runDir, label: "b", repo: repoB, hostId: hostBId, tokens: hostTokens,
      processList: children, preview: phase === "candidate",
    });
    hosts.push(hostB);
    await startMcpReady(runtime, clientToken, hosts, Number(flags["host-wait-seconds"] ?? 60) * 1000);
    progress.stage = "hosts_ready";

    driver = await startDriver({
      phase, mode: "live", extractor, binary, flags, runtime, clientToken, hostA, hostB, runDir,
      extractorProfile, python: flags.python ?? process.env.PYTHON ?? "python3",
    });
    progress.stage = "driver_started";

    progress.taskATerminalWaitStarted = true;
    progress.stage = "task_a_terminal_wait";
    await waitForMarker(
      path.join(runDir, "task-a-complete.proof"), TASK_A_MARKER, taskPollSettings.waitMs, driver,
      { timeoutCode: "TASK_A_TERMINAL_TIMEOUT", driverExitCode: "DOGFOOD_DRIVER_EXITED_BEFORE_TASK_A_TERMINAL" },
    );
    progress.stage = "task_a_terminal";
    await copyTaskAHeadToHostB(repoA, repoB);
    if (phase === "candidate") {
      const taskAConstraint = scenario.task_a.constraint;
      progress.stage = "task_a_projection";
      progress.diagnosticHostLabel = hostA.label;
      await waitForProjectionWhileDriverRuns(runtime, clientToken, hostA, taskAConstraint, projectionWaitMs, driver, runDir, metadata);
      progress.taskAProjectionPredicatePassed = true;
      progress.stage = "task_a_projection_predicate_passed";

      progress.stage = "stopping_host_a";
      progress.offlineCheckAttempted = true;
      await stopDedicatedHostAndConfirmOffline({
        host: hostA,
        runtime,
        token: clientToken,
        proofPath: path.join(runDir, "host-a-offline.proof"),
        budgetMs: projectionWaitMs,
      });
      progress.stage = "host_a_offline";
    }
    await fs.writeFile(path.join(runDir, "task-b-ready.proof"), TASK_B_READY_MARKER, { mode: 0o600, flag: "wx" });
    progress.stage = "task_b_ready";
    progress.taskBTerminalWaitStarted = true;
    progress.stage = "task_b_terminal_wait";
    await waitForTaskBTerminalBeforeProjection({
      markerPath: path.join(runDir, "task-b-complete.proof"),
      timeoutMs: taskPollSettings.waitMs,
      driver,
      project: async () => {
        progress.taskBTerminalMarkerVerified = true;
        progress.stage = "task_b_terminal";
        if (phase !== "candidate") return;
        progress.stage = "task_b_projection";
        progress.diagnosticHostLabel = hostB.label;
        await waitForProjectionWhileDriverRuns(
          runtime, clientToken, hostB, scenario.task_b.constraint, projectionWaitMs, driver, runDir, metadata,
        );
        progress.taskBProjectionPredicatePassed = true;
        progress.stage = "task_b_projection_predicate_passed";
        progress.stage = "queue_replay";
        await replayQueueAndWriteManifest(runtime, runDir);
        progress.queueReplayPassed = true;
        progress.stage = "queue_replay_complete";
      },
    });

    const driverExitCode = await waitForDriver(driver, 30 * 60 * 1000);
    progress.stage = "driver_terminal";
    const artifact = JSON.parse(await fs.readFile(driver.artifactPath, "utf8"));
    if (artifact.scenario_fingerprint === undefined || artifact.phase !== phase
        || artifact.scenario_revision !== scenario.revision) {
      throw new HarnessError("DOGFOOD_ARTIFACT_IDENTITY_INVALID");
    }
    const expectedBaselineResult = phase === "baseline"
      && artifact.outcome === "not_implemented"
      && artifact.synthesis_mode === "not_run"
      && driverExitCode === 1;
    if (driverExitCode !== 0 && !expectedBaselineResult) {
      throw new HarnessError("DOGFOOD_SCENARIO_FAILED");
    }
    successful = phase === "baseline"
      ? expectedBaselineResult
      : extractor === "live" && artifact.outcome === "pass"
        && artifact.mode === "live" && artifact.synthesis_mode === "live";
    const miniflarePackage = JSON.parse(await fs.readFile(path.join(ROOT, "gateway/node_modules/miniflare/package.json"), "utf8"));
    const summary = {
      schema_version: 1,
      phase,
      mode: "live",
      synthesis_mode: artifact.synthesis_mode,
      extractor,
      extractor_config: extractorConfig,
      timing: metadata.timing,
      runtime: { miniflare_version: miniflarePackage.version, d1: "workerd", gateway_source: phase === "baseline" ? "archived_baseline" : "candidate" },
      live_synthesis: extractor === "live" && phase === "candidate"
        ? (artifact.outcome === "pass" ? "PASS" : artifact.outcome.toUpperCase())
        : "NOT RUN",
      outcome: artifact.outcome,
      selectors: artifact.selectors,
      scenario_fingerprint: artifact.scenario_fingerprint,
      artifact: path.relative(ROOT, driver.artifactPath),
      gates: artifact.gates,
      queue_replay_proof: phase === "candidate" ? "verified" : "not_run",
      offline_host_proof: phase === "candidate" ? "verified" : "not_run",
    };
    return { summary, successful };
  } catch (error) {
    const diagnosticHost = hosts.find((host) => host.label === progress.diagnosticHostLabel)
      ?? hosts.find((host) => host.label === "a")
      ?? hosts[0]
      ?? { label: "unknown", hostId: "", sessionId: "" };
    const diagnostics = await writeLiveFailureDiagnostics(runtime, clientToken, diagnosticHost, phase, runDir);
    await writeLiveFailureManifest(runDir, metadata, safeHarnessErrorCode(error), progress, diagnostics);
    throw error;
  } finally {
    if (driver && !childExited(driver.child)) await stopOwnedProcess(driver.child, 3_000);
    // Only these child processes were launched by this runner with isolated state.
    for (const host of hosts) await stopOwnedProcess(host.hostAgent);
    for (const host of hosts) {
      try {
        runCommand(binary, ["session", "stop", host.sessionId], {
          cwd: host.repo, env: host.env, timeout: 15_000,
          failureCode: "DEDICATED_SESSION_CLEANUP_FAILED",
        });
      } catch {
        // Session cleanup is retried below by stopping this runner-owned supervisor.
      }
    }
    for (const child of children.slice().reverse()) await stopOwnedProcess(child);
    await runtime.dispose();
  }
}

async function main() {
  const flags = parseArgs(process.argv.slice(2));
  if (flags.help) return printHelp();
  const phase = flags.phase;
  const mode = flags.mode ?? "fixture";
  const extractor = flags.extractor ?? (phase === "baseline" ? "disabled" : "live");
  if (!new Set(["baseline", "candidate"]).has(phase) || !new Set(["fixture", "live"]).has(mode)
      || !new Set(["disabled", "fixture", "live"]).has(extractor)) {
    throw new HarnessError("INVALID_PHASE_MODE_OR_EXTRACTOR");
  }
  if (mode === "fixture" && extractor === "live") throw new HarnessError("FIXTURE_MODE_CANNOT_USE_LIVE_EXTRACTOR");
  if (flags["extractor-reasoning-effort"] !== undefined && extractor !== "live") {
    throw new HarnessError("EXTRACTOR_REASONING_EFFORT_REQUIRES_LIVE_EXTRACTOR");
  }
  const runId = randomUUID();
  const runDir = flags.output
    ? checkSafeRelativeArtifactPath(flags.output)
    : path.join(ROOT, "dogfood/runs", `memory-continuity-${phase}-${runId}`);
  await mkdirPrivate(runDir);
  let result;
  if (mode === "fixture") {
    result = { summary: await runFixture(phase, runDir), successful: false };
  } else {
    result = await runLive(flags, phase, extractor, runDir);
  }
  const summaryPath = path.join(runDir, "harness-summary.json");
  await fs.writeFile(summaryPath, JSON.stringify(result.summary, null, 2) + "\n", { mode: 0o600, flag: "wx" });
  process.stdout.write(JSON.stringify({ ...result.summary, harness_summary: path.relative(ROOT, summaryPath) }) + "\n");
  if (mode === "live" && !result.successful) process.exitCode = 1;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main()
    .catch((error) => {
      const code = error instanceof HarnessError ? error.code : "MEMORY_DOGFOOD_FAILED";
      process.stderr.write(JSON.stringify({ outcome: "blocked", error_code: code }) + "\n");
      process.exitCode = 1;
    })
    .finally(() => stopEsbuild());
}
