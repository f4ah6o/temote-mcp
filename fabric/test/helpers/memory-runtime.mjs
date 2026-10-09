import assert from "node:assert/strict";
import fs from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { build } from "esbuild";
import { convertV4MiniflareOptions, Miniflare } from "miniflare";

const GATEWAY_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const DEFAULT_PRODUCTION_ENTRY = path.join(GATEWAY_ROOT, "src/index.js");
const RUNTIME_ENTRY_PATH = fileURLToPath(new URL("./memory-runtime-entry.mjs", import.meta.url));
const DEFAULT_MIGRATIONS = [
  "migrations/0001_observation_knowledge.sql",
  "migrations/0002_observation_ingest.sql",
  "migrations/0003_memory_worker.sql",
  "migrations/0004_memory_worker_bootstrap.sql",
  "migrations/0005_mcp_events.sql",
  "migrations/0006_browser_host_enrollment.sql",
];

async function compileRuntimeEntry(productionEntryPath) {
  const productionRoot = path.dirname(path.dirname(productionEntryPath));
  const runtimeSource = await fs.readFile(RUNTIME_ENTRY_PATH, "utf8");
  const relativeSpecifier = (target) => {
    let value = path.relative(path.dirname(RUNTIME_ENTRY_PATH), target).split(path.sep).join("/");
    if (!value.startsWith(".")) value = "./" + value;
    return value;
  };
  const rewrittenSource = runtimeSource
    .replaceAll('"../../src/index.js"', JSON.stringify(relativeSpecifier(productionEntryPath)))
    .replaceAll('"../../src/routing-runtime.js"', JSON.stringify(relativeSpecifier(path.join(productionRoot, "src/routing-runtime.js"))));
  const result = await build({
    stdin: {
      contents: rewrittenSource,
      resolveDir: path.dirname(RUNTIME_ENTRY_PATH),
      sourcefile: path.relative(GATEWAY_ROOT, RUNTIME_ENTRY_PATH),
      loader: "js",
    },
    bundle: true,
    format: "esm",
    platform: "browser",
    target: "es2022",
    write: false,
    logLevel: "silent",
  });
  return result.outputFiles[0].text;
}

// The admin route is in memory-runtime-entry.mjs and is never deployed.

function splitSqlStatements(sql) {
  const statements = [];
  let pending = "";
  let inTrigger = false;
  for (const line of sql.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("--")) continue;
    if (/^PRAGMA\s+foreign_keys\s*=\s*ON\s*;?$/i.test(trimmed)) continue;
    if (/^CREATE\s+TRIGGER\b/i.test(trimmed)) inTrigger = true;
    pending += line + "\n";
    if (inTrigger && /^END\s*;\s*$/i.test(trimmed)) {
      statements.push(pending.trim());
      pending = "";
      inTrigger = false;
    } else if (!inTrigger && trimmed.endsWith(";")) {
      statements.push(pending.trim());
      pending = "";
    }
  }
  if (pending.trim()) statements.push(pending.trim());
  return statements;
}

/**
 * Start the production Worker in a local workerd/Miniflare runtime with a real
 * D1 binding and Queue binding. An admin Worker provides migration and bounded
 * SQL inspection helpers over that same D1 database.
 *
 * The returned `dispose` must be awaited. Optional caller bindings are applied
 * to the gateway Worker (for example provider endpoint/key), while test auth
 * and owner values remain isolated defaults unless explicitly overridden.
 */
export async function startMemoryRuntime({
  memoryExtractor = "fixture",
  memoryEnabled = true,
  memoryEndpoint = "https://memory-provider.invalid/v1/chat/completions",
  memoryModel = "fixture-test-model",
  memoryApiKey = "test-only-provider-key",
  memoryTestProviderMode = null,
  bindings = {},
  compatibilityDate = "2026-09-28",
  migrations = DEFAULT_MIGRATIONS,
  productionEntryPath = DEFAULT_PRODUCTION_ENTRY,
  applyMigrations = true,
  resourcePersistencePath,
  queueName = "temote-memory-test",
  maxQueueRetries = 2,
} = {}) {
  const ownedTemp = resourcePersistencePath === undefined;
  const persistencePath = resourcePersistencePath ?? await fs.mkdtemp(path.join(os.tmpdir(), "temote-memory-mf-"));
  const resolvedProductionEntry = path.resolve(productionEntryPath);
  const productionRoot = path.dirname(path.dirname(resolvedProductionEntry));
  const productionEntry = await fs.stat(resolvedProductionEntry);
  assert.equal(productionEntry.isFile(), true, "productionEntryPath must name a Worker entry file");
  const databaseName = `temote-memory-${path.basename(persistencePath).replace(/[^a-zA-Z0-9_-]/g, "-")}`;
  const runtimeScript = await compileRuntimeEntry(resolvedProductionEntry);
  const gatewayBindings = {
    CLIENT_TOKEN: "memory-test-client-token",
    HOST_TOKENS_JSON: JSON.stringify({ "memory-test-host": "memory-test-host-token" }),
    OBSERVATION_OWNER_ID: "memory-test-owner",
    MEMORY_ENABLED: memoryEnabled ? "true" : "false",
    MEMORY_EXTRACTOR: memoryExtractor,
    MEMORY_ENDPOINT: memoryEndpoint,
    MEMORY_MODEL: memoryModel,
    MEMORY_API_KEY: memoryApiKey,
    MEMORY_TIMEOUT_MS: "2000",
    MEMORY_INPUT_BUDGET_BYTES: "8192",
    MEMORY_OUTPUT_BUDGET_BYTES: "4096",
    MEMORY_MAX_ATTEMPTS: String(maxQueueRetries + 1),
    MEMORY_BATCH_SIZE: "16",
    ...(memoryTestProviderMode ? { MEMORY_TEST_PROVIDER_MODE: memoryTestProviderMode } : {}),
    ...bindings,
  };
  const options = convertV4MiniflareOptions({
    resourcePersistencePath: persistencePath,
    compatibilityDate,
    workers: [
      {
        name: "memory-test",
        script: runtimeScript,
        modules: true,
        compatibilityDate,
        d1Databases: { OBSERVATION_DB: databaseName },
        queueProducers: { MEMORY_QUEUE: { queueName } },
        queueConsumers: {
          [queueName]: { maxBatchSize: 10, maxBatchTimeout: 1, maxRetries: maxQueueRetries },
        },
        durableObjects: {
          GATEWAY_SESSIONS: { className: "GatewaySession", useSQLite: true },
          GATEWAY_REGISTRY: { className: "GatewayRegistry", useSQLite: true },
        },
        bindings: gatewayBindings,
      },
    ],
  });
  const miniflare = new Miniflare(options);
  let server;
  try {
    await miniflare.ready;
    const fetchRuntime = (url, init) => miniflare.dispatchFetch(url, init);
    server = http.createServer(async (request, response) => {
      const chunks = [];
      let total = 0;
      for await (const chunk of request) {
        total += chunk.byteLength;
        if (total > 1_048_576) {
          response.writeHead(413).end();
          return;
        }
        chunks.push(chunk);
      }
      try {
        const body = Buffer.concat(chunks);
        const forwarded = await fetchRuntime("http://" + request.headers.host + request.url, {
          method: request.method,
          headers: request.headers,
          ...(request.method === "GET" || request.method === "HEAD" ? {} : { body }),
        });
        response.writeHead(forwarded.status, Object.fromEntries(forwarded.headers));
        response.end(Buffer.from(await forwarded.arrayBuffer()));
      } catch {
        response.writeHead(502, { "content-type": "application/json" });
        response.end('{"error":"runtime_dispatch_failed"}');
      }
    });
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    const address = server.address();
    const baseUrl = "http://127.0.0.1:" + address.port;
    const adminFetch = (route, body) => fetchRuntime(`http://memory-test/__memory_test/${route}`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
    });
    const health = await adminFetch("health", {});
    assert.equal(health.status, 200, `memory admin health failed (${health.status})`);
    const healthBody = await health.json();
    assert.equal(healthBody.success, true, "real D1 binding was not available in workerd");
    const testQueueTable = await adminFetch("exec", {
      sql: "CREATE TABLE IF NOT EXISTS __memory_test_queue_dispatches (dispatch_id INTEGER PRIMARY KEY AUTOINCREMENT, received_at TEXT NOT NULL, completed_at TEXT, message_count INTEGER NOT NULL, state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'failed')))",
      params: [],
    });
    assert.equal(testQueueTable.status, 200, "test queue dispatch instrumentation could not initialize");

    const callAdmin = async (route, body) => {
      const response = await adminFetch(route, body);
      const value = await response.json();
      if (!response.ok || !value.success) {
        const error = new Error(`D1_${route.toUpperCase()}_FAILED`);
        error.status = response.status;
        error.detail = typeof value.error === "string" ? value.error : "unknown";
        throw error;
      }
      return value;
    };
    const executeSql = async (sql, params = []) => {
      if (Array.isArray(params) && params.length) return callAdmin("exec", { sql, params });
      let latest;
      for (const statement of splitSqlStatements(sql)) latest = await callAdmin("exec", { sql: statement, params: [] });
      return latest;
    };
    const querySql = async (sql, params = []) => {
      assert.match(sql.trim(), /^(SELECT|PRAGMA|WITH)\b/i, "querySql accepts read-only SQL only");
      const value = await callAdmin("query", { sql, params });
      return value.result?.results ?? [];
    };
    const batchSql = async (statements) => callAdmin("batch", { statements });
    const enqueue = async (body) => callAdmin("queue", { body });
    const queueDispatches = async () => querySql(
      "SELECT dispatch_id, received_at, completed_at, message_count, state FROM __memory_test_queue_dispatches ORDER BY dispatch_id",
    );
    const queueDispatchCount = async () => {
      const rows = await querySql("SELECT COUNT(*) AS count FROM __memory_test_queue_dispatches");
      return Number(rows[0]?.count ?? 0);
    };
    const callMcp = async (token, name, args = {}, id = 1) => {
      const response = await fetchRuntime("https://memory-test.local/mcp", {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } }),
      });
      const text = await response.text();
      if (text.length > 1_048_576) throw new Error("MCP_RESPONSE_TOO_LARGE");
      return { status: response.status, body: JSON.parse(text) };
    };
    const waitFor = async (predicate, { timeoutMs = 5000, intervalMs = 25 } = {}) => {
      const deadline = Date.now() + timeoutMs;
      let latest;
      while (Date.now() <= deadline) {
        latest = await predicate();
        if (latest) return latest;
        await new Promise((resolve) => setTimeout(resolve, intervalMs));
      }
      throw new Error("MEMORY_RUNTIME_WAIT_TIMEOUT");
    };
    const runScheduled = async () => callAdmin("scheduled", {});
    const apply = async () => {
      for (const migration of migrations) {
        const source = await fs.readFile(path.resolve(productionRoot, migration), "utf8");
        await executeSql(source);
      }
    };
    if (applyMigrations) await apply();
    return {
      miniflare,
      server,
      fetch: fetchRuntime,
      baseUrl,
      endpoint: baseUrl + "/mcp",
      adminUrl: baseUrl + "/__memory_test",
      queueName,
      productionEntryPath: resolvedProductionEntry,
      productionRoot,
      bindings: gatewayBindings,
      executeSql,
      querySql,
      batchSql,
      enqueue,
      queueDispatches,
      queueDispatchCount,
      callMcp,
      runScheduled,
      waitFor,
      applyMigrations: apply,
      async dispose() {
        await new Promise((resolve) => server.close(resolve));
        await miniflare.dispose();
        if (ownedTemp) await fs.rm(persistencePath, { recursive: true, force: true });
      },
    };
  } catch (error) {
    if (server?.listening) await new Promise((resolve) => server.close(resolve));
    await miniflare.dispose().catch(() => {});
    if (ownedTemp) await fs.rm(persistencePath, { recursive: true, force: true });
    throw error;
  }
}

export const MEMORY_TEST_OWNER = "memory-test-owner";
export const MEMORY_TEST_REPOSITORY = "github:temote-tests/memory-continuity";
export const MEMORY_TEST_CLIENT_TOKEN = "memory-test-client-token";
export const MEMORY_TEST_HOST_ID = "memory-test-host";
export const MEMORY_TEST_HOST_TOKEN = "memory-test-host-token";
