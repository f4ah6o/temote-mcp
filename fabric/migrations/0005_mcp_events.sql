-- Durable MCP Events state. Apply after 0004; secrets stay in D1, never in logs.
CREATE TABLE event_subscriptions (
  id TEXT PRIMARY KEY,
  principal TEXT NOT NULL,
  callback_url TEXT NOT NULL,
  name TEXT NOT NULL CHECK (name IN ('job.state.changed', 'session.state.changed')),
  arguments_json TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  job_id TEXT,
  instance_key TEXT NOT NULL,
  secret TEXT NOT NULL,
  previous_secret TEXT,
  rotate_until INTEGER,
  verified_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX event_subscriptions_resource ON event_subscriptions(host_id, session_id, name, expires_at);
CREATE TABLE event_projections (
  resource_key TEXT PRIMARY KEY,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  instance_key TEXT NOT NULL,
  name TEXT NOT NULL,
  job_id TEXT,
  state TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK (revision > 0),
  observed_at TEXT NOT NULL
);
CREATE TABLE event_outbox (
  delivery_id TEXT PRIMARY KEY,
  subscription_id TEXT NOT NULL REFERENCES event_subscriptions(id) ON DELETE CASCADE,
  event_id TEXT NOT NULL,
  event_body TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  instance_key TEXT NOT NULL,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  lease_until INTEGER,
  created_at INTEGER NOT NULL,
  UNIQUE(subscription_id, event_id)
);
CREATE INDEX event_outbox_due ON event_outbox(next_attempt_at, lease_until, expires_at);
