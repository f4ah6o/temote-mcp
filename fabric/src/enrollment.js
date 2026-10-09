import { authorizeAccessIdentity, authorizeFederatedHost, validateLegacyHostInventory } from "./access.js";
import { jsonResponse, readJson, unauthorizedClient, unauthorizedHost, withCors } from "./http.js";
import { validateHostId } from "./protocol.js";
import { hostStub } from "./routing.js";

const MAX_ENROLLMENT_BODY_BYTES = 8192;
const PENDING_TTL_MS = 5 * 60 * 1000;
const GRANT_TTL_MS = 90 * 24 * 60 * 60 * 1000;
const ROOT_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const DIGEST = /^[0-9a-f]{64}$/i;

export async function handleEnrollmentRequest(request, env, pathname) {
  const identity = await authorizeAccessIdentity(request, env);
  if (!identity) return unauthorizedClient();
  const ownerKey = await ownerKeyFor(identity);
  if (pathname === "/v1/enrollment-identity") {
    if (request.method !== "GET") return withCors(new Response(null, { status: 405 }));
    return reply({ owner_key: ownerKey, email: boundedEmail(identity.email) });
  }
  if (!isD1(env?.OBSERVATION_DB)) return reply({ error: "enrollment_database_unavailable" }, 503);
  const match = /^\/v1\/enrollments\/([^/]+)(?:\/(activate|pending|roots))?$/.exec(pathname);
  if (!match) return reply({ error: "not_found" }, 404);
  const hostId = match[1];
  const action = match[2] ?? "reserve";
  if (!validateHostId(hostId)) return reply({ error: "invalid_host_id" }, 400);
  if (action === "reserve" && request.method === "GET") return ownEnrollmentStatus(env, ownerKey, hostId);
  if (action === "reserve" && request.method === "POST") return reserve(request, env, ownerKey, hostId);
  if (action === "activate" && request.method === "POST") return activate(request, env, ownerKey, hostId);
  if (action === "pending" && request.method === "DELETE") return cancelPending(request, env, ownerKey, hostId);
  if (action === "roots" && request.method === "PUT") return updateRoots(request, env, ownerKey, hostId);
  if (action === "reserve" && request.method === "DELETE") return revoke(request, env, ownerKey, hostId);
  return withCors(new Response(null, { status: 405 }));
}

export async function authorizeBrowserHost(request, env, hostId) {
  if (!validateHostId(hostId) || !isD1(env?.OBSERVATION_DB)) return null;
  const inventory = validateLegacyHostInventory(env);
  if (!inventory.ok || inventory.ids.has(hostId)) return null;
  const identity = await authorizeAccessIdentity(request, env);
  const suppliedGrant = request.headers.get("x-temote-fabric-host-grant");
  if (!identity || !suppliedGrant || suppliedGrant.length < 32 || suppliedGrant.length > 256) return null;
  const ownerKey = await ownerKeyFor(identity);
  const row = await env.OBSERVATION_DB.prepare(
    "SELECT host_id, owner_key, grant_id, generation, status, grant_digest, roots_json, expires_at FROM fabric_host_grants WHERE host_id = ?",
  ).bind(hostId).first();
  if (!row || row.status !== "active" || row.owner_key !== ownerKey
      || !Number.isSafeInteger(Number(row.expires_at)) || Number(row.expires_at) <= Date.now()) return null;
  const digest = await sha256Hex(suppliedGrant);
  if (!constantTimeEqual(digest, row.grant_digest)) return null;
  let roots;
  try {
    roots = JSON.parse(row.roots_json);
  } catch {
    return null;
  }
  if (!validRootNames(roots) || !Number.isSafeInteger(Number(row.generation))) return null;
  return {
    mode: "browser",
    owner_key: ownerKey,
    grant_id: row.grant_id,
    grant_generation: Number(row.generation),
    approved_roots: roots,
  };
}

export async function authorizeLegacyFederatedHost(request, env, hostId) {
  if (!authorizeFederatedHost(request, env, hostId)) return false;
  const db = env?.OBSERVATION_DB;
  if (!db || typeof db.prepare !== "function") return true;
  try {
    const row = await db.prepare("SELECT host_id FROM fabric_host_grants WHERE host_id = ? LIMIT 1").bind(hostId).first();
    return row === null || row === undefined;
  } catch {
    // Once D1 is configured, inability to check the global host-id namespace
    // must not turn a static credential into a fallback for a browser grant.
    return false;
  }
}

export async function activeBrowserGrant(env, hostId, ownerKey = null) {
  if (!isD1(env?.OBSERVATION_DB) || !validateHostId(hostId)) return null;
  const row = await env.OBSERVATION_DB.prepare(
    "SELECT host_id, owner_key, grant_id, generation, status, roots_json, expires_at FROM fabric_host_grants WHERE host_id = ?",
  ).bind(hostId).first();
  if (!row || row.status !== "active" || (ownerKey && row.owner_key !== ownerKey)
      || !Number.isSafeInteger(Number(row.expires_at)) || Number(row.expires_at) <= Date.now()) return null;
  let roots;
  try { roots = JSON.parse(row.roots_json); } catch { return null; }
  if (!validRootNames(roots)) return null;
  return {
    host_id: row.host_id,
    owner_key: row.owner_key,
    grant_id: row.grant_id,
    grant_generation: Number(row.generation),
    approved_roots: roots,
  };
}

export function sameBrowserGrantSnapshot(grant, snapshot) {
  return Boolean(grant && snapshot && snapshot.mode === "browser"
    && grant.owner_key === snapshot.owner_key
    && grant.grant_id === snapshot.grant_id
    && grant.grant_generation === snapshot.grant_generation
    && JSON.stringify(grant.approved_roots) === JSON.stringify(snapshot.approved_roots));
}

export async function browserHostIdsForOwner(env, ownerKey) {
  if (!isD1(env?.OBSERVATION_DB) || typeof ownerKey !== "string") return null;
  const result = await env.OBSERVATION_DB.prepare(
    "SELECT host_id FROM fabric_host_grants WHERE owner_key = ? AND status = 'active' AND expires_at > ? ORDER BY host_id LIMIT 256",
  ).bind(ownerKey, Date.now()).all();
  const rows = result?.results;
  if (!Array.isArray(rows) || rows.length > 256 || rows.some((row) => !validateHostId(row.host_id))) return null;
  return new Set(rows.map((row) => row.host_id));
}

export async function allBrowserHostIds(env) {
  // D1 is optional for the pre-enrollment static-host deployment. In that
  // explicit mode there is no browser namespace to enumerate; a configured
  // but unreadable or unmigrated database still fails closed.
  if (!isD1(env?.OBSERVATION_DB)) return new Set();
  try {
    const result = await env.OBSERVATION_DB.prepare(
      "SELECT host_id FROM fabric_host_grants ORDER BY host_id LIMIT 1025",
    ).all();
    const rows = result?.results;
    if (!Array.isArray(rows) || rows.length > 1024 || rows.some((row) => !validateHostId(row.host_id))) return null;
    return new Set(rows.map((row) => row.host_id));
  } catch {
    return null;
  }
}

export async function sweepHostGrantOutbox(env) {
  const db = env?.OBSERVATION_DB;
  if (!isD1(db)) return;
  let rows;
  try {
    rows = (await db.prepare(
      "SELECT operation_id, host_id, generation FROM fabric_host_grant_outbox WHERE state = 'pending' ORDER BY updated_at, operation_id LIMIT 64",
    ).all())?.results;
  } catch {
    return;
  }
  if (!Array.isArray(rows)) return;
  for (const row of rows) {
    if (!UUID.test(row?.operation_id) || !validateHostId(row?.host_id)
        || !Number.isSafeInteger(Number(row?.generation))) continue;
    try {
      const response = await hostStub(env, row.host_id).fetch("https://host.internal/fence", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ host_id: row.host_id, generation: Number(row.generation) }),
      });
      const now = Date.now();
      if (response.ok) {
        await db.prepare("UPDATE fabric_host_grant_outbox SET state = 'complete', attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND host_id = ? AND generation = ? AND state = 'pending'")
          .bind(now, row.operation_id, row.host_id, Number(row.generation)).run();
      } else {
        await db.prepare("UPDATE fabric_host_grant_outbox SET attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND state = 'pending'")
          .bind(now, row.operation_id).run();
      }
    } catch {
      try {
        await db.prepare("UPDATE fabric_host_grant_outbox SET attempts = attempts + 1, updated_at = ? WHERE operation_id = ? AND state = 'pending'")
          .bind(Date.now(), row.operation_id).run();
      } catch { /* D1 authorization remains the commit point. */ }
    }
  }
}

async function reserve(request, env, ownerKey, hostId) {
  const inventory = validateLegacyHostInventory(env);
  if (!inventory.ok) return reply({ error: "legacy_host_inventory_invalid" }, 503);
  if (inventory.ids.has(hostId)) return reply({ error: "host_id_conflict" }, 409);
  const parsed = await readJson(request, MAX_ENROLLMENT_BODY_BYTES);
  if (!parsed.ok) return reply({ error: "invalid_json" }, 400);
  const body = parsed.value;
  if (!isRecordWithOnly(body, ["attempt_id", "grant_id", "grant_digest", "root_names", "expected_generation", "operation_id"])
      || !UUID.test(body.attempt_id) || !UUID.test(body.grant_id) || !UUID.test(body.operation_id) || !validDigest(body.grant_digest)
      || !Number.isSafeInteger(body.expected_generation) || body.expected_generation < 0
      || !validRootNames(body.root_names) || body.root_names.length === 0) {
    return reply({ error: "invalid_enrollment" }, 400);
  }
  const db = env.OBSERVATION_DB;
  const now = Date.now();
  const rootsJson = JSON.stringify([...body.root_names].sort());
  const existing = await readGrant(db, hostId);
  if (!existing && body.expected_generation !== 0) return reply({ error: "grant_generation_conflict" }, 409);
  if (existing) {
    if (existing.owner_key !== ownerKey) return reply({ error: "host_id_conflict" }, 409);
    if (existing.status === "pending" && existing.attempt_id === body.attempt_id
        && existing.operation_id === body.operation_id && existing.grant_digest === body.grant_digest.toLowerCase()
        && existing.grant_id === body.grant_id && existing.roots_json === rootsJson
        && existing.reservation_operation_id === body.operation_id
        && Number(existing.generation) === body.expected_generation + 1) {
      if (Number(existing.pending_expires_at) > now) return grantReply(existing, ownerKey);
      const reacquired = await db.prepare(
        "UPDATE fabric_host_grants SET pending_expires_at = ?, updated_at = ? WHERE host_id = ? AND owner_key = ? AND status = 'pending' AND attempt_id = ? AND reservation_operation_id = ? AND generation = ? AND grant_digest = ? AND pending_expires_at <= ?",
      ).bind(now + PENDING_TTL_MS, now, hostId, ownerKey, body.attempt_id, body.operation_id,
        body.expected_generation + 1, body.grant_digest.toLowerCase(), now).run();
      const current = await readGrant(db, hostId);
      if (matchesPendingReservation(current, ownerKey, body, rootsJson, now)) return grantReply(current, ownerKey);
      if (!changes(reacquired)) return reply({ error: "grant_generation_conflict" }, 409);
      return reply({ error: "enrollment_unavailable" }, 503);
    }
    // Attempts are one-shot, including expired and cancelled attempts. A
    // delayed reserve cannot resurrect the same transaction after a terminal
    // state; a deliberate retry must use a fresh attempt id and current gen.
    if (existing.attempt_id === body.attempt_id) return reply({ error: "reservation_attempt_terminal_or_conflict" }, 409);
    const reusable = existing.status === "revoked"
      || (existing.status === "active" && Number(existing.expires_at) <= now)
      || (existing.status === "pending" && Number(existing.pending_expires_at) <= now);
    if (!reusable || Number(existing.generation) !== body.expected_generation) {
      return reply({ error: "grant_generation_conflict" }, 409);
    }
    const changed = await db.prepare(
      "UPDATE fabric_host_grants SET grant_id = ?, generation = generation + 1, status = 'pending', grant_digest = ?, roots_json = ?, attempt_id = ?, reservation_operation_id = ?, cancel_operation_id = NULL, activation_operation_id = NULL, operation_id = ?, operation_generation = generation + 1, pending_expires_at = ?, expires_at = NULL, activated_at = NULL, revoked_at = NULL, updated_at = ? WHERE host_id = ? AND owner_key = ? AND generation = ? AND (status = 'revoked' OR (status = 'active' AND expires_at <= ?) OR (status = 'pending' AND pending_expires_at <= ?))",
    ).bind(body.grant_id, body.grant_digest.toLowerCase(), rootsJson, body.attempt_id, body.operation_id, body.operation_id,
      now + PENDING_TTL_MS, now, hostId, ownerKey, body.expected_generation, now, now).run();
    if (!changes(changed)) {
      const current = await readGrant(db, hostId);
      if (matchesPendingReservation(current, ownerKey, body, rootsJson, now)) {
        return grantReply(current, ownerKey);
      }
      return reply({ error: "grant_generation_conflict" }, 409);
    }
    const updated = await readGrant(db, hostId);
    return updated ? grantReply(updated, ownerKey) : reply({ error: "enrollment_unavailable" }, 503);
  }

  const result = await db.prepare(
    "INSERT OR IGNORE INTO fabric_host_grants (host_id, owner_key, grant_id, generation, status, grant_digest, roots_json, attempt_id, operation_id, operation_generation, reservation_operation_id, pending_expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
  ).bind(hostId, ownerKey, body.grant_id, body.expected_generation + 1, body.grant_digest.toLowerCase(),
    rootsJson, body.attempt_id, body.operation_id, body.expected_generation + 1, body.operation_id,
    now + PENDING_TTL_MS, now, now).run();
  if (!changes(result)) {
    const raced = await readGrant(db, hostId);
    if (matchesPendingReservation(raced, ownerKey, body, rootsJson, now)) {
      return grantReply(raced, ownerKey);
    }
    return reply({ error: "host_id_conflict" }, 409);
  }
  const reserved = await readGrant(db, hostId);
  return reserved ? grantReply(reserved, ownerKey) : reply({ error: "enrollment_unavailable" }, 503);
}

async function ownEnrollmentStatus(env, ownerKey, hostId) {
  const row = await readGrant(env.OBSERVATION_DB, hostId);
  if (!row || row.owner_key !== ownerKey) return reply({ error: "not_found" }, 404);
  const roots = (() => {
    try { return JSON.parse(row.roots_json); } catch { return null; }
  })();
  if (!validRootNames(roots) || !Number.isSafeInteger(Number(row.generation))) {
    return reply({ error: "enrollment_unavailable" }, 503);
  }
  return reply({
    host_id: hostId,
    owner_key: ownerKey,
    status: row.status,
    generation: Number(row.generation),
    grant_id: row.grant_id,
    root_names: roots,
    ...(row.attempt_id ? {
      attempt_id: row.attempt_id,
      reservation_operation_id: row.reservation_operation_id,
    } : {}),
    ...(row.status === "pending" ? {
      pending_expires_at: Number(row.pending_expires_at),
    } : {}),
    ...(row.cancel_operation_id ? { cancel_operation_id: row.cancel_operation_id } : {}),
    ...(typeof row.operation_id === "string" ? { operation_id: row.operation_id } : {}),
    ...(row.operation_generation != null && Number.isSafeInteger(Number(row.operation_generation))
      ? { operation_generation: Number(row.operation_generation) } : {}),
    ...(row.status === "active" && Number.isSafeInteger(Number(row.expires_at))
      ? { expires_at: Number(row.expires_at) } : {}),
  });
}

async function activate(request, env, ownerKey, hostId) {
  const parsed = await readJson(request, MAX_ENROLLMENT_BODY_BYTES);
  if (!parsed.ok || !isRecordWithOnly(parsed.value, ["attempt_id", "reservation_operation_id", "expected_generation", "activation_operation_id"])
      || !UUID.test(parsed.value.attempt_id) || !UUID.test(parsed.value.reservation_operation_id)
      || !UUID.test(parsed.value.activation_operation_id) || !Number.isSafeInteger(parsed.value.expected_generation)
      || parsed.value.expected_generation < 1) return reply({ error: "invalid_activation" }, 400);
  const proof = request.headers.get("x-temote-fabric-host-grant");
  if (typeof proof !== "string" || proof.length < 32 || proof.length > 256) return unauthorizedHost();
  const digest = await sha256Hex(proof);
  const now = Date.now();
  const db = env.OBSERVATION_DB;
  const changed = await db.prepare(
    "UPDATE fabric_host_grants SET status = 'active', expires_at = ?, activated_at = ?, pending_expires_at = NULL, activation_operation_id = ?, updated_at = ? WHERE host_id = ? AND owner_key = ? AND status = 'pending' AND attempt_id = ? AND reservation_operation_id = ? AND generation = ? AND grant_digest = ? AND pending_expires_at > ?",
  ).bind(now + GRANT_TTL_MS, now, parsed.value.activation_operation_id, now, hostId, ownerKey, parsed.value.attempt_id,
    parsed.value.reservation_operation_id, parsed.value.expected_generation, digest, now).run();
  let row = await readGrant(db, hostId);
  if (!changes(changed) && !(row?.status === "active" && row.owner_key === ownerKey
      && row.attempt_id === parsed.value.attempt_id && row.activation_operation_id === parsed.value.activation_operation_id
      && row.reservation_operation_id === parsed.value.reservation_operation_id
      && Number(row.generation) === parsed.value.expected_generation && row.grant_digest === digest)) return reply({ error: "activation_conflict" }, 409);
  if (!row || row.owner_key !== ownerKey || row.status !== "active") return reply({ error: "activation_unavailable" }, 503);
  return grantReply(row, ownerKey);
}

async function cancelPending(request, env, ownerKey, hostId) {
  const parsed = await readJson(request, MAX_ENROLLMENT_BODY_BYTES);
  if (!parsed.ok || !isRecordWithOnly(parsed.value, ["attempt_id", "reservation_operation_id", "expected_generation", "operation_id"])
      || !UUID.test(parsed.value.attempt_id) || !UUID.test(parsed.value.reservation_operation_id)
      || !UUID.test(parsed.value.operation_id) || !Number.isSafeInteger(parsed.value.expected_generation)
      || parsed.value.expected_generation < 1) return reply({ error: "invalid_cancellation" }, 400);
  const now = Date.now();
  const changed = await env.OBSERVATION_DB.prepare(
    "UPDATE fabric_host_grants SET status = 'revoked', generation = generation + 1, revoked_at = ?, cancel_operation_id = ?, operation_id = ?, operation_generation = generation + 1, pending_expires_at = NULL, updated_at = ? WHERE host_id = ? AND owner_key = ? AND status = 'pending' AND attempt_id = ? AND reservation_operation_id = ? AND generation = ?",
  ).bind(now, parsed.value.operation_id, parsed.value.operation_id, now, hostId, ownerKey,
    parsed.value.attempt_id, parsed.value.reservation_operation_id, parsed.value.expected_generation).run();
  const row = await readGrant(env.OBSERVATION_DB, hostId);
  if (changes(changed)) return grantReply(row, ownerKey);
  if (row?.status === "revoked" && row.owner_key === ownerKey
      && row.cancel_operation_id === parsed.value.operation_id
      && row.attempt_id === parsed.value.attempt_id
      && row.reservation_operation_id === parsed.value.reservation_operation_id
      && Number(row.generation) === parsed.value.expected_generation + 1) {
    return grantReply(row, ownerKey);
  }
  return reply({ error: "cancellation_conflict" }, 409);
}

async function revoke(request, env, ownerKey, hostId) {
  const parsed = await readJson(request, MAX_ENROLLMENT_BODY_BYTES);
  if (!parsed.ok || !isRecordWithOnly(parsed.value, ["operation_id", "expected_generation"])
      || !UUID.test(parsed.value.operation_id) || !Number.isSafeInteger(parsed.value.expected_generation)) {
    return reply({ error: "invalid_revocation" }, 400);
  }
  const db = env.OBSERVATION_DB;
  const now = Date.now();
  const result = await db.batch([
    db.prepare("UPDATE fabric_host_grants SET status = 'revoked', generation = generation + 1, revoked_at = ?, operation_id = ?, operation_generation = generation + 1, updated_at = ? WHERE host_id = ? AND owner_key = ? AND status = 'active' AND generation = ?")
      .bind(now, parsed.value.operation_id, now, hostId, ownerKey, parsed.value.expected_generation),
    db.prepare("INSERT OR IGNORE INTO fabric_host_grant_outbox (operation_id, host_id, generation, state, created_at, updated_at) SELECT ?, host_id, operation_generation, 'pending', ?, ? FROM fabric_host_grants WHERE host_id = ? AND owner_key = ? AND operation_id = ? AND status = 'revoked'")
      .bind(parsed.value.operation_id, now, now, hostId, ownerKey, parsed.value.operation_id),
  ]);
  let row = await readGrant(db, hostId);
  if (changes(result?.[0])) return grantReply(row, ownerKey);
  if (row?.status === "revoked" && row.owner_key === ownerKey && row.operation_id === parsed.value.operation_id
      && Number(row.generation) === parsed.value.expected_generation + 1
      && Number(row.operation_generation) === parsed.value.expected_generation + 1) {
    // The D1 tombstone is the authorization commit point. The outbox is a
    // retryable Durable Object fence and never restores authority.
    await db.prepare("INSERT OR IGNORE INTO fabric_host_grant_outbox (operation_id, host_id, generation, state, created_at, updated_at) VALUES (?, ?, ?, 'pending', ?, ?)")
      .bind(parsed.value.operation_id, hostId, Number(row.operation_generation), now, now).run();
    return grantReply(row, ownerKey);
  }
  return reply({ error: "grant_generation_conflict" }, 409);
}

async function updateRoots(request, env, ownerKey, hostId) {
  const parsed = await readJson(request, MAX_ENROLLMENT_BODY_BYTES);
  if (!parsed.ok || !isRecordWithOnly(parsed.value, ["operation_id", "expected_generation", "root_names"])
      || !UUID.test(parsed.value.operation_id) || !Number.isSafeInteger(parsed.value.expected_generation)
      || !validRootNames(parsed.value.root_names) || parsed.value.root_names.length === 0) {
    return reply({ error: "invalid_root_update" }, 400);
  }
  const now = Date.now();
  const rootsJson = JSON.stringify([...parsed.value.root_names].sort());
  const db = env.OBSERVATION_DB;
  const result = await db.batch([
    db.prepare("UPDATE fabric_host_grants SET roots_json = ?, generation = generation + 1, operation_id = ?, operation_generation = generation + 1, updated_at = ? WHERE host_id = ? AND owner_key = ? AND status = 'active' AND generation = ? AND expires_at > ?")
      .bind(rootsJson, parsed.value.operation_id, now, hostId, ownerKey, parsed.value.expected_generation, now),
    db.prepare("INSERT OR IGNORE INTO fabric_host_grant_outbox (operation_id, host_id, generation, state, created_at, updated_at) SELECT ?, host_id, operation_generation, 'pending', ?, ? FROM fabric_host_grants WHERE host_id = ? AND owner_key = ? AND operation_id = ?")
      .bind(parsed.value.operation_id, now, now, hostId, ownerKey, parsed.value.operation_id),
  ]);
  const row = await readGrant(db, hostId);
  if (changes(result?.[0])) return grantReply(row, ownerKey);
  if (row?.owner_key === ownerKey && row.operation_id === parsed.value.operation_id
      && row.roots_json === rootsJson && Number(row.generation) === parsed.value.expected_generation + 1
      && Number(row.operation_generation) === parsed.value.expected_generation + 1) return grantReply(row, ownerKey);
  return reply({ error: "grant_generation_conflict" }, 409);
}

async function readGrant(db, hostId) {
  return db.prepare("SELECT * FROM fabric_host_grants WHERE host_id = ?").bind(hostId).first();
}

function matchesPendingReservation(row, ownerKey, body, rootsJson, now) {
  return Boolean(row?.owner_key === ownerKey && row.status === "pending"
    && row.attempt_id === body.attempt_id
    && row.operation_id === body.operation_id
    && row.reservation_operation_id === body.operation_id
    && row.grant_id === body.grant_id
    && row.grant_digest === body.grant_digest.toLowerCase()
    && row.roots_json === rootsJson
    && Number(row.generation) === body.expected_generation + 1
    && Number(row.pending_expires_at) > now);
}

function grantReply(row, ownerKey) {
  let roots = [];
  try { roots = JSON.parse(row?.roots_json ?? "[]"); } catch { return reply({ error: "grant_state_invalid" }, 503); }
  if (!validRootNames(roots)) return reply({ error: "grant_state_invalid" }, 503);
  return reply({
    host_id: row.host_id,
    owner_key: ownerKey,
    grant_id: row.grant_id,
    generation: Number(row.generation),
    status: row.status,
    root_names: roots,
    ...(typeof row.attempt_id === "string" ? { attempt_id: row.attempt_id } : {}),
    ...(typeof row.reservation_operation_id === "string"
      ? { reservation_operation_id: row.reservation_operation_id } : {}),
    ...(typeof row.cancel_operation_id === "string"
      ? { cancel_operation_id: row.cancel_operation_id } : {}),
    ...(typeof row.operation_id === "string" ? { operation_id: row.operation_id } : {}),
    ...(row.operation_generation != null && Number.isSafeInteger(Number(row.operation_generation))
      ? { operation_generation: Number(row.operation_generation) } : {}),
    ...(row.expires_at != null && Number.isSafeInteger(Number(row.expires_at))
      ? { expires_at: Number(row.expires_at) } : {}),
    ...(row.pending_expires_at != null && Number.isSafeInteger(Number(row.pending_expires_at))
      ? { pending_expires_at: Number(row.pending_expires_at) } : {}),
  });
}

function reply(value, status = 200) {
  return withCors(jsonResponse(value, status, { "cache-control": "no-store" }));
}

async function ownerKeyFor(identity) {
  return sha256Hex(`${identity.issuer}\0${identity.subject}`);
}

async function sha256Hex(value) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  return [...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function constantTimeEqual(left, right) {
  if (typeof left !== "string" || typeof right !== "string" || left.length !== right.length) return false;
  let diff = 0;
  for (let index = 0; index < left.length; index += 1) diff |= left.charCodeAt(index) ^ right.charCodeAt(index);
  return diff === 0;
}

function validDigest(value) { return typeof value === "string" && DIGEST.test(value); }
function validRootNames(value) {
  return Array.isArray(value) && value.length <= 32
    && value.every((name) => typeof name === "string" && ROOT_NAME.test(name) && name !== "." && name !== "..")
    && new Set(value).size === value.length;
}
function isRecordWithOnly(value, allowed) {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).every((key) => allowed.includes(key));
}
function isD1(value) { return typeof value?.prepare === "function" && typeof value?.batch === "function"; }
function changes(result) { return Number(result?.meta?.changes ?? result?.changes ?? 0) > 0; }
function boundedEmail(value) { return typeof value === "string" && value.length <= 320 ? value : "-"; }
