export const PROTOCOL_VERSION = "2025-06-18";
export const SUPPORTED_PROTOCOL_VERSIONS = new Set([
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
    "Inspect a local MCP session",
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
    "Call a tool exposed by the official local 1Password Environments MCP server. Non-read-only child tools remain approval-gated by the host local-mcp session.",
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
    "Run a host command through op run using the service-account token held by the selected local-mcp start process. 1Password CLI output masking remains enabled; normal sessions require host approval.",
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
  return SUPPORTED_PROTOCOL_VERSIONS.has(requested) ? requested : PROTOCOL_VERSION;
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
