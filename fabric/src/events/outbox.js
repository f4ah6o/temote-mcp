import { currentSession, principalStillAllowed, sendThroughHost } from "./service.js";
import { durableReady } from "./repository.js";

const MAX_ATTEMPTS = 8;
const BATCH_SIZE = 2;

export function retryDelay(attempt) {
  return Math.min(3_600_000, 1000 * 2 ** Math.min(attempt, 12));
}

export function deliveryDisposition(status) {
  if (status >= 200 && status < 300) return "accepted";
  if (status >= 300 && status < 400 || status === 410 || status === 413) return "terminal_rejected";
  return "retry";
}

export async function sweepEventOutbox(env, now = Date.now()) {
  // Memory-only and pre-migration deployments share this scheduled handler.
  // A sender outage keeps durable entries pending; absent configuration or
  // schema must never break the independent memory sweep.
  if (!await durableReady(env)) return;
  const db = env?.OBSERVATION_DB;
  await db.batch([
    db.prepare("DELETE FROM event_outbox WHERE expires_at <= ? OR attempt_count >= ?").bind(now, MAX_ATTEMPTS),
    db.prepare("UPDATE event_subscriptions SET previous_secret = NULL, rotate_until = NULL WHERE rotate_until <= ?").bind(now),
    db.prepare("DELETE FROM event_subscriptions WHERE expires_at <= ?").bind(now),
  ]);
  const due = await db.prepare(`SELECT o.*, s.principal, s.callback_url, s.secret, s.previous_secret,
    s.rotate_until, s.expires_at AS subscription_expires_at FROM event_outbox o
    JOIN event_subscriptions s ON s.id = o.subscription_id
    WHERE o.next_attempt_at <= ? AND (o.lease_until IS NULL OR o.lease_until <= ?)
    ORDER BY o.next_attempt_at LIMIT ?`).bind(now, now, BATCH_SIZE).all();
  for (const row of due.results ?? []) {
    const lease = await db.prepare(`UPDATE event_outbox SET lease_until = ?
      WHERE delivery_id = ? AND (lease_until IS NULL OR lease_until <= ?) AND expires_at > ?`)
      .bind(now + 90_000, row.delivery_id, now, now).run();
    if (Number(lease.meta?.changes ?? 0) !== 1) continue;
    try { await deliver(db, env, row, now); }
    catch { await defer(db, row, now, false); }
  }
}

async function deliver(db, env, row, now) {
  if (!await principalStillAllowed(row.principal, env)) {
    await db.batch([
      db.prepare("DELETE FROM event_outbox WHERE subscription_id = ?").bind(row.subscription_id),
      db.prepare("DELETE FROM event_subscriptions WHERE id = ?").bind(row.subscription_id),
    ]);
    return;
  }
  const current = await currentSession(env, row.host_id, row.session_id);
  if (!current) { await defer(db, row, now, false); return; }
  if (current.instance_key !== row.instance_key) {
    await db.prepare("DELETE FROM event_outbox WHERE delivery_id = ?").bind(row.delivery_id).run();
    return;
  }
  // A subscription may have been removed or rotated while the Host check was
  // pending. Re-read it after that await and use only the current keys.
  const active = await db.prepare("SELECT principal, callback_url, secret, previous_secret, rotate_until, expires_at, instance_key FROM event_subscriptions WHERE id = ?")
    .bind(row.subscription_id).first();
  if (!active || active.instance_key !== row.instance_key || active.expires_at <= Date.now() || !await principalStillAllowed(active.principal, env)) {
    await db.prepare("DELETE FROM event_outbox WHERE delivery_id = ?").bind(row.delivery_id).run();
    return;
  }
  const result = await sendThroughHost(env, {
    kind: "delivery", url: active.callback_url, subscriptionId: row.subscription_id,
    secret: active.secret,
    ...(active.previous_secret && active.rotate_until > now ? { previousSecret: active.previous_secret } : {}),
    eventBody: row.event_body,
  });
  if (result.ok && deliveryDisposition(result.status) !== "retry") {
    await db.prepare("DELETE FROM event_outbox WHERE delivery_id = ?").bind(row.delivery_id).run();
    return;
  }
  const senderOffline = !result.ok && ["sender_unavailable", "timeout", "invalid_address"].includes(result.reason);
  await defer(db, row, now, !senderOffline);
}

async function defer(db, row, now, countAttempt) {
  const attempts = Number(row.attempt_count) + Number(countAttempt);
  if (attempts >= MAX_ATTEMPTS || now >= row.expires_at || now >= row.subscription_expires_at) {
    await db.prepare("DELETE FROM event_outbox WHERE delivery_id = ?").bind(row.delivery_id).run();
    return;
  }
  await db.prepare("UPDATE event_outbox SET attempt_count = ?, next_attempt_at = ?, lease_until = NULL WHERE delivery_id = ?")
    .bind(attempts, Math.min(row.expires_at, row.subscription_expires_at, now + (countAttempt ? retryDelay(attempts) : 60_000)), row.delivery_id).run();
}
