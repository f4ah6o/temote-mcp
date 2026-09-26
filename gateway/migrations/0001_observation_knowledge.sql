-- Temote Fabric cloud observation / knowledge plane (O3C C0)
-- Contract-only migration. Runtime D1 binding and ingest are introduced in C1.

PRAGMA foreign_keys = ON;

CREATE TABLE observation_sources (
  owner_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  repository_key TEXT,
  source_base_revision INTEGER NOT NULL DEFAULT 0 CHECK (source_base_revision >= 0),
  source_head_revision INTEGER NOT NULL DEFAULT 0 CHECK (source_head_revision >= 0),
  acked_through_revision INTEGER NOT NULL DEFAULT 0 CHECK (acked_through_revision >= 0),
  cloud_head_seq INTEGER NOT NULL DEFAULT 0 CHECK (cloud_head_seq >= 0),
  journal_degraded INTEGER NOT NULL DEFAULT 0 CHECK (journal_degraded IN (0, 1)),
  gap_count INTEGER NOT NULL DEFAULT 0 CHECK (gap_count >= 0),
  last_synced_at TEXT,
  PRIMARY KEY (owner_id, host_id, session_id),
  CHECK (source_base_revision <= source_head_revision),
  CHECK (acked_through_revision <= source_head_revision)
);

CREATE INDEX observation_sources_owner_repository
  ON observation_sources(owner_id, repository_key);
CREATE INDEX observation_sources_owner_repository_freshness
  ON observation_sources(owner_id, repository_key, cloud_head_seq, last_synced_at);

CREATE TABLE observations (
  cloud_seq INTEGER PRIMARY KEY AUTOINCREMENT,
  owner_id TEXT NOT NULL,
  host_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  observation_id TEXT NOT NULL,
  source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
  schema_version INTEGER NOT NULL CHECK (schema_version > 0),
  repository_key TEXT,
  workspace_id TEXT,
  task_id TEXT,
  execution_id TEXT,
  operation_id TEXT,
  kind TEXT NOT NULL,
  action TEXT,
  actor_transport TEXT,
  actor_principal_ref TEXT,
  target_backend TEXT,
  content_kind TEXT,
  content_preview TEXT,
  content_digest TEXT,
  content_ref TEXT,
  state_status TEXT,
  state_revision INTEGER,
  evidence_refs TEXT NOT NULL DEFAULT '[]',
  observed_at TEXT NOT NULL,
  ingested_at TEXT NOT NULL,
  UNIQUE(owner_id, host_id, session_id, observation_id),
  UNIQUE(owner_id, host_id, session_id, source_revision),
  FOREIGN KEY(owner_id, host_id, session_id)
    REFERENCES observation_sources(owner_id, host_id, session_id)
);

CREATE INDEX observations_owner_repository_cloud_seq
  ON observations(owner_id, repository_key, cloud_seq);
CREATE INDEX observations_owner_repository_task_cloud_seq
  ON observations(owner_id, repository_key, task_id, cloud_seq);
CREATE INDEX observations_owner_session_cloud_seq
  ON observations(owner_id, host_id, session_id, cloud_seq);
CREATE INDEX observations_operation
  ON observations(owner_id, operation_id);

CREATE TABLE memory_checkpoints (
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  producer_version TEXT NOT NULL,
  last_cloud_seq INTEGER NOT NULL DEFAULT 0 CHECK (last_cloud_seq >= 0),
  last_success_at TEXT,
  last_error_at TEXT,
  last_error_code TEXT,
  stale INTEGER NOT NULL DEFAULT 1 CHECK (stale IN (0, 1)),
  PRIMARY KEY(owner_id, repository_key, worker_id, producer_version)
);

CREATE INDEX memory_checkpoints_owner_repository
  ON memory_checkpoints(owner_id, repository_key, last_cloud_seq);

CREATE TABLE memory_runs (
  run_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  producer_version TEXT NOT NULL,
  from_seq INTEGER NOT NULL CHECK (from_seq >= 0),
  to_seq INTEGER NOT NULL CHECK (to_seq >= from_seq),
  status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'completed', 'failed')),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  started_at TEXT,
  completed_at TEXT,
  error_code TEXT
);

CREATE INDEX memory_runs_owner_repository_range
  ON memory_runs(owner_id, repository_key, producer_version, from_seq, to_seq);
CREATE INDEX memory_runs_owner_repository_status
  ON memory_runs(owner_id, repository_key, status);

CREATE TABLE knowledge_items (
  knowledge_id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  repository_key TEXT NOT NULL,
  scope_type TEXT NOT NULL CHECK (scope_type IN ('user', 'repository', 'workspace', 'task', 'execution')),
  scope_id TEXT,
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
  UNIQUE(owner_id, repository_key, scope_type, scope_id, kind, semantic_key, producer, producer_version)
);

CREATE INDEX knowledge_items_current_scope
  ON knowledge_items(owner_id, repository_key, scope_type, scope_id, status, kind);
CREATE INDEX knowledge_items_repository_seq
  ON knowledge_items(owner_id, repository_key, source_through_cloud_seq);

CREATE TABLE knowledge_support (
  knowledge_id TEXT NOT NULL,
  observation_cloud_seq INTEGER NOT NULL,
  observation_id TEXT NOT NULL,
  support_role TEXT NOT NULL,
  PRIMARY KEY(knowledge_id, observation_cloud_seq, support_role),
  FOREIGN KEY(knowledge_id) REFERENCES knowledge_items(knowledge_id) ON DELETE CASCADE,
  FOREIGN KEY(observation_cloud_seq) REFERENCES observations(cloud_seq)
);

CREATE INDEX knowledge_support_observation
  ON knowledge_support(observation_cloud_seq, knowledge_id);

CREATE TABLE knowledge_supersession (
  new_knowledge_id TEXT NOT NULL,
  old_knowledge_id TEXT NOT NULL,
  relationship TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(new_knowledge_id, old_knowledge_id, relationship),
  CHECK (new_knowledge_id <> old_knowledge_id),
  FOREIGN KEY(new_knowledge_id) REFERENCES knowledge_items(knowledge_id) ON DELETE CASCADE,
  FOREIGN KEY(old_knowledge_id) REFERENCES knowledge_items(knowledge_id) ON DELETE CASCADE
);

CREATE INDEX knowledge_supersession_old
  ON knowledge_supersession(old_knowledge_id, new_knowledge_id);
