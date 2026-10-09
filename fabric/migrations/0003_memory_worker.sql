-- C4 durable memory worker state.  Queue messages are wake-up hints; this
-- migration keeps pending work, fencing state, and projection history in D1.

ALTER TABLE memory_checkpoints ADD COLUMN lease_token TEXT;
ALTER TABLE memory_checkpoints ADD COLUMN lease_until INTEGER;
ALTER TABLE memory_checkpoints ADD COLUMN fence INTEGER NOT NULL DEFAULT 0 CHECK (fence >= 0);
ALTER TABLE memory_checkpoints ADD COLUMN projection_epoch INTEGER NOT NULL DEFAULT 0 CHECK (projection_epoch >= 0);

ALTER TABLE memory_runs ADD COLUMN lease_token TEXT;
ALTER TABLE memory_runs ADD COLUMN fence INTEGER NOT NULL DEFAULT 0 CHECK (fence >= 0);
ALTER TABLE memory_runs ADD COLUMN projection_epoch INTEGER NOT NULL DEFAULT 0 CHECK (projection_epoch >= 0);
ALTER TABLE memory_runs ADD COLUMN outcome TEXT CHECK (outcome IS NULL OR outcome IN ('projected', 'empty'));
ALTER TABLE memory_runs ADD COLUMN input_count INTEGER NOT NULL DEFAULT 0 CHECK (input_count >= 0);

-- A requested producer builds in isolation. Readers continue using the active
-- producer until the requested generation catches up to the current D1 head.
CREATE TABLE memory_projection_heads (
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  requested_generation INTEGER NOT NULL CHECK (requested_generation > 0),
  requested_producer_version TEXT NOT NULL,
  active_generation INTEGER,
  active_producer_version TEXT,
  epoch INTEGER NOT NULL DEFAULT 1 CHECK (epoch > 0),
  published_at TEXT,
  PRIMARY KEY (owner_id, repository_key),
  CHECK ((active_generation IS NULL) = (active_producer_version IS NULL))
);

CREATE TABLE memory_commit_guards (
  guard_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  producer_version TEXT NOT NULL,
  run_id TEXT NOT NULL,
  from_seq INTEGER NOT NULL CHECK (from_seq >= 0),
  to_seq INTEGER NOT NULL CHECK (to_seq >= from_seq),
  fence INTEGER NOT NULL CHECK (fence > 0),
  lease_token TEXT NOT NULL,
  projection_epoch INTEGER NOT NULL CHECK (projection_epoch > 0),
  publish_projection INTEGER NOT NULL CHECK (publish_projection IN (0, 1)),
  created_at TEXT NOT NULL
);

CREATE TABLE memory_claim_guards (
  guard_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  producer_version TEXT NOT NULL,
  run_id TEXT NOT NULL,
  from_seq INTEGER NOT NULL CHECK (from_seq >= 0),
  to_seq INTEGER NOT NULL CHECK (to_seq >= from_seq),
  fence INTEGER NOT NULL CHECK (fence > 0),
  lease_token TEXT NOT NULL,
  projection_epoch INTEGER NOT NULL CHECK (projection_epoch > 0),
  max_attempts INTEGER NOT NULL CHECK (max_attempts > 0)
);

CREATE TRIGGER memory_claim_guard_valid
BEFORE INSERT ON memory_claim_guards
WHEN NOT EXISTS (
  SELECT 1
  FROM memory_checkpoints AS checkpoint
  JOIN memory_projection_heads AS head
    ON head.owner_id = NEW.owner_id AND head.repository_key = NEW.repository_key
  WHERE checkpoint.owner_id = NEW.owner_id
    AND checkpoint.repository_key = NEW.repository_key
    AND checkpoint.worker_id = NEW.worker_id
    AND checkpoint.producer_version = NEW.producer_version
    AND checkpoint.last_cloud_seq = NEW.from_seq
    AND checkpoint.fence = NEW.fence
    AND checkpoint.lease_token = NEW.lease_token
    AND checkpoint.lease_until > unixepoch('now')
    AND checkpoint.projection_epoch = NEW.projection_epoch
    AND head.epoch = NEW.projection_epoch
    AND head.requested_producer_version = NEW.producer_version
    AND NOT EXISTS (
      SELECT 1 FROM memory_runs AS existing
      WHERE existing.run_id = NEW.run_id
        AND (existing.status = 'completed' OR existing.attempt_count >= NEW.max_attempts)
    )
)
BEGIN
  SELECT RAISE(ABORT, 'memory_claim_fence_rejected');
END;

CREATE TRIGGER memory_claim_guard_complete
BEFORE DELETE ON memory_claim_guards
WHEN NOT EXISTS (
  SELECT 1 FROM memory_runs AS run
  WHERE run.run_id = OLD.run_id
    AND run.owner_id = OLD.owner_id
    AND run.repository_key = OLD.repository_key
    AND run.producer_version = OLD.producer_version
    AND run.from_seq = OLD.from_seq
    AND run.to_seq = OLD.to_seq
    AND run.status = 'running'
    AND run.attempt_count <= OLD.max_attempts
    AND run.lease_token = OLD.lease_token
    AND run.fence = OLD.fence
    AND run.projection_epoch = OLD.projection_epoch
)
BEGIN
  SELECT RAISE(ABORT, 'memory_claim_run_rejected');
END;

CREATE TRIGGER memory_commit_guard_valid
BEFORE INSERT ON memory_commit_guards
WHEN NOT EXISTS (
  SELECT 1
  FROM memory_checkpoints AS checkpoint
  JOIN memory_runs AS run ON run.run_id = NEW.run_id
  JOIN memory_projection_heads AS head
    ON head.owner_id = NEW.owner_id
   AND head.repository_key = NEW.repository_key
  WHERE checkpoint.owner_id = NEW.owner_id
    AND checkpoint.repository_key = NEW.repository_key
    AND checkpoint.worker_id = NEW.worker_id
    AND checkpoint.producer_version = NEW.producer_version
    AND checkpoint.last_cloud_seq = NEW.from_seq
    AND checkpoint.fence = NEW.fence
    AND checkpoint.lease_token = NEW.lease_token
    AND checkpoint.lease_until > unixepoch('now')
    AND checkpoint.projection_epoch = NEW.projection_epoch
    AND head.epoch = NEW.projection_epoch
    AND head.requested_producer_version = NEW.producer_version
    AND run.status = 'running'
    AND run.fence = NEW.fence
    AND run.lease_token = NEW.lease_token
    AND run.projection_epoch = NEW.projection_epoch
    AND run.producer_version = NEW.producer_version
    AND run.owner_id = NEW.owner_id
    AND run.repository_key = NEW.repository_key
    AND run.from_seq = NEW.from_seq
    AND run.to_seq = NEW.to_seq
    AND NOT EXISTS (
      SELECT 1 FROM memory_runs AS completed
      WHERE completed.run_id = NEW.run_id AND completed.status = 'completed'
    )
    AND (
      NEW.publish_projection = 0
      OR NOT EXISTS (
        SELECT 1 FROM observations AS newer
        WHERE newer.owner_id = NEW.owner_id
          AND newer.repository_key = NEW.repository_key
          AND newer.cloud_seq > NEW.to_seq
      )
    )
)
BEGIN
  SELECT RAISE(ABORT, 'memory_commit_fence_rejected');
END;

CREATE TRIGGER memory_projection_publish_guard
BEFORE UPDATE OF active_generation, active_producer_version ON memory_projection_heads
WHEN NEW.active_producer_version IS NOT OLD.active_producer_version
  AND NOT EXISTS (
    SELECT 1 FROM memory_commit_guards AS guard
    WHERE guard.owner_id = OLD.owner_id
      AND guard.repository_key = OLD.repository_key
      AND guard.producer_version = NEW.active_producer_version
      AND guard.projection_epoch = OLD.epoch
      AND guard.publish_projection = 1
      AND NOT EXISTS (
        SELECT 1 FROM observations AS newer
        WHERE newer.owner_id = OLD.owner_id
          AND newer.repository_key = OLD.repository_key
          AND newer.cloud_seq > guard.to_seq
      )
  )
BEGIN
  SELECT RAISE(ABORT, 'memory_projection_publish_requires_commit_guard');
END;

CREATE TRIGGER memory_checkpoint_commit_guard
BEFORE UPDATE OF last_cloud_seq ON memory_checkpoints
WHEN NEW.last_cloud_seq > OLD.last_cloud_seq
  AND NOT EXISTS (
    SELECT 1 FROM memory_commit_guards AS guard
    WHERE guard.owner_id = OLD.owner_id
      AND guard.repository_key = OLD.repository_key
      AND guard.worker_id = OLD.worker_id
      AND guard.producer_version = OLD.producer_version
      AND guard.from_seq = OLD.last_cloud_seq
      AND guard.to_seq = NEW.last_cloud_seq
      AND guard.fence = OLD.fence
      AND guard.lease_token = OLD.lease_token
      AND guard.projection_epoch = OLD.projection_epoch
  )
BEGIN
  SELECT RAISE(ABORT, 'memory_checkpoint_requires_commit_guard');
END;

-- Conditional UPDATEs that affect zero rows are successful SQL statements.
-- Assert the final batch state while the unconditional guard is still present
-- so a stale worker cannot leave only part of its projection committed.
CREATE TRIGGER memory_commit_guard_complete
BEFORE DELETE ON memory_commit_guards
WHEN NOT EXISTS (
  SELECT 1
  FROM memory_checkpoints AS checkpoint
  JOIN memory_runs AS run ON run.run_id = OLD.run_id
  JOIN memory_projection_heads AS head
    ON head.owner_id = OLD.owner_id AND head.repository_key = OLD.repository_key
  WHERE checkpoint.owner_id = OLD.owner_id
    AND checkpoint.repository_key = OLD.repository_key
    AND checkpoint.worker_id = OLD.worker_id
    AND checkpoint.producer_version = OLD.producer_version
    AND checkpoint.last_cloud_seq = OLD.to_seq
    AND checkpoint.fence = OLD.fence
    AND checkpoint.lease_token IS NULL
    AND checkpoint.lease_until IS NULL
    AND checkpoint.projection_epoch = OLD.projection_epoch
    AND run.owner_id = OLD.owner_id
    AND run.repository_key = OLD.repository_key
    AND run.producer_version = OLD.producer_version
    AND run.from_seq = OLD.from_seq
    AND run.to_seq = OLD.to_seq
    AND run.status = 'completed'
    AND run.lease_token IS NULL
    AND run.fence = OLD.fence
    AND run.projection_epoch = OLD.projection_epoch
    AND (
      OLD.publish_projection = 0
      OR (head.active_generation = head.requested_generation
          AND head.active_producer_version = OLD.producer_version
          AND head.epoch = OLD.projection_epoch)
    )
)
BEGIN
  SELECT RAISE(ABORT, 'memory_commit_incomplete');
END;

-- D1 observation ingest writes one pending row in the same transaction as the
-- observations.  Queue send success is recorded separately and is safe to
-- repeat after a lost response.
CREATE TABLE memory_outbox (
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  through_cloud_seq INTEGER NOT NULL CHECK (through_cloud_seq >= 0),
  queued_at TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_attempt_at INTEGER NOT NULL DEFAULT 0 CHECK (next_attempt_at >= 0),
  last_error_code TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (owner_id, repository_key)
);

CREATE INDEX memory_outbox_due
  ON memory_outbox(next_attempt_at, queued_at, owner_id, repository_key);

-- The C0 uniqueness constraint made it impossible to keep a superseded item
-- with the same semantic key and producer.  Rebuild the table and its two
-- reference tables while retaining all existing rows and provenance.
ALTER TABLE knowledge_support RENAME TO knowledge_support_legacy;
ALTER TABLE knowledge_supersession RENAME TO knowledge_supersession_legacy;
ALTER TABLE knowledge_items RENAME TO knowledge_items_legacy;

CREATE TABLE knowledge_items (
  knowledge_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  scope_type TEXT NOT NULL CHECK (scope_type IN ('user', 'repository', 'workspace', 'task', 'execution')),
  scope_id TEXT NOT NULL CHECK (length(trim(scope_id)) > 0),
  kind TEXT NOT NULL CHECK (kind IN ('fact', 'decision', 'constraint', 'observation', 'failure_pattern', 'unresolved', 'summary')),
  semantic_key TEXT NOT NULL,
  text TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('candidate', 'supported', 'current', 'superseded', 'retracted')),
  confidence REAL CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
  valid_from TEXT,
  valid_until TEXT,
  producer TEXT NOT NULL,
  producer_version TEXT NOT NULL,
  produced_at TEXT NOT NULL,
  source_through_cloud_seq INTEGER NOT NULL DEFAULT 0 CHECK (source_through_cloud_seq >= 0),
  support_incomplete INTEGER NOT NULL DEFAULT 0 CHECK (support_incomplete IN (0, 1)),
  CHECK (
    (scope_type = 'user' AND scope_id = owner_id)
    OR (scope_type = 'repository' AND scope_id = repository_key)
    OR (scope_type IN ('workspace', 'task', 'execution') AND length(trim(scope_id)) > 0)
  ),
  UNIQUE(owner_id, repository_key, knowledge_id)
);

INSERT INTO knowledge_items (
  knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
  semantic_key, text, status, confidence, valid_from, valid_until,
  producer, producer_version, produced_at, source_through_cloud_seq, support_incomplete
)
SELECT knowledge_id, owner_id, repository_key, scope_type, scope_id, kind,
       semantic_key, text, status, confidence, valid_from, valid_until,
       producer, producer_version, produced_at, source_through_cloud_seq,
       CASE WHEN (SELECT COUNT(*) FROM knowledge_support_legacy AS support
         WHERE support.owner_id = knowledge_items_legacy.owner_id
           AND support.repository_key = knowledge_items_legacy.repository_key
           AND support.knowledge_id = knowledge_items_legacy.knowledge_id) > 16
         THEN 1 ELSE 0 END
FROM knowledge_items_legacy;

CREATE TABLE knowledge_support (
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  knowledge_id TEXT NOT NULL,
  observation_cloud_seq INTEGER NOT NULL,
  observation_id TEXT NOT NULL,
  support_role TEXT NOT NULL,
  PRIMARY KEY(owner_id, repository_key, knowledge_id, observation_cloud_seq, support_role),
  FOREIGN KEY(owner_id, repository_key, knowledge_id)
    REFERENCES knowledge_items(owner_id, repository_key, knowledge_id) ON DELETE CASCADE,
  FOREIGN KEY(owner_id, repository_key, observation_cloud_seq, observation_id)
    REFERENCES observations(owner_id, repository_key, cloud_seq, observation_id)
);
INSERT INTO knowledge_support
SELECT owner_id, repository_key, knowledge_id, observation_cloud_seq, observation_id, support_role
FROM knowledge_support_legacy;

CREATE TABLE knowledge_supersession (
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  new_knowledge_id TEXT NOT NULL,
  old_knowledge_id TEXT NOT NULL,
  relationship TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(owner_id, repository_key, new_knowledge_id, old_knowledge_id, relationship),
  CHECK (new_knowledge_id <> old_knowledge_id),
  FOREIGN KEY(owner_id, repository_key, new_knowledge_id)
    REFERENCES knowledge_items(owner_id, repository_key, knowledge_id) ON DELETE CASCADE,
  FOREIGN KEY(owner_id, repository_key, old_knowledge_id)
    REFERENCES knowledge_items(owner_id, repository_key, knowledge_id) ON DELETE CASCADE
);
INSERT INTO knowledge_supersession
SELECT owner_id, repository_key, new_knowledge_id, old_knowledge_id, relationship, created_at
FROM knowledge_supersession_legacy;

DROP TABLE knowledge_support_legacy;
DROP TABLE knowledge_supersession_legacy;
DROP TABLE knowledge_items_legacy;

CREATE INDEX knowledge_items_current_scope
  ON knowledge_items(owner_id, repository_key, scope_type, scope_id, status, kind);
CREATE INDEX knowledge_items_repository_seq
  ON knowledge_items(owner_id, repository_key, source_through_cloud_seq);
CREATE INDEX knowledge_support_observation
  ON knowledge_support(owner_id, repository_key, observation_cloud_seq, knowledge_id);
CREATE INDEX knowledge_supersession_old
  ON knowledge_supersession(owner_id, repository_key, old_knowledge_id, new_knowledge_id);
