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


if __name__ == "__main__":
    unittest.main()
