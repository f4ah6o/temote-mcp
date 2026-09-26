import pathlib
import sqlite3
import unittest


MIGRATION = (
    pathlib.Path(__file__).resolve().parents[1]
    / "migrations"
    / "0001_observation_knowledge.sql"
)


class FabricC0SchemaIntegrityTest(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.executescript(MIGRATION.read_text(encoding="utf-8"))

    def tearDown(self):
        self.db.close()

    def add_observation(self, owner, repository, host, session, observation_id, revision):
        self.db.execute(
            """
            INSERT INTO observation_sources (
              owner_id, host_id, session_id, repository_key,
              source_base_revision, source_head_revision
            ) VALUES (?, ?, ?, ?, ?, ?)
            """,
            (owner, host, session, repository, 0, revision),
        )
        cursor = self.db.execute(
            """
            INSERT INTO observations (
              owner_id, host_id, session_id, observation_id,
              source_revision, schema_version, repository_key,
              kind, observed_at, ingested_at
            ) VALUES (?, ?, ?, ?, ?, 1, ?, 'operation', ?, ?)
            """,
            (
                owner,
                host,
                session,
                observation_id,
                revision,
                repository,
                "2026-09-26T00:00:00Z",
                "2026-09-26T00:00:01Z",
            ),
        )
        return cursor.lastrowid

    def add_knowledge(
        self,
        knowledge_id,
        owner,
        repository,
        semantic_key,
        *,
        scope_type="repository",
        scope_id=None,
    ):
        if scope_id is None and scope_type == "repository":
            scope_id = repository
        self.db.execute(
            """
            INSERT INTO knowledge_items (
              knowledge_id, owner_id, repository_key, scope_type, scope_id,
              kind, semantic_key, text, status, producer,
              producer_version, produced_at
            ) VALUES (?, ?, ?, ?, ?, 'fact', ?, 'text', 'current', 'test', 'v1', ?)
            """,
            (
                knowledge_id,
                owner,
                repository,
                scope_type,
                scope_id,
                semantic_key,
                "2026-09-26T00:00:02Z",
            ),
        )

    def test_cross_owner_support_is_rejected(self):
        repository = "github:f4ah6o/temote-mcp"
        cloud_seq = self.add_observation(
            "owner-b", repository, "host-b", "session-b", "obs-b", 1
        )
        self.add_knowledge("knowledge-a", "owner-a", repository, "semantic-a")

        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(
                """
                INSERT INTO knowledge_support (
                  owner_id, repository_key, knowledge_id,
                  observation_cloud_seq, observation_id, support_role
                ) VALUES (?, ?, ?, ?, ?, 'support')
                """,
                ("owner-a", repository, "knowledge-a", cloud_seq, "obs-b"),
            )

    def test_observation_cloud_seq_and_id_must_match(self):
        repository = "github:f4ah6o/temote-mcp"
        first = self.add_observation(
            "owner-a", repository, "host-a", "session-a", "obs-a", 1
        )
        self.db.execute(
            """
            UPDATE observation_sources
            SET source_head_revision = 2
            WHERE owner_id = 'owner-a'
              AND host_id = 'host-a'
              AND session_id = 'session-a'
            """
        )
        self.db.execute(
            """
            INSERT INTO observations (
              owner_id, host_id, session_id, observation_id,
              source_revision, schema_version, repository_key,
              kind, observed_at, ingested_at
            ) VALUES (
              'owner-a', 'host-a', 'session-a', 'obs-b',
              2, 1, ?, 'operation', ?, ?
            )
            """,
            (
                repository,
                "2026-09-26T00:00:03Z",
                "2026-09-26T00:00:04Z",
            ),
        )
        self.add_knowledge("knowledge-a", "owner-a", repository, "semantic-a")

        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(
                """
                INSERT INTO knowledge_support (
                  owner_id, repository_key, knowledge_id,
                  observation_cloud_seq, observation_id, support_role
                ) VALUES (
                  'owner-a', ?, 'knowledge-a', ?, 'obs-b', 'support'
                )
                """,
                (repository, first),
            )

    def test_cross_owner_supersession_is_rejected(self):
        repository = "github:f4ah6o/temote-mcp"
        self.add_knowledge("knowledge-new", "owner-a", repository, "semantic-new")
        self.add_knowledge("knowledge-old", "owner-b", repository, "semantic-old")

        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(
                """
                INSERT INTO knowledge_supersession (
                  owner_id, repository_key, new_knowledge_id,
                  old_knowledge_id, relationship, created_at
                ) VALUES (?, ?, ?, ?, 'supersedes', ?)
                """,
                (
                    "owner-a",
                    repository,
                    "knowledge-new",
                    "knowledge-old",
                    "2026-09-26T00:00:05Z",
                ),
            )

    def test_scope_identity_cannot_bypass_semantic_dedupe(self):
        repository = "github:f4ah6o/temote-mcp"
        self.add_knowledge("knowledge-1", "owner-a", repository, "same-semantic")

        with self.assertRaises(sqlite3.IntegrityError):
            self.add_knowledge("knowledge-2", "owner-a", repository, "same-semantic")

        with self.assertRaises(sqlite3.IntegrityError):
            self.add_knowledge(
                "knowledge-null",
                "owner-a",
                repository,
                "null-scope",
                scope_type="task",
                scope_id=None,
            )

        with self.assertRaises(sqlite3.IntegrityError):
            self.add_knowledge(
                "knowledge-wrong-repo-scope",
                "owner-a",
                repository,
                "wrong-repo-scope",
                scope_type="repository",
                scope_id="github:other/repository",
            )


C1_UPDATE_SOURCE = """
UPDATE observation_sources
SET repository_key = COALESCE(repository_key, ?),
    source_base_revision = MAX(source_base_revision, ?),
    source_head_revision = MAX(source_head_revision, ?),
    journal_degraded = CASE WHEN journal_degraded = 1 OR ? = 1 THEN 1 ELSE 0 END,
    gap_count = MAX(gap_count, ?),
    acked_through_revision = MAX(
      acked_through_revision,
      acked_through_revision + (
        WITH ordered AS (
          SELECT source_revision,
                 ROW_NUMBER() OVER (ORDER BY source_revision) AS rn
          FROM observations
          WHERE owner_id = observation_sources.owner_id
            AND host_id = observation_sources.host_id
            AND session_id = observation_sources.session_id
            AND source_revision > observation_sources.acked_through_revision
            AND source_revision <= MAX(observation_sources.source_head_revision, ?)
        )
        SELECT COALESCE(
          (
            SELECT MIN(rn) - 1
            FROM ordered
            WHERE source_revision <> observation_sources.acked_through_revision + rn
          ),
          (SELECT COUNT(*) FROM ordered)
        )
      )
    ),
    cloud_head_seq = MAX(
      cloud_head_seq,
      COALESCE((
        SELECT MAX(cloud_seq)
        FROM observations
        WHERE owner_id = observation_sources.owner_id
          AND host_id = observation_sources.host_id
          AND session_id = observation_sources.session_id
      ), 0)
    ),
    last_synced_at = ?
WHERE owner_id = ? AND host_id = ? AND session_id = ?
"""


class FabricC1IngestSqliteTest(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        migrations = sorted(
            (pathlib.Path(__file__).resolve().parents[1] / "migrations").glob("*.sql")
        )
        for migration in migrations:
            self.db.executescript(migration.read_text(encoding="utf-8"))

    def tearDown(self):
        self.db.close()

    def add_source(self, session="session-c1", repository="forge:f4ah6o/temote-mcp"):
        self.db.execute(
            """
            INSERT INTO observation_sources (
              owner_id, host_id, session_id, repository_key, source_base_revision
            ) VALUES ('owner-c1', 'host-c1', ?, ?, 0)
            """,
            (session, repository),
        )

    def add_observation(
        self,
        revision,
        *,
        session="session-c1",
        observation_id=None,
        repository="forge:f4ah6o/temote-mcp",
        digest=None,
    ):
        observation_id = observation_id or f"obs-{revision}"
        digest = digest or (f"{revision:064x}"[-64:])
        return self.db.execute(
            """
            INSERT INTO observations (
              owner_id, host_id, session_id, observation_id,
              source_revision, schema_version, repository_key,
              kind, observed_at, ingested_at, payload_digest
            ) VALUES (
              'owner-c1', 'host-c1', ?, ?, ?, 1, ?,
              'execution_state', ?, ?, ?
            )
            """,
            (
                session,
                observation_id,
                revision,
                repository,
                "2026-09-26T00:00:00Z",
                "2026-09-26T00:00:01Z",
                digest,
            ),
        )

    def update_source(self, head, *, degraded=0, gaps=0, session="session-c1"):
        self.db.execute(
            C1_UPDATE_SOURCE,
            (
                "forge:f4ah6o/temote-mcp",
                0,
                head,
                degraded,
                gaps,
                head,
                "2026-09-26T00:00:02Z",
                "owner-c1",
                "host-c1",
                session,
            ),
        )

    def source(self, session="session-c1"):
        return self.db.execute(
            """
            SELECT source_head_revision, acked_through_revision, cloud_head_seq,
                   journal_degraded, gap_count
            FROM observation_sources
            WHERE owner_id = 'owner-c1'
              AND host_id = 'host-c1'
              AND session_id = ?
            """,
            (session,),
        ).fetchone()

    def test_c1_requires_payload_digest_for_new_rows(self):
        self.add_source()
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute(
                """
                INSERT INTO observations (
                  owner_id, host_id, session_id, observation_id,
                  source_revision, schema_version, repository_key,
                  kind, observed_at, ingested_at
                ) VALUES (
                  'owner-c1', 'host-c1', 'session-c1', 'missing-digest',
                  1, 1, 'forge:f4ah6o/temote-mcp',
                  'execution_state', ?, ?
                )
                """,
                ("2026-09-26T00:00:00Z", "2026-09-26T00:00:01Z"),
            )

    def test_c1_rejects_repository_mismatch(self):
        self.add_source()
        with self.assertRaises(sqlite3.IntegrityError):
            self.add_observation(1, repository="forge:other/repository")

    def test_c1_contiguous_ack_waits_for_gap_then_advances(self):
        self.add_source()
        self.add_observation(1)
        self.add_observation(3)
        self.update_source(3)
        self.assertEqual(self.source()[1], 1)

        self.add_observation(2)
        self.update_source(3)
        head, ack, cloud_head, degraded, gaps = self.source()
        self.assertEqual((head, ack), (3, 3))
        self.assertGreaterEqual(cloud_head, 3)
        self.assertEqual((degraded, gaps), (0, 0))

    def test_c1_degraded_metadata_is_not_silently_cleared(self):
        self.add_source()
        self.add_observation(1)
        self.add_observation(3)
        self.update_source(3, degraded=1, gaps=1)
        self.assertEqual(self.source()[1:], (1, 2, 1, 1))

        self.add_observation(2)
        self.update_source(3, degraded=0, gaps=0)
        head, ack, _cloud_head, degraded, gaps = self.source()
        self.assertEqual((head, ack, degraded, gaps), (3, 3, 1, 1))

    def test_c1_transaction_rolls_back_partial_ingest_on_conflict(self):
        session = "transaction-conflict"
        self.db.execute("BEGIN")
        try:
            self.add_source(session=session)
            self.add_observation(1, session=session, observation_id="first")
            with self.assertRaises(sqlite3.IntegrityError):
                self.add_observation(1, session=session, observation_id="conflict")
        finally:
            self.db.rollback()

        source_count = self.db.execute(
            "SELECT COUNT(*) FROM observation_sources WHERE session_id = ?",
            (session,),
        ).fetchone()[0]
        observation_count = self.db.execute(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?",
            (session,),
        ).fetchone()[0]
        self.assertEqual((source_count, observation_count), (0, 0))


if __name__ == "__main__":
    unittest.main()
