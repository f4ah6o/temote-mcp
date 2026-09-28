const PRODUCER_SCHEMA_VERSION = 1;
const EXTRACTION_POLICY_VERSION = 3;
const PROMPT_VERSION = 2;

export const MEMORY_OUTPUT_SCHEMA = Object.freeze({
  version: PRODUCER_SCHEMA_VERSION,
  kinds: Object.freeze([
    "fact",
    "decision",
    "constraint",
    "observation",
    "failure_pattern",
    "unresolved",
    "summary",
  ]),
});

export const MEMORY_PROMPT_VERSION = PROMPT_VERSION;
export const MEMORY_POLICY_VERSION = EXTRACTION_POLICY_VERSION;

export async function memoryConfiguration(env) {
  return publicMemoryConfiguration(await loadMemoryConfiguration(env));
}

export async function loadMemoryConfiguration(env) {
  const enabled = truthy(env?.MEMORY_ENABLED);
  const requestedExtractor = normalized(env?.MEMORY_EXTRACTOR);
  const extractor = ["openai_compatible", "fixture"].includes(requestedExtractor)
    ? requestedExtractor
    : "disabled";
  const generation = positiveInteger(env?.MEMORY_PROJECTION_GENERATION, 1, 2_147_483_647);
  const timeoutMs = positiveInteger(env?.MEMORY_TIMEOUT_MS, 30_000, 120_000);
  const inputBudgetBytes = positiveInteger(env?.MEMORY_INPUT_BUDGET_BYTES, 16_384, 65_536);
  const outputBudgetBytes = positiveInteger(env?.MEMORY_OUTPUT_BUDGET_BYTES, 8_192, 32_768);
  const maxAttempts = positiveInteger(env?.MEMORY_MAX_ATTEMPTS, 3, 5);
  const batchSize = positiveInteger(env?.MEMORY_BATCH_SIZE, 32, 64);
  const model = boundedString(env?.MEMORY_MODEL, 256);
  const apiKey = boundedString(env?.MEMORY_API_KEY, 8192);
  const endpoint = normalizedEndpoint(env?.MEMORY_ENDPOINT);
  const credentialsConfigured = Boolean(model && apiKey && endpoint);
  const configured = enabled && (
    extractor === "fixture"
    || (extractor === "openai_compatible" && credentialsConfigured)
  );
  const errorCode = !enabled
    ? null
    : extractor === "disabled"
      ? "extractor_not_configured"
      : extractor === "openai_compatible" && !credentialsConfigured
        ? "provider_not_configured"
        : null;

  const producerVersion = configured
    ? "memory-v1-" + await sha256(JSON.stringify({
      adapter: extractor,
      endpoint: extractor === "openai_compatible" ? endpoint : null,
      model: extractor === "openai_compatible" ? model : "fixture-v1",
      prompt: PROMPT_VERSION,
      schema: PRODUCER_SCHEMA_VERSION,
      policy: EXTRACTION_POLICY_VERSION,
      inputBudgetBytes,
      outputBudgetBytes,
      batchSize,
      generation,
    }))
    : null;

  return Object.freeze({
    enabled,
    extractor,
    configured,
    errorCode,
    producerVersion,
    generation,
    timeoutMs,
    inputBudgetBytes,
    outputBudgetBytes,
    maxAttempts,
    batchSize,
    model: extractor === "openai_compatible" ? model : null,
    endpoint: extractor === "openai_compatible" ? endpoint : null,
    apiKey: extractor === "openai_compatible" ? apiKey : null,
  });
}

export function publicMemoryConfiguration(config) {
  return Object.freeze({
    enabled: config.enabled,
    extractor: config.extractor,
    configured: config.configured,
    producerVersion: config.producerVersion,
    timeoutMs: config.timeoutMs,
    inputBudgetBytes: config.inputBudgetBytes,
    outputBudgetBytes: config.outputBudgetBytes,
    maxAttempts: config.maxAttempts,
    batchSize: config.batchSize,
  });
}

function normalized(value) {
  return typeof value === "string" ? value.trim().toLowerCase() : "";
}

function truthy(value) {
  return typeof value === "string" && value.trim().toLowerCase() === "true";
}

function boundedString(value, maxBytes) {
  return typeof value === "string" && value.length > 0 && utf8Bytes(value) <= maxBytes
    ? value
    : null;
}

function positiveInteger(value, fallback, max) {
  if (typeof value !== "string" || !/^\d+$/.test(value)) return fallback;
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) return fallback;
  return Math.min(parsed, max);
}

function normalizedEndpoint(value) {
  if (typeof value !== "string" || utf8Bytes(value) > 2048) return null;
  try {
    const endpoint = new URL(value);
    if (endpoint.protocol !== "https:" || endpoint.username || endpoint.password
        || endpoint.search || endpoint.hash || !endpoint.pathname.endsWith("/chat/completions")) {
      return null;
    }
    return endpoint.origin + endpoint.pathname;
  } catch {
    return null;
  }
}

async function sha256(value) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function utf8Bytes(value) {
  return new TextEncoder().encode(value).byteLength;
}
