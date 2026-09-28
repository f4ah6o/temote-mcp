from __future__ import annotations

import json
import hashlib
import threading
import tempfile
import unittest
from unittest.mock import patch
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from .memory_continuity import (
    OFFLINE_PROOF_MARKER,
    REPLAY_MANIFEST_FIELDS,
    _baseline_not_implemented,
    _source_identity,
    _supported_item,
    _summary_matches_supported_policy,
    McpHttpClient,
    TASK_A_COMPLETE_MARKER,
    TASK_B_COMPLETE_MARKER,
    compare_runs,
    fixture_run,
    live_run,
    load_scenario,
    validate_artifact,
    validate_offline_proof,
    validate_queue_replay_manifest,
    write_task_a_complete_marker,
    write_task_b_complete_marker,
)

ROOT = Path(__file__).resolve().parents[1]
RUNS = ROOT / "dogfood" / "runs"


def passing_fixture(phase: str, *, synthesis_mode: str = "fixture") -> dict:
    scenario = load_scenario()
    value = fixture_run(phase, scenario, binary=Path(__file__))
    value["mode"] = "live"
    value["synthesis_mode"] = synthesis_mode
    value["selectors"] = {
        "backend": "opencode",
        "model": "gpt-5.6-luna",
        "effort": "max",
        "extractor_profile": "openai_compatible:glm-5.3-flash",
    }
    return value


class MemoryContinuityTests(unittest.TestCase):
    def test_scenario_uses_ordinary_task_language_and_exact_policy_quotes(self):
        scenario = load_scenario()
        self.assertEqual(scenario["repository_key"], "github:temote-tests/memory-continuity")
        self.assertEqual(scenario["task_a"]["constraint"], "Report output format must be JSON.")
        self.assertEqual(scenario["task_b"]["constraint"], "Report output format must be TOML.")
        for task in ("task_a", "task_b"):
            instruction = scenario[task]["instruction"].casefold()
            self.assertNotIn("memory", instruction)
            self.assertNotIn("remember", instruction)
            self.assertNotIn("summarize", instruction)
            self.assertIn("\n", instruction)

    def test_artifact_records_fixture_as_not_run_and_never_contains_task_body(self):
        scenario = load_scenario()
        result = fixture_run("candidate", scenario)
        validate_artifact(result)
        self.assertEqual(result["outcome"], "not_run")
        self.assertEqual(result["mode"], "fixture")
        rendered = json.dumps(result)
        self.assertNotIn(scenario["task_a"]["instruction"], rendered)
        self.assertNotIn(scenario["task_b"]["instruction"], rendered)
        self.assertTrue(all(state == "not_run" for state in result["gates"].values()))

    def test_comparison_does_not_qualify_fixture_or_missing_external_gates(self):
        baseline = passing_fixture("baseline")
        baseline["outcome"] = "not_implemented"
        baseline["gates"]["task_a_constraint_supported"] = "not_implemented"
        candidate = passing_fixture("candidate", synthesis_mode="fixture")
        candidate["outcome"] = "pass"
        candidate["gates"] = {key: "pass" for key in candidate["gates"]}
        validate_artifact(baseline)
        validate_artifact(candidate)
        result = compare_runs(baseline, candidate)
        self.assertEqual(result["qualification"], "blocked")
        self.assertTrue(result["baseline_honest_not_implemented"])
        self.assertFalse(result["binaries_identified"] is False)
        self.assertTrue(any(value == "not_run" for value in result["gates"].values()))

    def test_comparison_requires_identical_selectors_and_inputs(self):
        baseline = passing_fixture("baseline", synthesis_mode="live")
        baseline["outcome"] = "not_implemented"
        baseline["gates"]["task_a_constraint_supported"] = "not_implemented"
        candidate = passing_fixture("candidate", synthesis_mode="live")
        candidate["outcome"] = "pass"
        candidate["gates"] = {key: "pass" for key in candidate["gates"]}
        gates = {
            name: "pass" for name in (
                "issue_completion", "memory_pipeline", "knowledge_quality", "head_switch",
                "offline_host", "tenancy", "retry_recovery", "tests", "final_diff",
            )
        }
        self.assertEqual(compare_runs(baseline, candidate, independent_gates=gates)["qualification"], "qualified")

        candidate["selectors"]["model"] = "different-model"
        self.assertEqual(compare_runs(baseline, candidate, independent_gates=gates)["qualification"], "blocked")
        candidate["selectors"]["model"] = baseline["selectors"]["model"]
        candidate["provenance"]["input_sha256"] = "f" * 64
        self.assertEqual(compare_runs(baseline, candidate, independent_gates=gates)["qualification"], "blocked")

    def test_baseline_empty_projection_is_not_implementation(self):
        empty = {"relevant_facts": [], "constraints": [], "unresolved": []}
        self.assertTrue(_baseline_not_implemented({"memory": {"state": "disabled"}}, empty))
        self.assertTrue(_baseline_not_implemented({"memory": {"state": "not_configured"}}, empty))
        self.assertFalse(_baseline_not_implemented({"memory": {"state": "ready_empty"}}, empty))
        existing = {"constraints": [{"text": "An item", "status": "current", "kind": "constraint"}]}
        self.assertFalse(_baseline_not_implemented({"memory": {"state": "disabled"}}, existing))

    def test_current_item_requires_repository_scope_instruction_support_for_task(self):
        context = {"constraints": [{
            "knowledge_id": "knowledge-a",
            "text": "Report output format must be JSON.",
            "status": "current",
            "scope_type": "repository",
            "scope_id": "github:temote-tests/memory-continuity",
            "authority": "derived",
            "support_refs": [{
                "observation_id": "observation-a",
                "cloud_seq": 1,
                "observation_kind": "instruction",
                "task_id": "task-a",
            }],
        }]}
        repo = "github:temote-tests/memory-continuity"
        self.assertIsNotNone(_supported_item(context, "JSON", repo, "constraint", "task-a"))
        self.assertIsNone(_supported_item(context, "JSON", repo, "constraint", "task-b"))
        context["constraints"][0]["scope_id"] = "github:other/repo"
        self.assertIsNone(_supported_item(context, "JSON", repo, "constraint", "task-a"))

    def test_current_summary_must_match_policy_and_instruction_support(self):
        reference = {
            "observation_id": "task-a-instruction",
            "cloud_seq": 17,
            "task_id": "task-a",
            "observation_kind": "instruction",
            "role": "summary_quote",
        }
        context = {"current_summary": {
            "knowledge_summary": "Report output format must be JSON.",
            "knowledge_summary_refs": [reference],
        }}

        self.assertTrue(_summary_matches_supported_policy(
            context, "Report output format must be JSON.", "task-a",
        ))
        self.assertTrue(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [{**reference, "role": "direct_quote"}],
            }},
            "Report output format must be JSON.", "task-a",
        ))
        self.assertFalse(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [{**reference, "task_id": "task-b"}],
            }},
            "Report output format must be JSON.", "task-a",
        ))
        self.assertFalse(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [{**reference, "observation_kind": "execution_state"}],
            }},
            "Report output format must be JSON.", "task-a",
        ))
        self.assertFalse(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [{**reference, "role": "verification"}],
            }},
            "Report output format must be JSON.", "task-a",
        ))
        self.assertFalse(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [],
            }},
            "Report output format must be JSON.", "task-a",
        ))
        self.assertFalse(_summary_matches_supported_policy(
            {"current_summary": {
                "knowledge_summary": "Report output format must be JSON.",
                "knowledge_summary_refs": [reference],
            }},
            "Report output format must be TOML.", "task-a",
        ))

    def test_offline_marker_is_exact_and_confined_to_ignored_artifacts(self):
        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            path = Path(directory) / "offline.proof"
            path.write_bytes(OFFLINE_PROOF_MARKER)
            self.assertTrue(validate_offline_proof(path))
            path.write_bytes(b"host stopped\n")
            self.assertFalse(validate_offline_proof(path))

        with tempfile.TemporaryDirectory() as directory:
            outside = Path(directory) / "proof"
            outside.write_bytes(OFFLINE_PROOF_MARKER)
            with self.assertRaises(ValueError):
                validate_offline_proof(outside)

    def test_task_a_handshake_marker_is_bounded_private_and_confined(self):
        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            marker = Path(directory) / "task-a-complete"
            write_task_a_complete_marker(marker)
            self.assertEqual(marker.read_bytes(), TASK_A_COMPLETE_MARKER)
            self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                write_task_a_complete_marker(marker)

        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                write_task_a_complete_marker(Path(directory) / "task-a-complete")

    def test_task_b_handshake_marker_is_bounded_private_and_confined(self):
        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            marker = Path(directory) / "task-b-complete"
            write_task_b_complete_marker(marker)
            self.assertEqual(marker.read_bytes(), TASK_B_COMPLETE_MARKER)
            self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                write_task_b_complete_marker(marker)

        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                write_task_b_complete_marker(Path(directory) / "task-b-complete")

    def test_archived_source_identity_requires_both_verified_overrides(self):
        scenario = load_scenario()
        empty_diff = hashlib.sha256(b"").hexdigest()
        identity = _source_identity(
            Path(__file__), scenario,
            source_head="e0d6c7674f4d8d43999c77979687ca37cdd04ea7",
            working_diff_sha256=empty_diff,
        )
        self.assertEqual(identity["source_head"], "e0d6c7674f4d8d43999c77979687ca37cdd04ea7")
        self.assertEqual(identity["working_diff_sha256"], empty_diff)
        with self.assertRaises(ValueError):
            _source_identity(Path(__file__), scenario, source_head=identity["source_head"])

    def test_only_repository_context_capability_errors_mean_baseline_not_implemented(self):
        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["content-length"])))
                if body["params"]["name"] == "context_resolve":
                    message = "missing or invalid params.arguments.session_id"
                else:
                    message = "unknown tool: unrelated_tool"
                encoded = json.dumps({"jsonrpc": "2.0", "id": body["id"],
                                      "error": {"code": -32602, "message": message}}).encode()
                self.send_response(200)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def log_message(self, *_args):
                return

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        client = McpHttpClient(f"http://127.0.0.1:{server.server_port}/mcp", "test-token")
        try:
            with self.assertRaisesRegex(RuntimeError, "SESSION_REQUIRED"):
                client.call("context_resolve", {"repository": "github:example/repo"})
            with self.assertRaisesRegex(RuntimeError, "FABRIC_MCP_ERROR"):
                client.call("other_tool", {})
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=1)

    def test_archived_baseline_runs_both_tasks_but_does_not_wait_for_memory(self):
        class FakeClient:
            starts = 0
            marker = None

            def __init__(self, *_args, **_kwargs):
                pass

            def call(self, name, _arguments):
                if name == "context_status":
                    raise RuntimeError("SESSION_REQUIRED")
                if name == "context_resolve":
                    raise AssertionError("baseline must not fallback to repository retrieval")
                if name == "opencode_task_start":
                    self.starts += 1
                    return {"task_id": f"baseline-task-{self.starts}"}
                if name == "opencode_task_get":
                    if _arguments["task_id"] == "baseline-task-2":
                        self.assert_no_early_marker()
                    return {"status": "completed"}
                raise AssertionError(f"unexpected baseline tool: {name}")

            @classmethod
            def assert_no_early_marker(cls):
                if cls.marker is not None:
                    assert not cls.marker.exists(), "Task B marker appeared before terminal completion"

        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            marker = Path(directory) / "task-b-complete"
            FakeClient.marker = marker
            with patch("dogfood.memory_continuity.McpHttpClient", FakeClient):
                result = live_run(
                    phase="baseline", endpoint="https://fabric.example/mcp", token="test-token",
                    session_a="baseline-session-a", session_b="baseline-session-b",
                    backend="opencode", model="gpt-5.6-luna", effort="max",
                    extractor_profile="openai_compatible:glm-5.3-flash",
                    synthesis_mode="not_run", binary=Path(__file__), max_polls=2,
                    poll_interval=0, task_b_complete_file=marker,
                )
            self.assertEqual(marker.read_bytes(), TASK_B_COMPLETE_MARKER)
            self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
        self.assertEqual(result["outcome"], "not_implemented")
        self.assertEqual(result["synthesis_mode"], "not_run")
        self.assertEqual(sum(event["action"] == "opencode_task_start" for event in result["events"]), 2)
        self.assertEqual(sum(event["state"] == "completed" for event in result["events"]), 2)
        self.assertEqual(result["gates"]["task_a_constraint_supported"], "not_implemented")
        self.assertEqual(result["gates"]["task_b_constraint_current"], "not_implemented")
        self.assertEqual(result["gates"]["queue_replay_no_growth"], "not_run")

    def test_task_b_poll_limit_does_not_emit_terminal_handshake_marker(self):
        class FakeClient:
            starts = 0

            def __init__(self, *_args, **_kwargs):
                pass

            def call(self, name, arguments):
                if name == "context_status":
                    raise RuntimeError("SESSION_REQUIRED")
                if name == "opencode_task_start":
                    type(self).starts += 1
                    return {"task_id": f"baseline-task-{type(self).starts}"}
                if name == "opencode_task_get":
                    if arguments["task_id"] == "baseline-task-2":
                        self.assert_no_marker()
                        return {"status": "running"}
                    return {"status": "completed"}
                raise AssertionError(f"unexpected baseline tool: {name}")

            @staticmethod
            def assert_no_marker():
                assert not marker.exists(), "Task B marker appeared while task was running"

        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            marker = Path(directory) / "task-b-complete"
            with patch("dogfood.memory_continuity.McpHttpClient", FakeClient):
                result = live_run(
                    phase="baseline", endpoint="https://fabric.example/mcp", token="test-token",
                    session_a="baseline-session-a", session_b="baseline-session-b",
                    backend="opencode", model="gpt-5.6-luna", effort="max",
                    extractor_profile="openai_compatible:glm-5.3-flash",
                    synthesis_mode="not_run", binary=Path(__file__), max_polls=1,
                    poll_interval=0, task_b_complete_file=marker,
                )
            self.assertEqual(result["outcome"], "blocked")
            self.assertFalse(marker.exists())

    def test_task_b_failed_terminal_does_not_emit_completion_marker(self):
        class FakeClient:
            starts = 0
            marker = None

            def __init__(self, *_args, **_kwargs):
                pass

            def call(self, name, arguments):
                if name == "context_status":
                    raise RuntimeError("SESSION_REQUIRED")
                if name == "opencode_task_start":
                    type(self).starts += 1
                    return {"task_id": f"baseline-task-{type(self).starts}"}
                if name == "opencode_task_get":
                    if arguments["task_id"] == "baseline-task-2":
                        assert not type(self).marker.exists(), (
                            "Task B marker appeared before successful completion"
                        )
                        return {"status": "failed"}
                    return {"status": "completed"}
                raise AssertionError(f"unexpected baseline tool: {name}")

        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            marker = Path(directory) / "task-b-complete"
            FakeClient.marker = marker
            with patch("dogfood.memory_continuity.McpHttpClient", FakeClient):
                result = live_run(
                    phase="baseline", endpoint="https://fabric.example/mcp", token="test-token",
                    session_a="baseline-session-a", session_b="baseline-session-b",
                    backend="opencode", model="gpt-5.6-luna", effort="max",
                    extractor_profile="openai_compatible:glm-5.3-flash",
                    synthesis_mode="not_run", binary=Path(__file__), max_polls=2,
                    poll_interval=0, task_b_complete_file=marker,
                )
            self.assertEqual(result["outcome"], "fail")
            self.assertIn("failed", [event["state"] for event in result["events"]])
            self.assertFalse(marker.exists())

    def test_replay_manifest_requires_real_dispatch_and_unchanged_projection(self):
        RUNS.mkdir(mode=0o700, parents=True, exist_ok=True)
        manifest = {
            "schema_version": 1,
            "repository_key": "github:temote-tests/memory-continuity",
            "dispatch_count_before": 2,
            "dispatch_count_after": 3,
            "knowledge_count_before": 4,
            "knowledge_count_after": 4,
            "support_count_before": 5,
            "support_count_after": 5,
            "supersession_count_before": 1,
            "supersession_count_after": 1,
            "run_count_before": 2,
            "run_count_after": 2,
            "checkpoint_before": 4,
            "checkpoint_after": 4,
        }
        self.assertEqual(set(manifest), REPLAY_MANIFEST_FIELDS)
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            path = Path(directory) / "queue-replay.json"
            path.write_text(json.dumps(manifest), encoding="utf-8")
            self.assertTrue(validate_queue_replay_manifest(path, manifest["repository_key"]))
            manifest["knowledge_count_after"] += 1
            path.write_text(json.dumps(manifest), encoding="utf-8")
            self.assertFalse(validate_queue_replay_manifest(path, manifest["repository_key"]))
            manifest["knowledge_count_after"] -= 1
            manifest["dispatch_count_after"] = manifest["dispatch_count_before"]
            path.write_text(json.dumps(manifest), encoding="utf-8")
            self.assertFalse(validate_queue_replay_manifest(path, manifest["repository_key"]))

    def test_replay_manifest_has_no_extra_unbounded_fields(self):
        with tempfile.TemporaryDirectory(dir=RUNS) as directory:
            path = Path(directory) / "queue-replay.json"
            value = {field: 0 for field in REPLAY_MANIFEST_FIELDS}
            value["schema_version"] = 1
            value["repository_key"] = "github:temote-tests/memory-continuity"
            value["dispatch_count_after"] = 1
            value["task_body"] = "must not enter the safe manifest"
            path.write_text(json.dumps(value), encoding="utf-8")
            self.assertFalse(validate_queue_replay_manifest(path, value["repository_key"]))


if __name__ == "__main__":
    unittest.main()
