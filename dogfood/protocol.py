"""Small, dependency-free, bounded dogfood data contract and evaluator."""

from __future__ import annotations

import hashlib
import json
import os
import re
from pathlib import Path

OPERATIONS = frozenset({
    "inspect_session", "start_agent", "lose_task_id", "rediscover_task",
    "inject_poll_failure", "start_agent_uncertain", "retry_same_start",
    "wait_until_terminal", "read_terminal_result",
})
ASSERTIONS = frozenset({
    "terminal_state_is_unambiguous", "no_duplicate_task", "bounded_result",
    "tasks_can_be_rediscovered", "retry_is_machine_decidable", "exact_retry",
    "identity_is_separate", "repository_gates_recorded",
})
PHASES = frozenset({"baseline", "candidate"})
OUTCOMES = frozenset({"pass", "fail", "blocked", "not_run"})
TERMINAL = frozenset({"completed", "failed", "interrupted"})
SAFE_STATES = TERMINAL | frozenset({"active", "accepted", "running", "unknown",
                                   "reconciliation_required", "not_modified", "ok",
                                   "error", "unavailable", "blocked", "duplicate_start",
                                   "ambiguous_terminal"})
MAX_ARTIFACT = 1_048_576


def canonical(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(value: object) -> str:
    return hashlib.sha256(canonical(value)).hexdigest()


def scenario(path: Path) -> dict:
    data = json.loads(path.read_text())
    if set(data) != {"id", "revision", "goal", "operations", "assertions"}:
        raise ValueError("scenario fields must match the versioned contract")
    if not isinstance(data["id"], str) or not data["id"] or not isinstance(data["goal"], str):
        raise ValueError("scenario identity and goal are required")
    if type(data["revision"]) is not int or data["revision"] < 1:
        raise ValueError("scenario revision must be positive")
    if not isinstance(data["operations"], list) or not data["operations"]:
        raise ValueError("scenario operations are required")
    if any(op not in OPERATIONS for op in data["operations"]):
        raise ValueError("unknown logical operation")
    if not isinstance(data["assertions"], list) or any(a not in ASSERTIONS for a in data["assertions"]):
        raise ValueError("unknown assertion")
    if "start_agent" not in data["operations"] and "start_agent_uncertain" not in data["operations"]:
        raise ValueError("scenario must exercise a delegated task")
    data["fingerprint"] = digest(data)
    return data


def snapshot(repository_head: str, server_build_identity: str, contract_fingerprint: str,
             capabilities: dict) -> dict:
    if not (isinstance(repository_head, str) and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", repository_head)
            and isinstance(server_build_identity, str) and re.fullmatch(r"[0-9a-f]{64}", server_build_identity)
            and isinstance(contract_fingerprint, str) and re.fullmatch(r"[0-9a-f]{64}", contract_fingerprint)):
        raise ValueError("repository, running binary and contract fingerprints are required")
    return {
        "repository_head": repository_head,
        "server_build_identity": server_build_identity,
        "server_contract_fingerprint": contract_fingerprint,
        "environment_capabilities": capabilities,
    }


class Recorder:
    def __init__(self, run_id: str, scenario_data: dict, phase: str):
        if phase not in PHASES:
            raise ValueError("invalid phase")
        self.run_id, self.scenario, self.phase = run_id, scenario_data, phase
        self.events: list[dict] = []
        self._seen_args: set[str] = set()

    def call(self, operation: str, tool: str, arguments: dict, response: object,
             *, state: str = "unknown", error_code: str | None = None,
             retryable: bool | None = None, next_action: str | None = None,
             duration_ms: int = 0, recovery: bool = False) -> None:
        if operation not in OPERATIONS or not tool or duration_ms < 0:
            raise ValueError("invalid observation identity")
        if state not in SAFE_STATES:
            state = "unknown"
        # Never retain argument values or response bodies: they can include prompts or secrets.
        keys = sorted(arguments)
        event = {
            "ref": f"{self.run_id}:{len(self.events) + 1}",
            "logical_operation": operation,
            "call_index": len(self.events) + 1,
            "tool": tool,
            "request": {
                "explicit_arguments": keys,
                "repeated_arguments": sorted(set(keys) & self._seen_args),
                "opaque_ids": sorted(k for k in keys if k in {"session_id", "task_id", "operation_id", "job_id", "reference"}),
            },
            "response": {
                "state": state[:64], "structured_error": error_code[:64] if error_code else None,
                "retryable": retryable, "suggested_next_action": next_action[:64] if next_action else None,
                "output_bytes": len(canonical(response)),
            },
            "decision": {"next_action_decidable": next_action is not None},
            "recovery": {"attempted": recovery},
            "duration_ms": duration_ms,
        }
        self._seen_args.update(keys)
        self.events.append(event)


def metrics(events: list[dict]) -> dict:
    result = {
        "tool_calls": len(events), "tool_calls_per_operation": {}, "explicit_argument_count": 0,
        "repeated_argument_count": 0, "opaque_id_handoffs": 0, "poll_count": 0,
        "unchanged_poll_count": 0, "rediscovery_calls": 0, "recovery_calls": 0,
        "response_bytes": 0, "ambiguous_terminal_states": 0, "duplicate_start_attempts": 0,
    }
    for event in events:
        op, req, resp = event["logical_operation"], event["request"], event["response"]
        result["tool_calls_per_operation"][op] = result["tool_calls_per_operation"].get(op, 0) + 1
        result["explicit_argument_count"] += len(req["explicit_arguments"])
        result["repeated_argument_count"] += len(req["repeated_arguments"])
        result["opaque_id_handoffs"] += len(req["opaque_ids"])
        result["response_bytes"] += resp["output_bytes"]
        result["poll_count"] += op == "wait_until_terminal"
        result["unchanged_poll_count"] += op == "wait_until_terminal" and resp["state"] == "not_modified"
        result["rediscovery_calls"] += op == "rediscover_task"
        result["recovery_calls"] += bool(event["recovery"]["attempted"])
        result["ambiguous_terminal_states"] += resp["state"] == "ambiguous_terminal"
        result["duplicate_start_attempts"] += resp["state"] == "duplicate_start"
    return result


def validate_run(run: dict) -> None:
    if run.get("schema_version") != 1 or run.get("phase") not in PHASES or run.get("outcome") not in OUTCOMES:
        raise ValueError("invalid run version, phase or outcome")
    if not run.get("run_id") or not run.get("scenario_fingerprint"):
        raise ValueError("run and scenario identity are required")
    if not run.get("scenario_id") or type(run.get("scenario_revision")) is not int or run["scenario_revision"] < 1:
        raise ValueError("scenario identity or revision is invalid")
    assertions = run.get("assertions")
    if not isinstance(assertions, dict) or any(v not in OUTCOMES for v in assertions.values()):
        raise ValueError("invalid assertion results")
    identity = run.get("snapshot", {})
    snapshot(identity.get("repository_head"), identity.get("server_build_identity"),
             identity.get("server_contract_fingerprint"), identity.get("environment_capabilities", {}))
    events = run.get("events")
    if not isinstance(events, list) or len(events) > 10_000:
        raise ValueError("events missing or unbounded")
    if any(e.get("ref") != f"{run['run_id']}:{i}" or e.get("call_index") != i
           for i, e in enumerate(events, 1)):
        raise ValueError("event references are not contiguous")
    if run.get("metrics") != metrics(events):
        raise ValueError("metrics do not match source observations")
    if len(canonical(run)) > MAX_ARTIFACT:
        raise ValueError("run exceeds bounded artifact size")


def save(path: Path, value: dict) -> None:
    validate_run(value)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.exists():
        raise FileExistsError(path)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(canonical(value) + b"\n")


def load(path: Path) -> dict:
    if path.stat().st_size > MAX_ARTIFACT + 1:
        raise ValueError("run exceeds bounded artifact size")
    run = json.loads(path.read_text())
    validate_run(run)
    return run


def compare(baseline: dict, candidate: dict, *, gates: dict[str, str] | None = None,
            target_metrics: list[str] | None = None,
            target_operations: list[str] | None = None,
            target_assertions: list[str] | None = None) -> dict:
    validate_run(baseline)
    validate_run(candidate)
    if baseline["phase"] != "baseline" or candidate["phase"] != "candidate":
        raise ValueError("comparison requires baseline and candidate in that order")
    if baseline["run_id"] == candidate["run_id"]:
        raise ValueError("comparison requires distinct runs")
    if baseline["scenario_fingerprint"] != candidate["scenario_fingerprint"]:
        raise ValueError("scenario revisions differ")
    gate_results = gates or {}
    if any(value not in OUTCOMES for value in gate_results.values()):
        raise ValueError("invalid gate result")
    target_metrics = target_metrics or []
    target_operations = target_operations or []
    target_assertions = target_assertions or []
    if any(key not in baseline["metrics"] or key == "tool_calls_per_operation" for key in target_metrics):
        raise ValueError("unknown scalar target metric")
    if any(key not in baseline["assertions"] or key not in candidate["assertions"] for key in target_assertions):
        raise ValueError("unknown target assertion")
    if any(key not in OPERATIONS for key in target_operations):
        raise ValueError("unknown target operation")
    targets_met = bool(target_metrics or target_operations or target_assertions)
    targets_met &= all(candidate["metrics"][key] < baseline["metrics"][key] for key in target_metrics)
    targets_met &= all(candidate["metrics"]["tool_calls_per_operation"].get(key, 0)
                       < baseline["metrics"]["tool_calls_per_operation"].get(key, 0)
                       for key in target_operations)
    targets_met &= all(baseline["assertions"][key] != "pass" and candidate["assertions"][key] == "pass"
                       for key in target_assertions)
    no_assertion_regression = all(value != "pass" or candidate["assertions"].get(key) == "pass"
                                  for key, value in baseline["assertions"].items())
    selectors = ("backend", "model", "effort")
    baseline_profile = baseline["snapshot"]["environment_capabilities"]
    candidate_profile = candidate["snapshot"]["environment_capabilities"]
    environment_comparable = all(baseline_profile.get(key) == candidate_profile.get(key)
                                 for key in selectors)
    safe = (candidate["outcome"] == "pass" and no_assertion_regression and targets_met
            and environment_comparable
            and bool(gate_results) and all(v == "pass" for v in gate_results.values())
            and all(v == "pass" for v in candidate["assertions"].values()))
    measures = {}
    for key, value in baseline["metrics"].items():
        if key == "tool_calls_per_operation":
            continue
        measures[key] = {"baseline": value, "candidate": candidate["metrics"][key],
                         "baseline_refs": [e["ref"] for e in baseline["events"]],
                         "candidate_refs": [e["ref"] for e in candidate["events"]]}
    measures["tool_calls_per_operation"] = {}
    for operation in sorted(set(baseline["metrics"]["tool_calls_per_operation"])
                            | set(candidate["metrics"]["tool_calls_per_operation"])):
        measures["tool_calls_per_operation"][operation] = {
            "baseline": baseline["metrics"]["tool_calls_per_operation"].get(operation, 0),
            "candidate": candidate["metrics"]["tool_calls_per_operation"].get(operation, 0),
            "baseline_refs": [e["ref"] for e in baseline["events"] if e["logical_operation"] == operation],
            "candidate_refs": [e["ref"] for e in candidate["events"] if e["logical_operation"] == operation],
        }
    frictions = []
    for key in ("duplicate_start_attempts", "ambiguous_terminal_states", "recovery_calls",
                "unchanged_poll_count", "opaque_id_handoffs", "tool_calls"):
        old, new = baseline["metrics"][key], candidate["metrics"][key]
        if old > new:
            frictions.append({"metric": key, "baseline": old, "candidate": new,
                              "severity_candidate": "P0" if key in {"duplicate_start_attempts", "ambiguous_terminal_states"} else "P1",
                              "event_refs": measures[key]["baseline_refs"]})
    for key in target_operations:
        old = baseline["metrics"]["tool_calls_per_operation"].get(key, 0)
        new = candidate["metrics"]["tool_calls_per_operation"].get(key, 0)
        if old > new:
            frictions.append({"operation": key, "baseline": old, "candidate": new,
                              "severity_candidate": "P1",
                              "event_refs": [e["ref"] for e in baseline["events"]
                                             if e["logical_operation"] == key]})
    for key in target_assertions:
        if baseline["assertions"][key] != "pass" and candidate["assertions"][key] == "pass":
            frictions.append({"assertion": key, "baseline": baseline["assertions"][key],
                              "candidate": "pass", "severity_candidate": "P0",
                              "event_refs": [e["ref"] for e in baseline["events"]]})
    for name, predicate in (
        ("undecidable_response", lambda e: not e["decision"]["next_action_decidable"]),
        ("unstructured_error", lambda e: e["response"]["state"] == "error"
         and e["response"]["structured_error"] is None),
    ):
        old_refs = [e["ref"] for e in baseline["events"] if predicate(e)]
        new_refs = [e["ref"] for e in candidate["events"] if predicate(e)]
        if len(old_refs) > len(new_refs):
            frictions.append({"condition": name, "baseline": len(old_refs),
                              "candidate": len(new_refs), "severity_candidate": "P2",
                              "event_refs": old_refs})
    return {
        "schema_version": 1, "baseline_run_id": baseline["run_id"], "candidate_run_id": candidate["run_id"],
        "scenario_fingerprint": baseline["scenario_fingerprint"],
        "comparison": measures, "friction_inventory": frictions,
        "target_metrics": target_metrics, "target_operations": target_operations,
        "target_assertions": target_assertions,
        "environment_comparable": environment_comparable,
        "regression_gates": gate_results, "qualification": "qualified" if safe else "blocked",
        "reason": "assertions and gates passed" if safe else "missing, failed or incomparable acceptance evidence",
    }
