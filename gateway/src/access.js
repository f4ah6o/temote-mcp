import { validateHostId } from "./protocol.js";

const MAX_JWKS_BYTES = 1024 * 1024;
const MAX_ACCESS_JWT_BYTES = 64 * 1024;
const MAX_ACCESS_JWT_HEADER_BYTES = 8 * 1024;
const MAX_ACCESS_JWT_CLAIMS_BYTES = 32 * 1024;
const MAX_ACCESS_JWT_SIGNATURE_BYTES = 8 * 1024;
const MAX_ACCESS_KID_CHARS = 256;
const MAX_LOG_FIELD_CHARS = 256;
const jwksCache = new Map();

export function authorizeLegacyHost(request, env) {
  if (!env.HOST_TOKEN) return false;
  const authorization = request.headers.get("authorization") || "";
  return authorization === `Bearer ${env.HOST_TOKEN}`;
}

export function federatedHostToken(env, hostId) {
  if (!validateHostId(hostId) || typeof env?.HOST_TOKENS_JSON !== "string") return null;
  try {
    const tokens = JSON.parse(env.HOST_TOKENS_JSON);
    if (!tokens || typeof tokens !== "object" || Array.isArray(tokens)) return null;
    const token = tokens[hostId];
    return typeof token === "string" && token.length > 0 ? token : null;
  } catch {
    return null;
  }
}

export function authorizeFederatedHost(request, env, hostId) {
  const token = federatedHostToken(env, hostId);
  if (!token) return false;
  const authorization = request.headers.get("authorization") || "";
  return authorization === `Bearer ${token}`;
}

export async function authorizeClient(request, env) {
  const authorization = request.headers.get("authorization") || "";
  if (env.CLIENT_TOKEN && authorization === `Bearer ${env.CLIENT_TOKEN}`) {
    return { subject: "client-token", email: "-" };
  }
  const assertion = request.headers.get("cf-access-jwt-assertion");
  if (!assertion) return null;
  try {
    return await verifyAccessJwt(assertion, env);
  } catch (error) {
    console.error("Access JWT rejected", error);
    return null;
  }
}

async function verifyAccessJwt(token, env) {
  if (!env.ACCESS_TEAM_DOMAIN || !env.ACCESS_AUDIENCE) {
    throw new Error("Access JWT validation is not configured");
  }
  const parts = validateAccessJwtShape(token);
  const header = decodeJwtPart(parts[0]);
  const claims = decodeJwtPart(parts[1]);
  if (
    header.alg !== "RS256"
    || !accessKidAllowed(header.kid)
  ) throw new Error("unsupported JWT key");

  const issuer = normalizeAccessTeamDomain(env.ACCESS_TEAM_DOMAIN);
  const jwks = await getJwks(issuer);
  const jwk = jwks.find((candidate) => candidate.kid === header.kid);
  if (!jwk) throw new Error("JWT signing key not found");
  const key = await crypto.subtle.importKey(
    "jwk",
    jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const valid = await crypto.subtle.verify(
    "RSASSA-PKCS1-v1_5",
    key,
    base64UrlBytes(parts[2]),
    new TextEncoder().encode(`${parts[0]}.${parts[1]}`),
  );
  if (!valid) throw new Error("invalid JWT signature");

  const now = Math.floor(Date.now() / 1000);
  if (typeof claims.exp !== "number" || claims.exp <= now) throw new Error("expired JWT");
  if (typeof claims.nbf === "number" && claims.nbf > now + 60) throw new Error("JWT not active");
  if (claims.iss !== issuer) throw new Error("invalid JWT issuer");
  const audiences = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  if (!audiences.includes(env.ACCESS_AUDIENCE)) throw new Error("invalid JWT audience");

  const email = typeof claims.email === "string" ? claims.email : "";
  if (!accessEmailAllowed(env.ACCESS_ALLOWED_EMAILS, email)) {
    throw new Error("email is not allowed or ACCESS_ALLOWED_EMAILS is empty");
  }
  if (typeof claims.sub !== "string" || !claims.sub) throw new Error("JWT subject missing");
  return { subject: claims.sub, email: claims.email || "-" };
}

export function boundedLogField(value, fallback = "-") {
  if (typeof value !== "string") return fallback;
  if (value.length <= MAX_LOG_FIELD_CHARS) return value;
  return `${value.slice(0, MAX_LOG_FIELD_CHARS)}…`;
}

export function validateAccessJwtShape(token) {
  if (typeof token !== "string" || token.length === 0 || token.length > MAX_ACCESS_JWT_BYTES) {
    throw new Error("invalid JWT size");
  }
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("invalid JWT");
  const limits = [MAX_ACCESS_JWT_HEADER_BYTES, MAX_ACCESS_JWT_CLAIMS_BYTES, MAX_ACCESS_JWT_SIGNATURE_BYTES];
  for (let index = 0; index < parts.length; index += 1) {
    const part = parts[index];
    if (part.length === 0 || part.length > limits[index] || !/^[A-Za-z0-9_-]+$/.test(part)) {
      throw new Error("invalid JWT segment");
    }
  }
  return parts;
}

export function accessKidAllowed(value) {
  return typeof value === "string" && value.length > 0 && value.length <= MAX_ACCESS_KID_CHARS;
}

export function normalizeAccessTeamDomain(value) {
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error("ACCESS_TEAM_DOMAIN is invalid");
  }
  const raw = value.trim();
  const candidate = raw.includes("://") ? raw : `https://${raw}`;
  let parsed;
  try {
    parsed = new URL(candidate);
  } catch {
    throw new Error("ACCESS_TEAM_DOMAIN is invalid");
  }
  if (
    parsed.protocol !== "https:"
    || parsed.username !== ""
    || parsed.password !== ""
    || parsed.search !== ""
    || parsed.hash !== ""
    || parsed.pathname.replaceAll("/", "") !== ""
    || parsed.hostname === ""
  ) {
    throw new Error("ACCESS_TEAM_DOMAIN must be an HTTPS origin without a path");
  }
  return parsed.origin;
}

export function accessEmailAllowed(configured, email) {
  const allowedEmails = (configured || "")
    .split(",")
    .map((value) => value.trim().toLowerCase())
    .filter(Boolean);
  if (allowedEmails.length === 0) return false;
  const normalizedEmail = typeof email === "string" ? email.trim().toLowerCase() : "";
  return normalizedEmail.length > 0 && allowedEmails.includes(normalizedEmail);
}

async function getJwks(teamOrigin) {
  const cached = jwksCache.get(teamOrigin);
  if (cached && cached.expiresAt > Date.now()) return cached.keys;
  const response = await fetch(`${teamOrigin}/cdn-cgi/access/certs`, {
    cf: { cacheTtl: 300, cacheEverything: true },
  });
  if (!response.ok) throw new Error(`failed to fetch Access keys: ${response.status}`);
  const body = await readBoundedJson(response, MAX_JWKS_BYTES, "Access key response");
  if (!Array.isArray(body.keys)) throw new Error("invalid Access key response");
  jwksCache.set(teamOrigin, { keys: body.keys, expiresAt: Date.now() + 300_000 });
  return body.keys;
}

function decodeJwtPart(value) {
  return JSON.parse(new TextDecoder().decode(base64UrlBytes(value)));
}

function base64UrlBytes(value) {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const decoded = atob(padded);
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}

async function readBoundedJson(message, limit, label) {
  const bytes = await readBoundedBytes(message, limit, label);
  return JSON.parse(new TextDecoder().decode(bytes));
}

async function readBoundedBytes(message, limit, label) {
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
