import routedToolMetadata from "../contract/routed-tool-metadata.json" with { type: "json" };

export const PUBLIC_TOOLS = routedToolMetadata;

export const LEGACY_PROTOCOL_VERSION = "2025-06-18";
export const MODERN_PROTOCOL_VERSION = "2026-07-28";
export const SUPPORTED_LEGACY_PROTOCOL_VERSIONS = new Set([
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
]);


const SESSION_ID_PATTERN = /^(?!\.{1,2}$)[A-Za-z0-9._-]{1,64}$/;
const HOST_ID_PATTERN = /^(?=.{1,128}$)(?=.*[A-Za-z0-9])[A-Za-z0-9._-]+$/;

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
