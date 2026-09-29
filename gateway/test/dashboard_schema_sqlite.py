import json
import pathlib
import sqlite3
import subprocess
import unittest


GATEWAY_ROOT = pathlib.Path(__file__).resolve().parents[1]
RAW_MARKER = "dashboard-sqlite-raw-fixture"
SECRET_MARKER = "dashboard-sqlite-secret-fixture"

CAPTURE_PRODUCTION_QUERIES = r"""
import {
  readDashboardHostReplicas,
  readDashboardReplicaSource,
  readDashboardTimeline,
} from "./src/dashboard/projection.js";

const ownerId = "owner-a";
const hostId = "host-a";
const sessionId = "session-a";
const raw = "dashboard-sqlite-raw-fixture";
const secret = "dashboard-sqlite-secret-fixture";
const source = (owner, host, session, head, cloudHead) => ({
  owner_id: owner,
  host_id: host,
  session_id: session,
  source_base_revision: 0,
  source_head_revision: head,
  acked_through_revision: head,
  cloud_head_seq: cloudHead,
  journal_degraded: 0,
  gap_count: 0,
  last_synced_at: "2026-09-29T00:00:00.000Z",
});
const validSourceRows = [
  source("owner-a", "host-a", "session-a", 3, 3),
  source("owner-a", "host-a", "session-b", 2, 2),
  source("owner-a", "host-b", "session-a", 4, 4),
  source("owner-b", "host-a", "session-a", 99, 99),
];
const event = (owner, host, session, seq, revision, kind) => ({
  owner_id: owner,
  host_id: host,
  session_id: session,
  cloud_seq: seq,
  source_revision: revision,
  kind,
  action: "task_start",
  target_backend: "codex",
  content_kind: "text",
  state_status: "accepted",
  state_revision: revision,
  observed_at: "2026-09-29T00:00:00.000Z",
  content_preview: raw,
  content_ref: secret,
  actor_principal_ref: secret,
  evidence_refs: JSON.stringify([{ kind: "stdout", ref: raw }]),
  argv: ["--token", secret],
  environment: { TEMOTE_MCP_TOKEN: secret },
});
const observations = [
  event("owner-a", "host-a", "session-a", 1, 1, "instruction"),
  event("owner-a", "host-a", "session-a", 2, 2, "operation_accepted"),
  event("owner-b", "host-a", "session-a", 20, 1, "execution_state"),
  event("owner-a", "host-b", "session-a", 21, 1, "execution_state"),
  event("owner-a", "host-a", "session-b", 22, 1, "execution_state"),
];

const calls = [];
const fakeD1 = {
  prepare(sql) {
    return {
      bind(...bindings) {
        const call = { sql, bindings, method: null };
        calls.push(call);
        return {
          async all() {
            call.method = "all";
            if (/\bFROM\s+observations\b/i.test(sql)) {
              const [owner, host, session] = bindings;
              const after = /cloud_seq\s*>\s*\?/i.test(sql) ? Number(bindings[3]) : null;
              const limit = Number(bindings.at(-1));
              const results = observations
                .filter((row) => row.owner_id === owner
                  && row.host_id === host
                  && row.session_id === session
                  && (after === null || row.cloud_seq > after))
                .sort((left, right) => /ORDER\s+BY\s+cloud_seq\s+DESC/i.test(sql)
                  ? right.cloud_seq - left.cloud_seq
                  : left.cloud_seq - right.cloud_seq)
                .slice(0, limit);
              return { results };
            }
            const [owner, ...hostIds] = bindings;
            const results = hostIds.flatMap((host) => {
              const rows = validSourceRows.filter((row) => row.owner_id === owner && row.host_id === host);
              if (rows.length === 0) return [];
              return [{
                host_id: host,
                last_synced_at: rows.map((row) => row.last_synced_at).sort().at(-1),
                source_head_revision: Math.max(...rows.map((row) => row.source_head_revision)),
                acked_through_revision: Math.max(...rows.map((row) => row.acked_through_revision)),
                cloud_head_seq: Math.max(...rows.map((row) => row.cloud_head_seq)),
                journal_degraded: Math.max(...rows.map((row) => row.journal_degraded)),
                gap_count: rows.reduce((total, row) => total + row.gap_count, 0),
              }];
            });
            return { results };
          },
          async first() {
            call.method = "first";
            const [owner, host, session] = bindings;
            return validSourceRows.find((row) => row.owner_id === owner
              && row.host_id === host && row.session_id === session) ?? null;
          },
        };
      },
    };
  },
};
const env = { OBSERVATION_DB: fakeD1 };

const inventory = await readDashboardHostReplicas(env, ownerId, ["host-a", "host-b"]);
const sourceResult = await readDashboardReplicaSource(env, ownerId, hostId, sessionId);
const initial = await readDashboardTimeline(env, ownerId, hostId, sessionId, null, 1);
observations.push(event("owner-a", "host-a", "session-a", 3, 3, "execution_state"));
const incremental = await readDashboardTimeline(
  env,
  ownerId,
  hostId,
  sessionId,
  initial.next_cursor,
  256,
);

console.log(JSON.stringify({
  calls,
  inventory: {
    ok: inventory.ok,
    hosts: [...inventory.byHost].map(([host, row]) => ({ host, source_head_revision: row.source_head_revision })),
  },
  source: { ok: sourceResult.ok, source_head_revision: sourceResult.source?.source_head_revision },
  initial: {
    ok: initial.ok,
    events: initial.events,
    next_cursor: initial.next_cursor,
    has_older: initial.has_older,
  },
  incremental: {
    ok: incremental.ok,
    events: incremental.events,
    after: incremental.after,
    has_more: incremental.has_more,
  },
}));
"""


def capture_production_queries():
    completed = subprocess.run(
        ["node", "--input-type=module", "-e", CAPTURE_PRODUCTION_QUERIES],
        cwd=GATEWAY_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


class DashboardProjectionSqliteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.capture = capture_production_queries()
        cls.db = sqlite3.connect(":memory:")
        cls.db.row_factory = sqlite3.Row
        migrations = sorted((GATEWAY_ROOT / "migrations").glob("*.sql"))
        if not migrations:
            raise AssertionError("no checked-in D1 migrations found")
        for migration in migrations:
            cls.db.executescript(migration.read_text(encoding="utf-8"))
        cls.seed_sources_and_observations()

    @classmethod
    def tearDownClass(cls):
        cls.db.close()

    @classmethod
    def seed_sources_and_observations(cls):
        sources = [
            ("owner-a", "host-a", "session-a", 3, 3),
            ("owner-a", "host-a", "session-b", 2, 2),
            ("owner-a", "host-b", "session-a", 4, 4),
            ("owner-b", "host-a", "session-a", 99, 99),
        ]
        for owner, host, session, head, cloud_head in sources:
            cls.db.execute(
                """
                INSERT INTO observation_sources (
                  owner_id, host_id, session_id, repository_key,
                  source_base_revision, source_head_revision, acked_through_revision,
                  cloud_head_seq, journal_degraded, gap_count, last_synced_at
                ) VALUES (?, ?, ?, NULL, 0, ?, ?, ?, 0, 0, ?)
                """,
                (
                    owner,
                    host,
                    session,
                    head,
                    head,
                    cloud_head,
                    "2026-09-29T00:00:00.000Z",
                ),
            )

        observations = [
            ("owner-a", "host-a", "session-a", 1, "instruction"),
            ("owner-a", "host-a", "session-a", 2, "operation_accepted"),
            ("owner-b", "host-a", "session-a", 1, "execution_state"),
            ("owner-a", "host-b", "session-a", 1, "execution_state"),
            ("owner-a", "host-a", "session-b", 1, "execution_state"),
        ]
        for index, (owner, host, session, revision, kind) in enumerate(observations, start=1):
            cls.db.execute(
                """
                INSERT INTO observations (
                  owner_id, host_id, session_id, observation_id,
                  source_revision, schema_version, repository_key, kind,
                  action, actor_transport, actor_principal_ref, target_backend,
                  content_kind, content_preview, content_digest, content_ref,
                  state_status, state_revision, evidence_refs, observed_at,
                  ingested_at, payload_digest
                ) VALUES (
                  ?, ?, ?, ?, ?, 1, NULL, ?, 'task_start', 'mcp', ?,
                  'codex', 'text', ?, ?, ?, 'accepted', ?, ?, ?, ?, ?
                )
                """,
                (
                    owner,
                    host,
                    session,
                    f"obs-{index}",
                    revision,
                    kind,
                    SECRET_MARKER,
                    RAW_MARKER,
                    f"{index:064x}",
                    SECRET_MARKER,
                    revision,
                    json.dumps([{"kind": "stdout", "ref": RAW_MARKER}]),
                    "2026-09-29T00:00:00.000Z",
                    "2026-09-29T00:00:01.000Z",
                    f"{index:064x}",
                ),
            )
        cls.db.commit()

    def execute_capture(self, index):
        call = self.capture["calls"][index]
        cursor = self.db.execute(call["sql"], call["bindings"])
        columns = [column[0] for column in cursor.description]
        rows = [dict(row) for row in cursor.fetchall()]
        return call, columns, rows

    def test_production_dashboard_queries_execute_with_exact_scope_and_safe_columns(self):
        capture = self.capture
        calls = capture["calls"]
        self.assertEqual(len(calls), 6)
        self.assertTrue(capture["inventory"]["ok"])
        self.assertTrue(capture["source"]["ok"])
        self.assertTrue(capture["initial"]["ok"])
        self.assertTrue(capture["incremental"]["ok"])

        self.assertEqual(
            [call["bindings"] for call in calls],
            [
                ["owner-a", "host-a", "host-b"],
                ["owner-a", "host-a", "session-a"],
                ["owner-a", "host-a", "session-a"],
                ["owner-a", "host-a", "session-a", 2],
                ["owner-a", "host-a", "session-a"],
                ["owner-a", "host-a", "session-a", 2, 257],
            ],
        )
        self.assertEqual([call["method"] for call in calls], ["all", "first", "first", "all", "first", "all"])
        self.assertEqual(capture["inventory"]["hosts"], [
            {"host": "host-a", "source_head_revision": 3},
            {"host": "host-b", "source_head_revision": 4},
        ])
        self.assertEqual(capture["source"]["source_head_revision"], 3)
        self.assertEqual([event["cloud_seq"] for event in capture["initial"]["events"]], [2])
        self.assertEqual(capture["initial"]["has_older"], True)
        self.assertEqual(capture["incremental"]["after"], 2)
        self.assertEqual([event["cloud_seq"] for event in capture["incremental"]["events"]], [3])

        inventory_call, inventory_columns, inventory_rows = self.execute_capture(0)
        self.assertIn("owner_id = ?", inventory_call["sql"])
        self.assertIn("host_id IN (?,?)", inventory_call["sql"])
        self.assertEqual(
            inventory_columns,
            [
                "host_id",
                "last_synced_at",
                "source_head_revision",
                "acked_through_revision",
                "cloud_head_seq",
                "journal_degraded",
                "gap_count",
            ],
        )
        self.assertEqual({row["host_id"] for row in inventory_rows}, {"host-a", "host-b"})
        self.assertEqual(next(row["source_head_revision"] for row in inventory_rows if row["host_id"] == "host-a"), 3)

        source_call, source_columns, source_rows = self.execute_capture(1)
        self.assertEqual(
            source_columns,
            [
                "host_id",
                "session_id",
                "source_base_revision",
                "source_head_revision",
                "acked_through_revision",
                "cloud_head_seq",
                "journal_degraded",
                "gap_count",
                "last_synced_at",
            ],
        )
        self.assertEqual(len(source_rows), 1)
        self.assertEqual((source_rows[0]["host_id"], source_rows[0]["session_id"]), ("host-a", "session-a"))
        self.assertEqual(source_rows[0]["source_head_revision"], 3)
        self.assertIn("owner_id = ? AND host_id = ? AND session_id = ?", source_call["sql"])

        initial_call, timeline_columns, initial_rows = self.execute_capture(3)
        self.assertEqual(
            timeline_columns,
            [
                "cloud_seq",
                "source_revision",
                "kind",
                "action",
                "target_backend",
                "content_kind",
                "state_status",
                "state_revision",
                "observed_at",
            ],
        )
        self.assertIn("owner_id = ? AND host_id = ? AND session_id = ?", initial_call["sql"])
        self.assertIn("ORDER BY cloud_seq DESC LIMIT ?", initial_call["sql"])
        self.assertEqual([row["cloud_seq"] for row in initial_rows], [2, 1])

        self.db.execute(
            """
            INSERT INTO observations (
              owner_id, host_id, session_id, observation_id,
              source_revision, schema_version, repository_key, kind,
              action, actor_transport, actor_principal_ref, target_backend,
              content_kind, content_preview, content_digest, content_ref,
              state_status, state_revision, evidence_refs, observed_at,
              ingested_at, payload_digest
            ) VALUES (
              'owner-a', 'host-a', 'session-a', 'obs-arrived-after-first-page',
              3, 1, NULL, 'execution_state', 'task_start', 'mcp', ?,
              'codex', 'text', ?, ?, ?, 'accepted', 3, ?, ?, ?, ?
            )
            """,
            (
                SECRET_MARKER,
                RAW_MARKER,
                "3".zfill(64),
                SECRET_MARKER,
                json.dumps([{"kind": "stdout", "ref": RAW_MARKER}]),
                "2026-09-29T00:00:00.000Z",
                "2026-09-29T00:00:01.000Z",
                "3".zfill(64),
            ),
        )
        self.db.commit()
        after_call, after_columns, after_rows = self.execute_capture(5)
        self.assertEqual(after_columns, timeline_columns)
        self.assertIn("AND cloud_seq > ?", after_call["sql"])
        self.assertIn("ORDER BY cloud_seq ASC LIMIT ?", after_call["sql"])
        self.assertEqual(len(after_rows), 1)
        self.assertEqual(after_rows[0]["source_revision"], 3)
        self.assertEqual(after_rows[0]["kind"], "execution_state")
        self.assertEqual(after_call["bindings"][-1], 257, "production API maximum of 256 rows uses one look-ahead row")

        for _call, columns, rows in [
            (inventory_call, inventory_columns, inventory_rows),
            (source_call, source_columns, source_rows),
            (initial_call, timeline_columns, initial_rows),
            (after_call, after_columns, after_rows),
        ]:
            self.assertFalse({"content_preview", "content_ref", "evidence_refs", "actor_principal_ref"} & set(columns))
            self.assertNotIn(RAW_MARKER, json.dumps(rows))
            self.assertNotIn(SECRET_MARKER, json.dumps(rows))

        self.assertNotIn(RAW_MARKER, json.dumps(capture["initial"]["events"]))
        self.assertNotIn(SECRET_MARKER, json.dumps(capture["initial"]["events"]))
        self.assertNotIn(RAW_MARKER, json.dumps(capture["incremental"]["events"]))
        self.assertNotIn(SECRET_MARKER, json.dumps(capture["incremental"]["events"]))


if __name__ == "__main__":
    unittest.main()
