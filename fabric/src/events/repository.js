import { matches } from "./catalog.js";

export function eventsReady(env) {
  return typeof env?.OBSERVATION_DB?.prepare === "function"
    && typeof env?.OBSERVATION_DB?.batch === "function"
    && typeof env?.EVENT_SENDER_URL === "string"
    && typeof env?.EVENT_SENDER_BEARER === "string" && env.EVENT_SENDER_BEARER.length >= 32
    && typeof env?.EVENT_SENDER_ACCESS_CLIENT_ID === "string" && env.EVENT_SENDER_ACCESS_CLIENT_ID.length > 0
    && typeof env?.EVENT_SENDER_ACCESS_CLIENT_SECRET === "string" && env.EVENT_SENDER_ACCESS_CLIENT_SECRET.length > 0;
}

export async function durableReady(env) {
  if (!eventsReady(env)) return false;
  try {
    const url = new URL(env.EVENT_SENDER_URL);
    if (url.protocol !== "https:" || url.username || url.password || url.hash || url.search || url.pathname !== "/events/send") return false;
    await env.OBSERVATION_DB.prepare("SELECT id FROM event_subscriptions LIMIT 0").all();
    await env.OBSERVATION_DB.prepare("SELECT resource_key FROM event_projections LIMIT 0").all();
    await env.OBSERVATION_DB.prepare("SELECT delivery_id FROM event_outbox LIMIT 0").all();
    return true;
  } catch { return false; }
}

export async function repositoryReady(env) {
  if (!await durableReady(env)) return false;
  try {
    const response = await fetch(new URL("/events/health", env.EVENT_SENDER_URL), {
      method: "GET", redirect: "error", signal: AbortSignal.timeout(3000),
      headers: {
        authorization: `Bearer ${env.EVENT_SENDER_BEARER}`,
        "CF-Access-Client-Id": env.EVENT_SENDER_ACCESS_CLIENT_ID,
        "CF-Access-Client-Secret": env.EVENT_SENDER_ACCESS_CLIENT_SECRET,
      },
    });
    return response.status === 204;
  } catch { return false; }
}

export async function putSubscription(db, row) {
  await db.prepare(`INSERT INTO event_subscriptions
    (id, principal, callback_url, name, arguments_json, host_id, session_id, job_id, instance_key,
     secret, previous_secret, rotate_until, verified_at, expires_at, updated_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?)
    ON CONFLICT(id) DO UPDATE SET
      instance_key=excluded.instance_key,
      previous_secret=CASE WHEN event_subscriptions.secret <> excluded.secret THEN event_subscriptions.secret ELSE event_subscriptions.previous_secret END,
      rotate_until=CASE WHEN event_subscriptions.secret <> excluded.secret THEN excluded.updated_at + 300000 ELSE event_subscriptions.rotate_until END,
      secret=excluded.secret, verified_at=excluded.verified_at,
      expires_at=excluded.expires_at, updated_at=excluded.updated_at`)
    .bind(row.id, row.principal, row.callback_url, row.name, row.arguments_json, row.host_id,
      row.session_id, row.job_id, row.instance_key, row.secret, row.verified_at,
      row.expires_at, row.updated_at).run();
}

export async function removeSubscription(db, id, principal) {
  await db.batch([
    db.prepare("DELETE FROM event_outbox WHERE subscription_id = ? AND EXISTS (SELECT 1 FROM event_subscriptions WHERE id = ? AND principal = ?)").bind(id, id, principal),
    db.prepare("DELETE FROM event_subscriptions WHERE id = ? AND principal = ?").bind(id, principal),
  ]);
}

export async function recordTransition(db, transition, now = Date.now()) {
  const { name, host_id, session_id, instance_key, data } = transition;
  if (!["job.state.changed", "session.state.changed"].includes(name)) return false;
  const jobId = name === "job.state.changed" ? data.job_id : null;
  const resourceKey = JSON.stringify([host_id, session_id, instance_key, name, jobId]);
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const old = await db.prepare("SELECT state, revision FROM event_projections WHERE resource_key = ?").bind(resourceKey).first();
    if (!forwardStateChange(name, old?.state ?? null, data.state)) return false;
    const revision = (Number(old?.revision) || 0) + 1;
    const eventId = `evt_${await hash(JSON.stringify([resourceKey, revision]))}`;
    const event = { eventId, name, timestamp: data.timestamp, data: { ...data, previous_state: old?.state ?? null }, cursor: null };
    const body = JSON.stringify(event);
    if (new TextEncoder().encode(body).byteLength > 262_144) return false;
    const subscribers = await db.prepare("SELECT * FROM event_subscriptions WHERE host_id = ? AND session_id = ? AND name = ? AND instance_key = ? AND expires_at > ?")
      .bind(host_id, session_id, name, instance_key, now).all();
    const statements = [db.prepare(`INSERT INTO event_projections
      (resource_key, host_id, session_id, instance_key, name, job_id, state, revision, observed_at)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(resource_key) DO UPDATE SET state=excluded.state, revision=excluded.revision,
        observed_at=excluded.observed_at WHERE event_projections.revision = ? AND event_projections.state <> excluded.state`)
      .bind(resourceKey, host_id, session_id, instance_key, name, jobId, data.state, revision, data.timestamp, Number(old?.revision) || 0)];
    for (const sub of subscribers.results ?? []) {
      if (!matches(sub, transition)) continue;
      const deliveryId = `${sub.id}:${eventId}`;
      statements.push(db.prepare(`INSERT OR IGNORE INTO event_outbox
        (delivery_id, subscription_id, event_id, event_body, host_id, session_id, instance_key,
         next_attempt_at, expires_at, created_at)
        SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? FROM event_projections
        WHERE resource_key = ? AND revision = ? AND state = ?`)
        .bind(deliveryId, sub.id, eventId, body, host_id, session_id, instance_key,
          now, Math.min(sub.expires_at, now + 86_400_000), now, resourceKey, revision, data.state));
    }
    await db.batch(statements);
    const committed = await db.prepare("SELECT state, revision FROM event_projections WHERE resource_key = ?").bind(resourceKey).first();
    if (committed?.state === data.state && Number(committed.revision) >= revision) return true;
  }
  return false;
}

export function forwardStateChange(name, before, after) {
  const sequence = name === "job.state.changed"
    ? ["running", "completed", "failed", "stopped", "unknown"]
    : ["starting", "active", "stopping", "stopped", "crashed", "failed"];
  if (!sequence.includes(after) || before === after) return false;
  if (before === null) return true;
  if (name === "job.state.changed") return before === "running";
  if (before === "starting") return after !== "starting";
  if (before === "active") return after !== "starting" && after !== "active";
  return before === "stopping" && ["stopped", "crashed", "failed"].includes(after);
}

async function hash(value) {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)));
  return [...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
