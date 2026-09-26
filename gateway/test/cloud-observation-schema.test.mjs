import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

import {
  FABRIC_AUTHORITY,
  FABRIC_FRESHNESS_CONTRACT,
  OBSERVATION_SCHEMA_VERSION,
  OBSERVATION_SYNC_CONTRACT,
  OWNER_REPOSITORY_IDENTITY_CONTRACT,
  canonicalRepositoryKey,
} from "../src/observation/schema.js";

const migration = fs.readFileSync(
  new URL("../migrations/0001_observation_knowledge.sql", import.meta.url),
  "utf8",
);

test("C0 cloud observation contract is versioned without changing execution authority", () => {
  assert.equal(OBSERVATION_SCHEMA_VERSION, 1);
  assert.deepEqual(FABRIC_AUTHORITY, {
    hostLive: "authoritative/live",
    replicatedObservation: "replicated_observed",
    derivedKnowledge: "derived",
  });
  assert.equal(OWNER_REPOSITORY_IDENTITY_CONTRACT.localPathIsIdentity, false);
  assert.equal(canonicalRepositoryKey({ forge: "GitHub", owner: "f4ah6o", repository: "temote-mcp" }), "github:f4ah6o/temote-mcp");
  assert.equal(canonicalRepositoryKey({ fingerprint: "abc123" }), "repo:abc123");
  assert.equal(canonicalRepositoryKey({}), null);
});

test("C0 sync and freshness contracts preserve contiguous ack and degraded state", () => {
  assert.equal(OBSERVATION_SYNC_CONTRACT.schemaVersion, 1);
  assert.match(OBSERVATION_SYNC_CONTRACT.ackRule, /contiguous.*D1-committed/);
  assert.match(OBSERVATION_SYNC_CONTRACT.queueRule, /never rolls back/);
  for (const field of [
    "source_gap_count",
    "cloud_observation_stale",
    "knowledge_stale",
    "partial",
  ]) {
    assert.ok(FABRIC_FRESHNESS_CONTRACT.required.includes(field));
  }
  assert.match(FABRIC_FRESHNESS_CONTRACT.gapRule, /partial\/degraded/);
});

test("C0 D1 migration declares all observation and knowledge state domains", () => {
  for (const table of [
    "observation_sources",
    "observations",
    "memory_checkpoints",
    "memory_runs",
    "knowledge_items",
    "knowledge_support",
    "knowledge_supersession",
  ]) {
    assert.match(migration, new RegExp(`CREATE TABLE ${table}\\b`));
  }
});

test("C0 D1 migration encodes observation dedupe and knowledge provenance constraints", () => {
  assert.match(migration, /UNIQUE\(owner_id, host_id, session_id, observation_id\)/);
  assert.match(migration, /UNIQUE\(owner_id, host_id, session_id, source_revision\)/);
  assert.match(migration, /UNIQUE\(owner_id, repository_key, cloud_seq, observation_id\)/);
  assert.match(
    migration,
    /FOREIGN KEY\(owner_id, repository_key, knowledge_id\)\s+REFERENCES knowledge_items\(owner_id, repository_key, knowledge_id\)/,
  );
  assert.match(
    migration,
    /FOREIGN KEY\(owner_id, repository_key, observation_cloud_seq, observation_id\)\s+REFERENCES observations\(owner_id, repository_key, cloud_seq, observation_id\)/,
  );
  assert.doesNotMatch(migration, /FOREIGN KEY\(observation_cloud_seq\) REFERENCES observations\(cloud_seq\)/);
  assert.match(migration, /CHECK \(new_knowledge_id <> old_knowledge_id\)/);
  assert.match(migration, /observations_owner_repository_cloud_seq/);
  assert.match(migration, /knowledge_items_current_scope/);
});

test("C0 knowledge relationship tables are owner and repository isolated", () => {
  const support = migration.match(/CREATE TABLE knowledge_support \(([\s\S]*?)\n\);/)?.[1] || "";
  const supersession = migration.match(/CREATE TABLE knowledge_supersession \(([\s\S]*?)\n\);/)?.[1] || "";

  for (const table of [support, supersession]) {
    assert.match(table, /owner_id TEXT NOT NULL/);
    assert.match(table, /repository_key TEXT NOT NULL/);
  }

  assert.match(
    supersession,
    /FOREIGN KEY\(owner_id, repository_key, new_knowledge_id\)\s+REFERENCES knowledge_items\(owner_id, repository_key, knowledge_id\)/,
  );
  assert.match(
    supersession,
    /FOREIGN KEY\(owner_id, repository_key, old_knowledge_id\)\s+REFERENCES knowledge_items\(owner_id, repository_key, knowledge_id\)/,
  );
});

test("C0 semantic dedupe requires canonical non-null scope identity", () => {
  const items = migration.match(/CREATE TABLE knowledge_items \(([\s\S]*?)\n\);/)?.[1] || "";

  assert.match(items, /scope_id TEXT NOT NULL CHECK \(length\(trim\(scope_id\)\) > 0\)/);
  assert.match(items, /scope_type = 'user' AND scope_id = owner_id/);
  assert.match(items, /scope_type = 'repository' AND scope_id = repository_key/);
  assert.match(
    items,
    /UNIQUE\(owner_id, repository_key, scope_type, scope_id, kind, semantic_key, producer, producer_version\)/,
  );
});
