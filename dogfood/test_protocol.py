import copy
import tempfile
import unittest
from pathlib import Path

from .protocol import compare, load, metrics, save, scenario, validate_run
from .runner import FakeAdapter, execute

SCENARIOS = Path(__file__).parent / "scenarios"


def fixture(name: str, phase: str = "baseline") -> dict:
    data = scenario(SCENARIOS / f"{name}.json")
    return execute(data, phase, FakeAdapter(), session_id="fixture", repository_head="a" * 40,
                   binary_identity="b" * 64, poll_interval=0,
                   gates={"rust": "pass", "gateway": "pass", "final_diff": "pass",
                          "tests": "pass", "git_status": "pass", "ci": "pass", "action_result": "pass"})


class ProtocolTests(unittest.TestCase):
    def test_all_scenarios_execute(self):
        for path in SCENARIOS.glob("*.json"):
            with self.subTest(path=path.name):
                run = fixture(path.stem)
                validate_run(run)
                self.assertEqual(run["outcome"], "pass")
                self.assertTrue(all(value == "pass" for value in run["assertions"].values()))

    def test_uncertain_start_reuses_identity(self):
        run = fixture("duplicate-start")
        calls = [x for x in run["events"] if x["tool"] == "codex_task_start"]
        self.assertEqual(len(calls), 2)
        self.assertEqual(run["metrics"]["duplicate_start_attempts"], 0)
        self.assertEqual(run["assertions"]["exact_retry"], "pass")

    def test_rediscovery_uses_list(self):
        run = fixture("task-rediscovery")
        self.assertEqual(run["assertions"]["tasks_can_be_rediscovered"], "pass")
        self.assertEqual(run["metrics"]["rediscovery_calls"], 2)

    def test_no_values_or_raw_evidence_are_persisted(self):
        run = fixture("delegation-lifecycle")
        rendered = str(run)
        self.assertNotIn("Read the current repository status", rendered)
        self.assertNotIn("fixture result", rendered)
        self.assertNotIn("fixture-evidence", rendered)

    def test_save_load_and_uninventable_metrics(self):
        run = fixture("delegation-lifecycle")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run.json"
            save(path, run)
            self.assertEqual(load(path), run)
            changed = copy.deepcopy(run)
            changed["metrics"]["tool_calls"] += 1
            with self.assertRaises(ValueError):
                validate_run(changed)

    def test_comparison_blocks_failed_gate_and_mismatched_revision(self):
        old = fixture("delegation-lifecycle")
        new = fixture("delegation-lifecycle", "candidate")
        self.assertEqual(compare(old, new, gates={"rust": "pass"})["qualification"], "qualified")
        new["metrics"]["tool_calls"] -= 1
        # Metrics without changed source events are rejected before qualification.
        with self.assertRaises(ValueError):
            compare(old, new, gates={"rust": "pass"}, target_metrics=["tool_calls"])
        new["metrics"]["tool_calls"] += 1
        self.assertEqual(compare(old, new, gates={"host": "not_run"},
                                 target_metrics=["tool_calls"])["qualification"], "blocked")
        new["scenario_fingerprint"] = "another-revision"
        with self.assertRaises(ValueError):
            compare(old, new)

    def test_terminal_reuse_reduces_one_call_with_same_scenario(self):
        data = scenario(SCENARIOS / "delegation-lifecycle.json")
        options = {"session_id": "fixture", "repository_head": "a" * 40,
                   "binary_identity": "b" * 64, "poll_interval": 0}
        old = execute(data, "baseline", FakeAdapter(), terminal_read_strategy="reread", **options)
        new = execute(data, "candidate", FakeAdapter(), terminal_read_strategy="reuse", **options)
        self.assertEqual(old["metrics"]["tool_calls"] - new["metrics"]["tool_calls"], 1)
        report = compare(old, new, gates={"deterministic": "pass"}, target_metrics=["tool_calls"])
        self.assertEqual(report["qualification"], "qualified")
        self.assertEqual(report["improvement"], "improved")
        self.assertTrue(report["comparison"]["tool_calls"]["baseline_refs"])
        self.assertEqual(compare(old, new, gates={"deterministic": "pass"},
                                 target_operations=["read_terminal_result"])["qualification"], "qualified")
        new["snapshot"]["environment_capabilities"]["model"] = "different-model"
        self.assertEqual(compare(old, new, gates={"deterministic": "pass"},
                                 target_metrics=["tool_calls"])["qualification"], "blocked")

    def test_issue_completion_accepts_unchanged_and_blocks_regressions(self):
        data = scenario(SCENARIOS / "delegation-lifecycle.json")
        options = {"session_id": "fixture", "repository_head": "a" * 40,
                   "binary_identity": "b" * 64, "poll_interval": 0}
        for old_strategy, new_strategy, expected in [
            ("reread", "reuse", "improved"),
            ("reuse", "reuse", "unchanged"),
            ("reuse", "reread", "regressed"),
        ]:
            old = execute(data, "baseline", FakeAdapter(),
                          terminal_read_strategy=old_strategy, **options)
            new = execute(data, "candidate", FakeAdapter(),
                          terminal_read_strategy=new_strategy, **options)
            for gate in ("pass", "fail", "blocked", "not_run", None):
                with self.subTest(improvement=expected, gate=gate):
                    gates = {"issue_completion": gate} if gate else {}
                    report = compare(old, new, gates=gates,
                                     target_operations=["read_terminal_result"])
                    self.assertEqual(report["improvement"], expected)
                    accepted = gate == "pass" and expected != "regressed"
                    self.assertEqual(report["qualification"],
                                     "qualified" if accepted else "blocked")
            # Unselected call-count variation is not a measured regression.
            report = compare(old, new, gates={"issue_completion": "pass", "tests": "pass"})
            self.assertEqual(report["qualification"], "qualified")
            self.assertEqual(report["improvement"], "unchanged")

    def test_assertion_regression_blocks_without_improvement_targets(self):
        old = fixture("delegation-lifecycle")
        new = fixture("delegation-lifecycle", "candidate")
        new["assertions"]["bounded_result"] = "fail"
        new["outcome"] = "fail"
        report = compare(old, new, gates={"issue_completion": "pass"})
        self.assertEqual(report["qualification"], "blocked")
        self.assertEqual(report["improvement"], "regressed")
        new = fixture("delegation-lifecycle", "candidate")
        new["snapshot"]["environment_capabilities"]["model"] = "different-model"
        report = compare(old, new, gates={"issue_completion": "pass"})
        self.assertEqual(report["qualification"], "blocked")
        self.assertEqual(report["improvement"], "not_evaluated")

    def test_unchanged_comparison_cli_exits_successfully(self):
        import json
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            save(root / "baseline.json", fixture("delegation-lifecycle"))
            save(root / "candidate.json", fixture("delegation-lifecycle", "candidate"))
            (root / "gates.json").write_text(json.dumps({"issue_completion": "pass", "tests": "pass"}))
            result = subprocess.run(
                [sys.executable, "-m", "dogfood", "compare", str(root / "baseline.json"),
                 str(root / "candidate.json"), "--gates", str(root / "gates.json")],
                cwd=SCENARIOS.parent.parent, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            output = json.loads(result.stdout)
            self.assertEqual(output["qualification"], "qualified")
            self.assertEqual(output["improvement"], "unchanged")

    def test_secret_values_never_enter_observation(self):
        from .protocol import Recorder
        data = scenario(SCENARIOS / "delegation-lifecycle.json")
        recorder = Recorder("run", data, "baseline")
        recorder.call("inspect_session", "session_info", {"session_id": "secret-value"},
                      {"api_key": "secret-value"}, state="secret-value")
        self.assertNotIn("secret-value", str(recorder.events))

    def test_undecidable_baseline_response_is_p2_candidate(self):
        old = fixture("delegation-lifecycle")
        new = fixture("delegation-lifecycle", "candidate")
        old["events"][0]["decision"]["next_action_decidable"] = False
        old["metrics"] = metrics(old["events"])
        report = compare(old, new, gates={"deterministic": "pass"},
                         target_assertions=["bounded_result"])
        self.assertTrue(any(f.get("condition") == "undecidable_response" and
                            f["severity_candidate"] == "P2" for f in report["friction_inventory"]))

    def test_release_qualification_requires_ci_and_action(self):
        data = scenario(SCENARIOS / "release-qualification.json")
        run = execute(data, "candidate", FakeAdapter(), session_id="fixture",
                      repository_head="a" * 40, binary_identity="b" * 64,
                      poll_interval=0, gates={"tests": "pass", "ci": "not_run"})
        self.assertEqual(run["outcome"], "blocked")
        self.assertEqual(run["assertions"]["repository_gates_recorded"], "blocked")


if __name__ == "__main__":
    unittest.main()
