export const LEGACY_PROTOCOL_VERSION = "2025-06-18";
export const MODERN_PROTOCOL_VERSION = "2026-07-28";
export const SUPPORTED_LEGACY_PROTOCOL_VERSIONS = new Set([
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
]);

const SESSION_ID_PATTERN = /^(?!\.{1,2}$)[A-Za-z0-9._-]{1,64}$/;

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
const networkMutation = {
  readOnlyHint: false,
  destructiveHint: true,
  idempotentHint: false,
  openWorldHint: true,
};

const sessionProperty = {
  session_id: {
    type: "string",
    description: "Target host session ID. Mac and Windows/WSL2 use different IDs.",
  },
};

function schema(properties, required = []) {
  return {
    type: "object",
    properties,
    required,
    additionalProperties: false,
  };
}

function tool(name, title, description, annotations, inputSchema) {
  return { name, title, description, annotations, inputSchema };
}

export const PUBLIC_TOOLS = [
  tool(
    "session_list",
    "List gateway sessions",
    "List active Mac, Linux, and Windows/WSL2 sessions registered with the gateway.",
    readOnly,
    schema({}),
  ),
  tool(
    "session_info",
    "Inspect a Temote MCP session",
    "Show a session's ID, working directory, and allowed sandbox roots.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "read_file",
    "Read a local file",
    "Read a UTF-8 file from the selected host session.",
    readOnly,
    schema({ ...sessionProperty, path: { type: "string" } }, ["session_id", "path"]),
  ),
  tool(
    "get_image",
    "Read a local image",
    "Read a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image from the selected host.",
    readOnly,
    schema({ ...sessionProperty, path: { type: "string" } }, ["session_id", "path"]),
  ),
  tool(
    "list_directory",
    "List a local directory",
    "List entries in a directory under the selected host session's sandbox roots.",
    readOnly,
    schema({ ...sessionProperty, path: { type: "string" } }, ["session_id", "path"]),
  ),
  tool(
    "write_file",
    "Write a local file",
    "Write a UTF-8 file in the selected host session's sandbox.",
    idempotentMutation,
    schema(
      { ...sessionProperty, path: { type: "string" }, content: { type: "string" } },
      ["session_id", "path", "content"],
    ),
  ),
  tool(
    "git_add",
    "Stage files with Git",
    "Stage selected paths in the host repository.",
    idempotentMutation,
    schema(
      {
        ...sessionProperty,
        paths: { type: "array", items: { type: "string" }, minItems: 1, maxItems: 256 },
        cwd: { type: "string" },
      },
      ["session_id", "paths"],
    ),
  ),
  tool(
    "git_commit",
    "Create a local Git commit",
    "Commit the current Git index on the selected host.",
    mutation,
    schema(
      { ...sessionProperty, message: { type: "string", minLength: 1, maxLength: 16384 }, cwd: { type: "string" } },
      ["session_id", "message"],
    ),
  ),
  tool(
    "git_fetch",
    "Fetch Git remote updates",
    "Run approval-gated git fetch on the selected host.",
    { ...networkMutation, destructiveHint: false, idempotentHint: true },
    schema(
      { ...sessionProperty, cwd: { type: "string" }, remote: { type: "string", default: "origin" } },
      ["session_id"],
    ),
  ),
  tool(
    "git_pull",
    "Fast-forward Git branch",
    "Run approval-gated git pull --ff-only on the selected host.",
    networkMutation,
    schema({ ...sessionProperty, cwd: { type: "string" } }, ["session_id"]),
  ),
  tool(
    "git_push",
    "Push current Git branch",
    "Push the current branch after endpoint approval.",
    networkMutation,
    schema(
      {
        ...sessionProperty,
        cwd: { type: "string" },
        remote: { type: "string" },
        set_upstream: { type: "boolean", default: false },
      },
      ["session_id"],
    ),
  ),
  tool(
    "execute",
    "Run a sandboxed command",
    "Execute argv in the selected host's network-disabled sandbox.",
    mutation,
    schema(
      {
        ...sessionProperty,
        command: { type: "array", items: { type: "string" }, minItems: 1 },
        cwd: { type: "string" },
      },
      ["session_id", "command"],
    ),
  ),
  tool(
    "start_command",
    "Start a sandboxed command",
    "Start a background command in the selected host's network-disabled sandbox.",
    mutation,
    schema(
      {
        ...sessionProperty,
        command: { type: "array", items: { type: "string" }, minItems: 1 },
        cwd: { type: "string" },
      },
      ["session_id", "command"],
    ),
  ),
  tool(
    "poll_job",
    "Poll a sandbox job",
    "Poll a background command on the selected host.",
    { ...readOnly, idempotentHint: false },
    schema({ ...sessionProperty, job_id: { type: "string" } }, ["session_id", "job_id"]),
  ),
  tool(
    "stop_job",
    "Stop a sandbox job",
    "Stop a background command on the selected host.",
    mutation,
    schema({ ...sessionProperty, job_id: { type: "string" } }, ["session_id", "job_id"]),
  ),
  tool(
    "onepassword_mcp_discover",
    "Discover 1Password MCP",
    "List resources and tool schemas exposed by the official local 1Password Environments MCP server.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "onepassword_mcp_read_resource",
    "Read a 1Password MCP resource",
    "Read a documentation resource exposed by the official local 1Password Environments MCP server.",
    readOnly,
    schema({ ...sessionProperty, uri: { type: "string" } }, ["session_id", "uri"]),
  ),
  tool(
    "onepassword_mcp_call",
    "Call a 1Password MCP tool",
    "Call a tool exposed by the official local 1Password Environments MCP server. Non-read-only child tools remain approval-gated by the host temote-mcp session.",
    networkMutation,
    schema(
      {
        ...sessionProperty,
        tool_name: { type: "string" },
        arguments: { type: "object", additionalProperties: true },
      },
      ["session_id", "tool_name", "arguments"],
    ),
  ),
  tool(
    "onepassword_service_account_status",
    "Check 1Password service account",
    "Check whether the selected host session has a service-account token and whether 1Password CLI accepts it. The token is never returned.",
    { ...readOnly, openWorldHint: true },
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "onepassword_service_account_run",
    "Run with 1Password service-account secrets",
    "Run a host command through op run using the service-account token held by the selected temote-mcp start process. 1Password CLI output masking remains enabled; normal sessions require host approval.",
    networkMutation,
    schema(
      {
        ...sessionProperty,
        command: { type: "array", items: { type: "string" }, minItems: 1 },
        cwd: { type: "string" },
        env_files: { type: "array", items: { type: "string" } },
        environment: { type: "object", additionalProperties: { type: "string" } },
      },
      ["session_id", "command"],
    ),
  ),
  tool(
    "kintone_mcp_status",
    "Check kintone MCP",
    "Check whether the selected temote-mcp session has the official kintone MCP executable and required authentication configuration. Credential values are never returned.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "kintone_mcp_discover",
    "Discover kintone MCP",
    "List tool schemas exposed by the official kintone MCP server using credentials retained only by the selected temote-mcp start process.",
    { ...readOnly, openWorldHint: true },
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "kintone_mcp_call",
    "Call a kintone MCP tool",
    "Call a tool exposed by the official kintone MCP server. All child tool calls are host-approval-gated in normal temote-mcp sessions.",
    networkMutation,
    schema(
      {
        ...sessionProperty,
        tool_name: { type: "string" },
        arguments: { type: "object", additionalProperties: true },
      },
      ["session_id", "tool_name", "arguments"],
    ),
  ),
  tool(
    "kintone_cli_status",
    "Check cli-kintone",
    "Check whether the selected temote-mcp session has cli-kintone plus kintone authentication configuration, and list the supported API-backed command pairs. Credential values and tenant URL are never returned.",
    readOnly,
    schema(sessionProperty, ["session_id"]),
  ),
  tool(
    "kintone_cli_run",
    "Run cli-kintone",
    "Run an allow-listed API-backed cli-kintone command using credentials held only by the temote-mcp start process. Supports record export/import/delete, customize export/apply, and plugin upload. Secret-bearing connection/auth options are rejected; file arguments and optional stdout_path must stay within permitted roots in normal sessions. All runs require local approval unless the session is in yolo mode.",
    networkMutation,
    schema(
      {
        ...sessionProperty,
        arguments: {
          type: "array",
          items: { type: "string" },
          minItems: 2,
          description: "cli-kintone arguments excluding the executable, beginning with a supported command pair such as [\"record\",\"export\",...].",
        },
        cwd: { type: "string" },
        stdout_path: {
          type: "string",
          description: "Optional file path for record export stdout. Written atomically on success; rejected for other command pairs.",
        },
      },
      ["session_id", "arguments"],
    ),
  ),
];

export function validateSessionId(value) {
  return typeof value === "string" && SESSION_ID_PATTERN.test(value);
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

export function serverInfo(version = "2026.8.0") {
  return { name: "temote-mcp-gateway", title: "Temote MCP Gateway", version };
}

export function discoverResult(version = "2026.8.0") {
  return {
    resultType: "complete",
    supportedVersions: [MODERN_PROTOCOL_VERSION],
    capabilities: { tools: { listChanged: false } },
    instructions:
      "This is one MCP gateway for multiple endpoint sessions. Use session_list, then pass the selected session_id to every other tool.",
    ttlMs: 0,
    cacheScope: "private",
    _meta: { "io.modelcontextprotocol/serverInfo": serverInfo(version) },
  };
}

export function modernizeResult(method, result, version = "2026.8.0") {
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
