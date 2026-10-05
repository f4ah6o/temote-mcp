const DAY = 86_400_000;
export const MAX_TTL_MS = 7 * DAY;
export const ROTATION_MS = 300_000;

export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  return JSON.stringify(value);
}

export async function subscriptionId(principal, url, name, args) {
  const bytes = new TextEncoder().encode(canonicalJson([principal, url, name, args]));
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return `sub_${[...hash].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

export function validSecret(value) {
  if (typeof value !== "string" || !/^whsec_[A-Za-z0-9+/]+={0,2}$/.test(value)) return false;
  try {
    const encoded = value.slice(6);
    const raw = atob(encoded);
    return raw.length >= 24 && raw.length <= 64 && btoa(raw) === encoded;
  } catch { return false; }
}

export function callbackUrl(value) {
  if (typeof value !== "string" || value.length > 2048) return null;
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || !url.hostname || url.username || url.password || url.hash || url.port && url.port !== "443") return null;
    if (url.hostname === "localhost" || url.hostname.endsWith(".local") || url.hostname.endsWith(".localhost") || url.hostname.endsWith(".internal")) return null;
    return url.href;
  } catch { return null; }
}

export function grantedTtl(value, present) {
  if (!present || value === null) return DAY;
  if (!Number.isSafeInteger(value) || value <= 0) return null;
  return Math.min(value, MAX_TTL_MS);
}

export function constantTimeEqual(a, b) {
  const left = new TextEncoder().encode(typeof a === "string" ? a : "");
  const right = new TextEncoder().encode(typeof b === "string" ? b : "");
  let difference = left.length ^ right.length;
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    difference |= (left[index] ?? 0) ^ (right[index] ?? 0);
  }
  return difference === 0;
}
