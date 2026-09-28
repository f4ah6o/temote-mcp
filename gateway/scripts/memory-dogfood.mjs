#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { randomBytes, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { startMemoryRuntime } from "../test/helpers/memory-runtime.mjs";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const REPOSITORY_KEY = "github:temote-tests/memory-continuity";
const UNRELATED_REPOSITORY = "github:temote-tests/memory-continuity-unrelated";
const OWNER_ID = "memory-dogfood-owner";
const OFFLINE_MARKER = "temote-memory-host-offline-v1\n";
const TASK_A_MARKER = "temote-memory-task-a-complete-v1\n";
const TASK_B_READY_MARKER = "temote-memory-task-b-ready-v1\n";
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
const MAX_ARTIFACT_BYTES = 1_048_576;

class HarnessError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

function parseArgs(argv) {
  const result = { flags: {}, positional: [] };
  const boolean = new Set(["help"]);
  const valued = new Set([
    "phase", "mode", "extractor", "binary", "baseline-binary", "candidate-binary", "baseline-gateway-root",
    "output", "backend", "model", "effort", "extractor-profile", "extractor-endpoint",
    "extractor-model", "auth-file", "python", "task-polls", "poll-interval",
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
    "",
    "Fixture mode runs the deterministic real-D1/Queue integration test and is never live qualification.",
    "Live mode starts dedicated Temote supervisors and host agents in isolated XDG_STATE_HOME roots.",
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

async function waitUntil(predicate, timeoutMs, intervalMs = 100) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    if (await predicate()) return true;
    await delay(intervalMs);
  }
  return false;
}

async function stopOwnedProcess(child, timeoutMs = 5_000) {
  if (!child || childExited(child)) return;
  child.kill("SIGTERM");
  const exited = await waitUntil(() => childExited(child), timeoutMs, 50);
  if (!exited && !childExited(child)) {
    child.kill("SIGKILL");
    await waitUntil(() => childExited(child), 2_000, 50);
  }
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

function hostIdsFrom(value, found = new Set(), depth = 0) {
  if (depth > 16) return found;
  if (Array.isArray(value)) {
    for (const child of value) hostIdsFrom(child, found, depth + 1);
  } else if (value && typeof value === "object") {
    if (typeof value.host_id === "string") found.add(value.host_id);
    for (const child of Object.values(value)) hostIdsFrom(child, found, depth + 1);
  }
  return found;
}

async function hostIsPresent(runtime, token, hostId) {
  const result = parseMcpText(
    await runtime.callMcp(token, "host_list", {}, Math.floor(Math.random() * 1_000_000_000)),
    "host_list",
  );
  return hostIdsFrom(result).has(hostId);
}

async function waitForHost(runtime, token, hostId, present, timeoutMs) {
  const ok = await waitUntil(async () => {
    try {
      return (await hostIsPresent(runtime, token, hostId)) === present;
    } catch {
      return false;
    }
  }, timeoutMs, 250);
  if (!ok) throw new HarnessError(present ? "DEDICATED_HOST_CONNECT_TIMEOUT" : "DEDICATED_HOST_OFFLINE_TIMEOUT");
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

async function waitForMarker(filePath, expected, timeoutMs, driver) {
  const result = await Promise.race([
    waitUntil(async () => {
      try {
        return (await fs.readFile(filePath, "utf8")) === expected;
      } catch {
        return false;
      }
    }, timeoutMs, 50).then((ok) => ok ? "marker" : "timeout"),
    driver.exit.then(() => "driver_exit"),
  ]);
  if (result !== "marker") throw new HarnessError(result === "timeout" ? "DOGFOOD_HANDSHAKE_TIMEOUT" : "DOGFOOD_DRIVER_EXITED_BEFORE_HANDSHAKE");
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

async function waitForTaskProjection(runtime, host, constraint, timeoutMs) {
  const end = Date.now() + timeoutMs;
  while (Date.now() <= end) {
    const source = await sourceStatus(runtime, host.hostId, host.sessionId);
    if (source?.complete && source.terminalCount > 0
        && await repositoryCheckpointReady(runtime)
        && await knowledgeStatus(runtime, constraint)) {
      return source;
    }
    await delay(250);
  }
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
  const result = await Promise.race([driver.exit, delay(timeoutMs).then(() => null)]);
  if (result === null) {
    driver.child.kill("SIGTERM");
    await waitUntil(() => childExited(driver.child), 3_000, 50);
    if (!childExited(driver.child)) driver.child.kill("SIGKILL");
    throw new HarnessError("DOGFOOD_DRIVER_TIMEOUT");
  }
  return result.code;
}

async function waitForProjectionWhileDriverRuns(runtime, host, constraint, timeoutMs, driver) {
  const result = await Promise.race([
    waitForTaskProjection(runtime, host, constraint, timeoutMs).then(() => "projected"),
    driver.exit.then(({ code }) => `driver_exit_${code}`),
  ]);
  if (result !== "projected") throw new HarnessError("DOGFOOD_DRIVER_EXITED_BEFORE_PROJECTION");
}

async function runFixture(phase, runDir) {
  const driverArtifact = path.join(runDir, `${phase}-fixture-status.json`);
  const python = process.env.PYTHON ?? "python3";
  const result = await execFile(python, [
    path.join(ROOT, "dogfood/memory_continuity.py"), phase, "--mode", "fixture", "--output", driverArtifact,
  ], { cwd: ROOT, failureCode: "DOGFOOD_FIXTURE_DRIVER_FAILED" });
  if (result.code !== 0) throw new HarnessError("DOGFOOD_FIXTURE_DRIVER_FAILED");
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
  const backend = flags.backend ?? DEFAULT_BACKEND;
  const model = flags.model ?? DEFAULT_TASK_MODEL;
  const effort = flags.effort ?? DEFAULT_TASK_EFFORT;
  const extractorEndpoint = flags["extractor-endpoint"] ?? process.env.TEMOTE_MCP_MEMORY_ENDPOINT ?? DEFAULT_ENDPOINT;
  const extractorModel = flags["extractor-model"] ?? process.env.TEMOTE_MCP_MEMORY_MODEL ?? DEFAULT_EXTRACTOR_MODEL;
  const extractorProfile = flags["extractor-profile"] ?? `opencode-go/${extractorModel}`;
  if (backend !== "codex" || !model || !effort) throw new HarnessError("CODING_BACKEND_SELECTOR_UNSUPPORTED");

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
      MEMORY_MAX_ATTEMPTS: "3",
      MEMORY_BATCH_SIZE: "16",
    },
  });

  const children = [];
  let driver;
  let hosts = [];
  let successful = false;
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

    driver = await startDriver({
      phase, mode: "live", extractor, binary, flags, runtime, clientToken, hostA, hostB, runDir,
      extractorProfile, python: flags.python ?? process.env.PYTHON ?? "python3",
    });

    const handshakeTimeout = Number(flags["pipeline-wait-seconds"] ?? 240) * 1000;
    await waitForMarker(path.join(runDir, "task-a-complete.proof"), TASK_A_MARKER, handshakeTimeout, driver);
    await copyTaskAHeadToHostB(repoA, repoB);
    if (phase === "candidate") {
      const pipelineWait = handshakeTimeout;
      const taskAConstraint = scenario.task_a.constraint;
      await waitForProjectionWhileDriverRuns(runtime, hostA, taskAConstraint, pipelineWait, driver);

      await stopOwnedProcess(hostA.hostAgent);
      await waitForHost(runtime, clientToken, hostA.hostId, false, 15_000);
      const proofPath = path.join(runDir, "host-a-offline.proof");
      await fs.writeFile(proofPath, OFFLINE_MARKER, { mode: 0o600, flag: "wx" });
    }
    await fs.writeFile(path.join(runDir, "task-b-ready.proof"), TASK_B_READY_MARKER, { mode: 0o600, flag: "wx" });
    if (phase === "candidate") {
      const pipelineWait = handshakeTimeout;
      await waitForProjectionWhileDriverRuns(runtime, hostB, scenario.task_b.constraint, pipelineWait, driver);
      await replayQueueAndWriteManifest(runtime, runDir);
    }

    const driverExitCode = await waitForDriver(driver, 30 * 60 * 1000);
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

main().catch((error) => {
  const code = error instanceof HarnessError ? error.code : "MEMORY_DOGFOOD_FAILED";
  process.stderr.write(JSON.stringify({ outcome: "blocked", error_code: code }) + "\n");
  process.exitCode = 1;
});
