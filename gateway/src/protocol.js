export const LEGACY_PROTOCOL_VERSION = "2025-06-18";
export const MODERN_PROTOCOL_VERSION = "2026-07-28";
export const SUPPORTED_LEGACY_PROTOCOL_VERSIONS = new Set([
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
]);


const SESSION_ID_PATTERN = /^(?!\.{1,2}$)[A-Za-z0-9._-]{1,64}$/;
const HOST_ID_PATTERN = /^(?=.{1,128}$)(?=.*[A-Za-z0-9])[A-Za-z0-9._-]+$/;

const readOnly = {
  readOnlyHint: true,
  destructiveHint: false,
  idempotentHint: true,
  openWorldHint: false,
};
const mutation = {
  readOnlyHint: false,
  destructiveHint: true,
  idempotentHint: false,
  openWorldHint: false,
};
const idempotentMutation = {
  readOnlyHint: false,
  destructiveHint: true,
  idempotentHint: true,
  openWorldHint: false,
};
const networkReadOnly = { ...readOnly, openWorldHint: true };
const idempotentNetworkMutation = { ...idempotentMutation, openWorldHint: true };
const networkMutation = {
  readOnlyHint: false,
  destructiveHint: true,
  idempotentHint: false,
  openWorldHint: true,
};

const hostProperty = {
  host_id: {
    type: "string",
    description: "Federated host ID. Omit only for backwards-compatible unqualified session routing.",
  },
};
const sessionProperty = {
  ...hostProperty,
  session_id: {
    type: "string",
    description: "Target host-local session ID. The same session ID may exist on multiple hosts.",
  },
};

function schema(properties, required = []) {
  const value = {
    type: "object",
    properties,
    additionalProperties: false,
  };
  if (required.length > 0) value.required = required;
  return value;
}


function tool(name, title, description, annotations, inputSchema) {
  return { name, title, description, annotations, inputSchema };
}

export function stripContractProse(value) {
  if (Array.isArray(value)) return value.map(stripContractProse);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key]) => key !== "title" && key !== "description")
      .map(([key, child]) => [key, stripContractProse(child)]),
  );
}

function canonicalContractJson(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalContractJson).join(",")}]`;
  }
  if (value && typeof value === "object") {
    const entries = Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalContractJson(value[key])}`);
    return `{${entries.join(",")}}`;
  }
  return JSON.stringify(value);
}

let publicContractFingerprintPromise;

export function publicContractFingerprint() {
  publicContractFingerprintPromise ??= computePublicContractFingerprint();
  return publicContractFingerprintPromise;
}

async function computePublicContractFingerprint() {
  const contract = {
    latestLegacyProtocolVersion: LEGACY_PROTOCOL_VERSION,
    supportedLegacyProtocolVersions: [...SUPPORTED_LEGACY_PROTOCOL_VERSIONS],
    modernProtocolVersion: MODERN_PROTOCOL_VERSION,
    tools: stripContractProse(PUBLIC_TOOLS),
  };
  const bytes = new TextEncoder().encode(canonicalContractJson(contract));
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

export const PUBLIC_TOOLS = [
  tool(
    "host_list",
    "List federated Temote hosts",
    "List currently leased host-level gateway agents with non-secret platform, capability, generation, protocol, and named-root metadata.",
    readOnly,
    schema({}),
  ),
  tool(
    "host_info",
    "Inspect a federated Temote host",
    "Show one currently leased host and its non-secret federation metadata.",
    readOnly,
    schema(hostProperty, ["host_id"]),
  ),
  tool(
    "session_list",
    "List gateway sessions",
    "List sessions with explicit host attribution. Optionally filter by host_id.",
    readOnly,
    schema(hostProperty),
  ),
  tool(
    "session_start",
    "Start a managed session on a federated host",
    "Start a normal sandboxed session below a named root on the selected host. Remote yolo creation is unavailable.",
    mutation,
    schema(
      { ...hostProperty, path: { type: "string" }, session_id: { type: "string" } },
      ["host_id", "path"],
    ),
  ),
  tool(
    "session_stop",
    "Stop a managed session on a federated host",
    "Stop a session owned by the selected host's public lifecycle supervisor. Local CLI/yolo sessions cannot be stopped remotely.",
    mutation,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "session_restart",
    "Restart a managed session on a federated host",
    "Restart an active session owned by the selected host's public lifecycle supervisor. Local CLI/yolo sessions cannot be restarted remotely.",
    mutation,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "session_info",
    "Inspect a Temote MCP session",
    "Show a session's ID, working directory, and allowed sandbox roots.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "context_resolve",
    "Resolve the session context bundle",
    "Project the session-owned observation journal into a deterministic bounded context bundle: current task/execution/workspace state, recent task-scoped instruction references, unresolved items, and provenance refs. Read-only; knowledge/memory synthesis is not implemented yet, so knowledge fields are explicitly empty. Observation bodies stay in the owner-only journal and are never inlined.",
    readOnly,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", minLength: 1, maxLength: 256 },
        repository: { type: "string", minLength: 1, maxLength: 256 },
        query: { type: "string", minLength: 1, maxLength: 512 },
        limit: { type: "integer", minimum: 1, maximum: 64, default: 16 },
        at_least_revision: { type: "integer", minimum: 0 },
      },
      ["session_id"],
    ),
  ),
  tool(
    "context_status",
    "Inspect the session context plane",
    "Report the session observation journal's revision, size, compaction, and degradation counters plus the memory-worker status. Read-only and bounded; raw observation records are never returned.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "evidence_read",
    "Read scoped Temote evidence",
    "Read a bounded chunk from an opaque expiring evidence record owned by the selected session and scope.",
    readOnly,
    schema(
      {
        ...sessionProperty,
        evidence_id: { type: "string", format: "uuid" },
        offset_bytes: { type: "integer", minimum: 0, default: 0 },
        max_bytes: { type: "integer", minimum: 1, maximum: 65536, default: 16384 },
      },
      ["session_id", "evidence_id"],
    ),
  ),
  tool(
    "codex_status",
    "Check Codex app-server compatibility",
    "Check the locally installed Codex app-server and return bounded compatibility metadata.",
    networkReadOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "codex_task_start",
    "Start a scoped Codex task",
    "Start an idempotent scoped Codex app-server task with durable pre-side-effect acceptance.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        operation_id: { type: "string", format: "uuid" },
        task: { type: "string", minLength: 1, maxLength: 1048576 },
        model: { type: "string", minLength: 1, maxLength: 256 },
        effort: { type: "string", minLength: 1, maxLength: 256 },
      },
      ["session_id", "operation_id", "task", "model", "effort"],
    ),
  ),
  tool(
    "codex_task_get",
    "Read a scoped Codex task",
    "Read and reconcile a retained Codex task owned by the selected full session instance and scope.",
    networkReadOnly,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        after_revision: { type: "integer", minimum: 0 },
      },
      ["session_id", "task_id"],
    ),
  ),
  tool(
    "codex_task_control",
    "Control a scoped Codex task",
    "Idempotently steer or interrupt the retained active turn of a scoped Codex task.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        operation_id: { type: "string", format: "uuid" },
        action: { type: "string", enum: ["steer", "resume", "interrupt"] },
        input: { type: "string", minLength: 1, maxLength: 1048576 },
      },
      ["session_id", "task_id", "operation_id", "action"],
    ),
  ),
  tool(
    "opencode_status",
    "Check OpenCode serve compatibility",
    "Check the locally installed opencode serve backend and return bounded compatibility metadata.",
    networkReadOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "opencode_task_start",
    "Start a scoped OpenCode task",
    "Start an idempotent scoped opencode serve task with durable pre-side-effect acceptance.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        operation_id: { type: "string", format: "uuid" },
        task: { type: "string", minLength: 1, maxLength: 1048576 },
        model: { type: "string", minLength: 1, maxLength: 256 },
        agent: { type: "string", minLength: 1, maxLength: 256 },
        variant: { type: "string", minLength: 1, maxLength: 256 },
      },
      ["session_id", "operation_id", "task"],
    ),
  ),
  tool(
    "opencode_task_get",
    "Read a scoped OpenCode task",
    "Read and reconcile a retained OpenCode task owned by the selected full session instance and scope.",
    networkReadOnly,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        after_revision: { type: "integer", minimum: 0 },
      },
      ["session_id", "task_id"],
    ),
  ),
  tool(
    "opencode_task_control",
    "Control a scoped OpenCode task",
    "Idempotently steer, resume, or interrupt the retained opencode serve session of a scoped task.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        operation_id: { type: "string", format: "uuid" },
        action: { type: "string", enum: ["steer", "resume", "interrupt"] },
        input: { type: "string", minLength: 1, maxLength: 1048576 },
      },
      ["session_id", "task_id", "operation_id", "action"],
    ),
  ),
  tool(
    "devin_status",
    "Check Devin ACP compatibility",
    "Check the locally installed devin acp backend and return bounded capability metadata.",
    networkReadOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "devin_task_start",
    "Start a scoped Devin task",
    "Start an idempotent scoped devin acp task with durable pre-side-effect acceptance.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        operation_id: { type: "string", format: "uuid" },
        task: { type: "string", minLength: 1, maxLength: 1048576 },
        model: { type: "string", minLength: 1, maxLength: 256 },
        agent: { type: "string", minLength: 1, maxLength: 256 },
        cloud: { type: "boolean" },
      },
      ["session_id", "operation_id", "task"],
    ),
  ),
  tool(
    "devin_task_get",
    "Read a scoped Devin task",
    "Read and reconcile a retained Devin task owned by the selected full session instance and scope.",
    networkReadOnly,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        after_revision: { type: "integer", minimum: 0 },
      },
      ["session_id", "task_id"],
    ),
  ),
  tool(
    "devin_task_control",
    "Control a scoped Devin task",
    "Idempotently steer, resume, or interrupt the retained devin acp session of a scoped task.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        operation_id: { type: "string", format: "uuid" },
        action: { type: "string", enum: ["steer", "resume", "interrupt"] },
        input: { type: "string", minLength: 1, maxLength: 1048576 },
      },
      ["session_id", "task_id", "operation_id", "action"],
    ),
  ),
  tool(
    "devin_cloud_status",
    "Check Devin Cloud API access",
    "Verify the configured Devin Cloud API v3 credential and return the authenticated principal, organization, and API base URL without the credential value.",
    networkReadOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "devin_cloud_task_start",
    "Start a Devin Cloud session task",
    "Start an idempotent scoped Devin Cloud hosted session (API v3) with durable pre-side-effect acceptance and a structured report schema.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        operation_id: { type: "string", format: "uuid" },
        task: { type: "string", minLength: 1, maxLength: 1048576 },
        title: { type: "string", minLength: 1, maxLength: 256 },
        devin_mode: { type: "string", description: "Devin session mode. The swe-2-medium/high/max values select SWE-2 reasoning effort only.", enum: ["normal", "fast", "lite", "ultra", "fusion", "swe-2-medium", "swe-2-high", "swe-2-max"] },
        swe_tier: { type: "string", description: "Optional SWE-2 service tier. promo keeps the requested SWE-2 mode and relies on upstream account promo eligibility; priority requires Temote to discover one exact account-visible priority/fast SWE-2 UID before remote session creation.", enum: ["promo", "priority"] },
        repos: { type: "array", items: { type: "string", minLength: 1, maxLength: 256 }, maxItems: 16 },
        max_acu_limit: { type: "integer", minimum: 1, maximum: 100000 },
      },
      ["session_id", "operation_id", "task"],
    ),
  ),
  tool(
    "devin_cloud_task_get",
    "Read a Devin Cloud session task",
    "Read and reconcile a retained Devin Cloud task owned by the selected full session instance and scope against the hosted session status.",
    networkReadOnly,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        after_revision: { type: "integer", minimum: 0 },
      },
      ["session_id", "task_id"],
    ),
  ),
  tool(
    "devin_cloud_task_control",
    "Control a Devin Cloud session task",
    "Idempotently steer, resume, or interrupt the hosted Devin Cloud session of a retained scoped task.",
    idempotentNetworkMutation,
    schema(
      {
        ...sessionProperty,
        task_id: { type: "string", format: "uuid" },
        operation_id: { type: "string", format: "uuid" },
        action: { type: "string", enum: ["steer", "resume", "interrupt"] },
        input: { type: "string", minLength: 1, maxLength: 1048576 },
      },
      ["session_id", "task_id", "operation_id", "action"],
    ),
  ),
  tool(
    "poll_job",
    "Poll a sandbox job",
    "Poll a background command on the selected host.",
    { ...readOnly, idempotentHint: false },
    schema(
      {
        ...sessionProperty,
        job_id: { type: "string" },
        output_limit_bytes: { type: "integer", minimum: 256, maximum: 1048576 },
        status_only: { type: "boolean" },
      },
      ["session_id", "job_id"],
    ),
  ),
  tool(
    "job_list",
    "List current-session sandbox jobs",
    "Return a bounded redacted snapshot of in-memory jobs owned by the selected session.",
    readOnly,
    schema(
      { ...sessionProperty, limit: { type: "integer", minimum: 1, maximum: 128, default: 50 } },
      ["session_id"],
    ),
  ),
  tool(
    "stop_job",
    "Stop a sandbox job",
    "Stop a background command on the selected host.",
    mutation,
    schema({ ...sessionProperty, job_id: { type: "string" } }, ["session_id", "job_id"]),
  ),
];

export function validateSessionId(value) {
  return typeof value === "string" && SESSION_ID_PATTERN.test(value);
}

export function validateHostId(value) {
  return typeof value === "string" && HOST_ID_PATTERN.test(value);
}

export function hostIdFromRpc(request) {
  const value = request?.params?.arguments?.host_id;
  return validateHostId(value) ? value : null;
}

export function sessionIdFromRpc(request) {
  const value = request?.params?.arguments?.session_id;
  return validateSessionId(value) ? value : null;
}

export function negotiateProtocolVersion(request) {
  const requested = request?.params?.protocolVersion;
  return SUPPORTED_LEGACY_PROTOCOL_VERSIONS.has(requested) ? requested : LEGACY_PROTOCOL_VERSION;
}

export function modernProtocolVersion(request) {
  const value = request?.params?._meta?.["io.modelcontextprotocol/protocolVersion"];
  return typeof value === "string" ? value : null;
}

export function isModernRequest(request) {
  if (request?.method === "server/discover") return true;
  const meta = request?.params?._meta;
  if (!meta || typeof meta !== "object" || Array.isArray(meta)) return false;
  return [
    "io.modelcontextprotocol/protocolVersion",
    "io.modelcontextprotocol/clientCapabilities",
    "io.modelcontextprotocol/clientInfo",
    "io.modelcontextprotocol/logLevel",
  ].some((key) => Object.hasOwn(meta, key));
}

export function validateModernRequestBody(request) {
  const meta = request?.params?._meta;
  if (!meta || typeof meta !== "object" || Array.isArray(meta)) {
    return { code: -32602, message: "modern MCP requests require params._meta" };
  }
  const version = meta["io.modelcontextprotocol/protocolVersion"];
  if (typeof version !== "string") {
    return { code: -32602, message: "missing io.modelcontextprotocol/protocolVersion" };
  }
  const capabilities = meta["io.modelcontextprotocol/clientCapabilities"];
  if (!capabilities || typeof capabilities !== "object" || Array.isArray(capabilities)) {
    return { code: -32602, message: "missing or invalid io.modelcontextprotocol/clientCapabilities" };
  }
  return null;
}

export function gatewayVersion(env) {
  const version = env?.GATEWAY_DEPLOYMENT?.id;
  if (typeof version !== "string" || version.length === 0 || version.length > 128) {
    throw new Error("GATEWAY_DEPLOYMENT version metadata binding is missing or invalid");
  }
  return version;
}

export function serverInfo(version) {
  return { name: "temote-mcp-gateway", title: "Temote MCP Gateway", version };
}

export function discoverResult(version) {
  return {
    resultType: "complete",
    supportedVersions: [MODERN_PROTOCOL_VERSION],
    capabilities: { tools: { listChanged: false } },
    instructions:
      "This is one MCP gateway for multiple federated Temote hosts. Use host_list and session_list, then pass host_id with session_id. An unqualified session_id is accepted only when ownership is unambiguous.",
    ttlMs: 0,
    cacheScope: "private",
    _meta: { "io.modelcontextprotocol/serverInfo": serverInfo(version) },
  };
}

export function modernizeResult(method, result, version) {
  if (!result || typeof result !== "object" || Array.isArray(result)) return result;
  if (method === "server/discover") return result;
  const modern = {
    ...result,
    resultType: "complete",
    _meta: {
      ...(result._meta && typeof result._meta === "object" ? result._meta : {}),
      "io.modelcontextprotocol/serverInfo": serverInfo(version),
    },
  };
  if (method === "tools/list") {
    modern.ttlMs = 0;
    modern.cacheScope = "private";
  }
  return modern;
}

export function textResult(text) {
  return { content: [{ type: "text", text }] };
}

export function rpcResult(id, result) {
  return { jsonrpc: "2.0", id: id ?? null, result };
}

export function rpcError(id, code, message, data = undefined) {
  const error = { code, message };
  if (data !== undefined) error.data = data;
  return { jsonrpc: "2.0", id: id ?? null, error };
}
