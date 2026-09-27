const MAX_BODY_BYTES = 8 * 1024 * 1024;

export async function readJson(request, limit = MAX_BODY_BYTES) {
  try {
    return { ok: true, value: await readBoundedJson(request, limit, "request body") };
  } catch (error) {
    return { ok: false, error: String(error) };
  }
}

async function readBoundedJson(message, limit, label) {
  const bytes = await readBoundedBytes(message, limit, label);
  return JSON.parse(new TextDecoder().decode(bytes));
}

export async function readBoundedBytes(message, limit, label) {
  const rawLength = message.headers.get("content-length");
  if (rawLength !== null) {
    const length = Number(rawLength);
    if (!Number.isSafeInteger(length) || length < 0 || length > limit) {
      throw new Error(`${label} is too large`);
    }
  }
  if (!message.body) return new Uint8Array();

  const reader = message.body.getReader();
  const chunks = [];
  let total = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = value instanceof Uint8Array ? value : new Uint8Array(value);
      if (chunk.byteLength > limit - total) throw new Error(`${label} is too large`);
      chunks.push(chunk);
      total += chunk.byteLength;
    }
  } finally {
    reader.releaseLock();
  }

  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

export async function safeBoundedJson(response, limit, label) {
  try {
    return await readBoundedJson(response, limit, label);
  } catch {
    return null;
  }
}

export function unauthorizedClient() {
  return withCors(jsonResponse({ error: "access_unauthorized" }, 401, { "cache-control": "no-store" }));
}

export function unauthorizedHost() {
  return withCors(jsonResponse({ error: "host_unauthorized" }, 401, { "cache-control": "no-store" }));
}

export function mcpJson(value) {
  return withCors(jsonResponse(value, 200, mcpHeaders()));
}

function mcpHeaders() {
  return { "content-type": "application/json", "cache-control": "no-store" };
}

export function jsonResponse(value, status = 200, extraHeaders = {}) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", ...extraHeaders },
  });
}

export function withCors(response) {
  const headers = new Headers(response.headers);
  headers.set("access-control-allow-origin", "*");
  headers.set("access-control-allow-methods", "GET,POST,DELETE,OPTIONS");
  headers.set(
    "access-control-allow-headers",
    "accept,authorization,content-type,mcp-protocol-version,mcp-method,mcp-name,mcp-session-id,cf-access-client-id,cf-access-client-secret,x-temote-host-id",
  );
  headers.set("access-control-expose-headers", "mcp-session-id");
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}
