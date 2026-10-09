-- Browser-enrolled host authority is separate from MCP caller OAuth tokens.
-- D1 stores only a digest of the Host grant and logical root names.
CREATE TABLE IF NOT EXISTS fabric_host_grants (
  host_id TEXT PRIMARY KEY,
  owner_key TEXT NOT NULL CHECK (length(owner_key) = 64),
  grant_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK (generation >= 1),
  status TEXT NOT NULL CHECK (status IN ('pending', 'active', 'revoked')),
  grant_digest TEXT NOT NULL CHECK (length(grant_digest) = 64),
  roots_json TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  operation_generation INTEGER NOT NULL CHECK (operation_generation >= 1),
  reservation_operation_id TEXT NOT NULL,
  cancel_operation_id TEXT,
  activation_operation_id TEXT,
  pending_expires_at INTEGER,
  expires_at INTEGER,
  activated_at INTEGER,
  revoked_at INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS fabric_host_grants_owner_active
  ON fabric_host_grants (owner_key, status, expires_at, host_id);

-- A D1 revoke/root-update commit is the authorization boundary. This outbox
-- fences cached Durable Object state; retries are idempotent and can never
-- restore a stale generation.
CREATE TABLE IF NOT EXISTS fabric_host_grant_outbox (
  operation_id TEXT PRIMARY KEY,
  host_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK (generation >= 1),
  state TEXT NOT NULL CHECK (state IN ('pending', 'complete')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS fabric_host_grant_outbox_pending
  ON fabric_host_grant_outbox (state, updated_at, host_id);
