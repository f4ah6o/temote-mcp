"""Dogfood contract and bounded live driver for evidence-backed memory continuity.

Run artifacts retain action names, statuses, bounded sizes and assertion outcomes;
task instructions, model replies, access credentials and evidence bodies stay in
process memory and are never written to disk.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
SCENARIO_PATH = ROOT / "dogfood" / "scenarios" / "memory-continuity.json"
VALID_GATE_STATES = {"pass", "fail", "blocked", "not_run", "not_implemented"}
TERMINAL = {"completed", "failed", "interrupted"}
MAX_ARTIFACT_BYTES = 1_048_576
UNRELATED_REPOSITORY = "github:temote-tests/memory-continuity-unrelated"
OFFLINE_PROOF_MARKER = b"temote-memory-host-offline-v1\n"
TASK_A_COMPLETE_MARKER = b"temote-memory-task-a-complete-v1\n"
TASK_B_READY_MARKER = b"temote-memory-task-b-ready-v1\n"
TASK_B_COMPLETE_MARKER = b"temote-memory-task-b-complete-v1\n"
REPLAY_MANIFEST_FIELDS = {
    "schema_version", "repository_key",
    "dispatch_count_before", "dispatch_count_after",
    "knowledge_count_before", "knowledge_count_after",
    "support_count_before", "support_count_after",
    "supersession_count_before", "supersession_count_after",
    "run_count_before", "run_count_after",
    "checkpoint_before", "checkpoint_after",
}


def _canonical(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def load_scenario(path: Path = SCENARIO_PATH) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding="utf-8"))
    expected = {
        "schema_version", "id", "revision", "goal", "repository_key",
        "task_a", "task_b", "limits", "assertions",
    }
    if set(data) != expected:
        raise ValueError("memory-continuity scenario fields do not match the contract")
    if data["schema_version"] != 1 or data["id"] != "memory-continuity":
        raise ValueError("unsupported memory-continuity scenario identity")
    if type(data["revision"]) is not int or data["revision"] < 1:
        raise ValueError("scenario revision must be a positive integer")
    if not re.fullmatch(r"github:[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", data["repository_key"]):
        raise ValueError("scenario must use a stable repository key")
    if not isinstance(data["goal"], str) or not data["goal"]:
        raise ValueError("scenario goal is required")
    if set(data["task_a"]) != {"instruction", "constraint", "unresolved"}:
        raise ValueError("task A contract is invalid")
    if set(data["task_b"]) != {"instruction", "old_constraint", "constraint", "unresolved"}:
        raise ValueError("task B contract is invalid")
    for task_name in ("task_a", "task_b"):
        task = data[task_name]
        for key, value in task.items():
            if not isinstance(value, str) or not value.strip() or len(value) > 2048:
                raise ValueError(f"{task_name}.{key} must be a bounded string")
        if re.search(r"\b(memory|remember|summari[sz]e)\b", task["instruction"], re.IGNORECASE):
            raise ValueError("normal task instructions must not request memory or summaries")
    if data["task_a"]["constraint"] != data["task_b"]["old_constraint"]:
        raise ValueError("task B must explicitly change task A's constraint")
    if data["task_b"]["constraint"] == data["task_b"]["old_constraint"]:
        raise ValueError("task B must introduce a changed constraint")
    if data["task_a"]["unresolved"] != data["task_b"]["unresolved"]:
        raise ValueError("the unresolved item must remain stable across the tasks")
    expected_assertions = {
        "task_a_constraint_supported", "task_a_unresolved_surfaced",
        "cross_head_repository_context", "bounded_context_response", "offline_host_context",
        "task_b_constraint_current", "task_a_constraint_superseded",
        "queue_replay_no_growth", "unrelated_repository_isolated",
    }
    if set(data["assertions"]) != expected_assertions:
        raise ValueError("memory-continuity assertions do not match the acceptance contract")
    limits = data["limits"]
    if set(limits) != {"context_bytes", "knowledge_items", "task_polls", "context_wait_seconds"}:
        raise ValueError("scenario limits are invalid")
    bounds = {
        "context_bytes": (256, 65536), "knowledge_items": (1, 64),
        "task_polls": (1, 200), "context_wait_seconds": (1, 600),
    }
    for key, (minimum, maximum) in bounds.items():
        if type(limits[key]) is not int or not minimum <= limits[key] <= maximum:
            raise ValueError(f"scenario limit {key} is outside its bound")
    data["fingerprint"] = hashlib.sha256(_canonical(data)).hexdigest()
    return data


def validate_artifact(value: dict[str, Any]) -> None:
    required = {
        "schema_version", "run_id", "scenario_id", "scenario_revision",
        "scenario_fingerprint", "phase", "mode", "synthesis_mode", "outcome", "selectors",
        "provenance", "gates", "events",
    }
    if set(value) != required or value["schema_version"] != 1:
        raise ValueError("invalid memory-continuity run artifact")
    if value["scenario_id"] != "memory-continuity" or type(value["scenario_revision"]) is not int:
        raise ValueError("invalid run scenario identity")
    if (value["phase"] not in {"baseline", "candidate"}
            or value["mode"] not in {"fixture", "live"}
            or value["synthesis_mode"] not in {"fixture", "live", "not_run"}):
        raise ValueError("invalid run phase or mode")
    if value["outcome"] not in VALID_GATE_STATES:
        raise ValueError("invalid run outcome")
    if not isinstance(value["scenario_fingerprint"], str) or not re.fullmatch(r"[0-9a-f]{64}", value["scenario_fingerprint"]):
        raise ValueError("invalid scenario fingerprint")
    if (not isinstance(value["selectors"], dict)
            or set(value["selectors"]) != {"backend", "model", "effort", "extractor_profile"}
            or any(item is not None and (not isinstance(item, str) or len(item) > 256)
                   for item in value["selectors"].values())):
        raise ValueError("run selector identity is invalid")
    if any(item is not None and not re.fullmatch(r"[A-Za-z0-9_.:/+-]{1,256}", item)
           for item in value["selectors"].values()):
        raise ValueError("run selector contains unsupported characters")
    provenance = value["provenance"]
    provenance_fields = {"source_head", "working_diff_sha256", "binary_sha256", "input_sha256"}
    if not isinstance(provenance, dict) or set(provenance) != provenance_fields:
        raise ValueError("run provenance is invalid")
    if not isinstance(provenance["source_head"], str) or not re.fullmatch(
        r"[0-9a-f]{40}|[0-9a-f]{64}", provenance["source_head"]
    ):
        raise ValueError("source checkout identity is missing")
    for field in ("working_diff_sha256", "input_sha256"):
        if not isinstance(provenance[field], str) or not re.fullmatch(r"[0-9a-f]{64}", provenance[field]):
            raise ValueError(f"{field} must be a SHA-256 digest")
    if provenance["binary_sha256"] is not None and (
        not isinstance(provenance["binary_sha256"], str)
        or not re.fullmatch(r"[0-9a-f]{64}", provenance["binary_sha256"])
    ):
        raise ValueError("binary identity must be a SHA-256 digest")
    if set(value["gates"]) != set(load_scenario()["assertions"]):
        raise ValueError("run artifact must record every independent assertion gate")
    if any(state not in VALID_GATE_STATES for state in value["gates"].values()):
        raise ValueError("invalid assertion gate state")
    if not isinstance(value["events"], list) or len(value["events"]) > 10000:
        raise ValueError("run events are missing or unbounded")
    for index, event in enumerate(value["events"], 1):
        if set(event) != {"ref", "action", "state", "error_code", "input_keys", "output_bytes", "duration_ms"}:
            raise ValueError("run event contains unexpected fields")
        if (event["ref"] != f"{value['run_id']}:{index}"
                or not isinstance(event["action"], str)
                or not re.fullmatch(r"[a-z][a-z0-9_./-]{0,63}", event["action"])):
            raise ValueError("run event reference is invalid")
        if event["state"] not in {"ok", "accepted", "running", *TERMINAL, "error", "not_run", "unknown"}:
            raise ValueError("run event state is invalid")
        if event["error_code"] is not None and not re.fullmatch(r"[A-Z][A-Z0-9_]{1,63}", event["error_code"]):
            raise ValueError("run error code is invalid")
        if (not isinstance(event["input_keys"], list)
                or any(not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", key)
                       for key in event["input_keys"])):
            raise ValueError("run event argument keys are invalid")
        if any(type(event[key]) is not int or event[key] < 0 for key in ("output_bytes", "duration_ms")):
            raise ValueError("run event bounds are invalid")
    if len(_canonical(value)) > MAX_ARTIFACT_BYTES:
        raise ValueError("run artifact exceeds the size bound")


def _save_artifact(path: Path, value: dict[str, Any]) -> None:
    validate_artifact(value)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(_canonical(value) + b"\n")


def _path_under_runs(path: Path) -> Path:
    if path.is_symlink():
        raise ValueError("proof artifact must not be a symlink")
    resolved = path.resolve(strict=True)
    try:
        resolved.relative_to((ROOT / "dogfood" / "runs").resolve())
    except ValueError as error:
        raise ValueError("proof artifacts must be under ignored dogfood/runs") from error
    if resolved.is_symlink() or not resolved.is_file():
        raise ValueError("proof artifact must be a regular file")
    return resolved


def validate_offline_proof(path: Path) -> bool:
    resolved = _path_under_runs(path)
    if resolved.stat().st_size != len(OFFLINE_PROOF_MARKER):
        return False
    return resolved.read_bytes() == OFFLINE_PROOF_MARKER


def _validate_marker(path: Path, marker: bytes) -> bool:
    resolved = _path_under_runs(path)
    return resolved.stat().st_size == len(marker) and resolved.read_bytes() == marker


def write_task_a_complete_marker(path: Path | None) -> None:
    if path is None:
        return
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    resolved_parent = path.parent.resolve(strict=True)
    try:
        resolved_parent.relative_to((ROOT / "dogfood" / "runs").resolve())
    except ValueError as error:
        raise ValueError("task completion marker must be under ignored dogfood/runs") from error
    if path.is_symlink():
        raise ValueError("task completion marker must not be a symlink")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                         | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(TASK_A_COMPLETE_MARKER)
        stream.flush()
        os.fsync(stream.fileno())


def write_task_b_complete_marker(path: Path | None) -> None:
    if path is None:
        return
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    resolved_parent = path.parent.resolve(strict=True)
    try:
        resolved_parent.relative_to((ROOT / "dogfood" / "runs").resolve())
    except ValueError as error:
        raise ValueError("task completion marker must be under ignored dogfood/runs") from error
    if path.is_symlink():
        raise ValueError("task completion marker must not be a symlink")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                         | getattr(os, "O_NOFOLLOW", 0), 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(TASK_B_COMPLETE_MARKER)
        stream.flush()
        os.fsync(stream.fileno())


def validate_queue_replay_manifest(path: Path, repository_key: str) -> bool:
    resolved = _path_under_runs(path)
    if resolved.stat().st_size > 4096:
        return False
    try:
        value = json.loads(resolved.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return False
    if not isinstance(value, dict) or set(value) != REPLAY_MANIFEST_FIELDS:
        return False
    if (type(value.get("schema_version")) is not int or value["schema_version"] != 1
            or value.get("repository_key") != repository_key):
        return False
    numeric = REPLAY_MANIFEST_FIELDS - {"schema_version", "repository_key"}
    if any(type(value.get(field)) is not int or value[field] < 0 for field in numeric):
        return False
    if value["dispatch_count_after"] <= value["dispatch_count_before"]:
        return False
    stable_counts = (
        "knowledge_count", "support_count", "supersession_count", "run_count", "checkpoint",
    )
    return all(value[name + "_before"] == value[name + "_after"] for name in stable_counts)


def _event(run_id: str, events: list[dict[str, Any]], action: str, input_keys: list[str],
           state: str, output_bytes: int, started: float, error_code: str | None = None) -> None:
    events.append({
        "ref": f"{run_id}:{len(events) + 1}", "action": action,
        "state": state if state in {"ok", "accepted", "running", *TERMINAL, "error", "not_run"} else "unknown",
        "error_code": error_code, "input_keys": sorted(input_keys),
        "output_bytes": min(max(int(output_bytes), 0), MAX_ARTIFACT_BYTES),
        "duration_ms": max(0, int((time.monotonic() - started) * 1000)),
    })


def _source_identity(binary: Path | None, scenario_data: dict[str, Any], *,
                     source_head: str | None = None,
                     working_diff_sha256: str | None = None) -> dict[str, Any]:
    if (source_head is None) != (working_diff_sha256 is None):
        raise ValueError("source head and working diff overrides must be supplied together")
    if source_head is None:
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
                              capture_output=True, check=True).stdout.strip()
        diff = subprocess.run(["git", "diff", "--binary", "HEAD"], cwd=ROOT,
                              capture_output=True, check=True).stdout
        untracked = subprocess.run(["git", "ls-files", "--others", "--exclude-standard", "-z"],
                                   cwd=ROOT, capture_output=True, check=True).stdout
        digest = hashlib.sha256()
        digest.update(diff)
        for name in sorted(filter(None, untracked.split(b"\0"))):
            digest.update(len(name).to_bytes(4, "big"))
            digest.update(name)
            path = ROOT / os.fsdecode(name)
            try:
                body = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
            except OSError:
                body = b"<unavailable>"
            digest.update(len(body).to_bytes(8, "big"))
            digest.update(body)
        source_head = head
        working_diff_sha256 = digest.hexdigest()
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", source_head):
        raise ValueError("source head override must be a full Git object id")
    if not re.fullmatch(r"[0-9a-f]{64}", working_diff_sha256):
        raise ValueError("working diff override must be a SHA-256 digest")
    binary_digest = hashlib.sha256(binary.read_bytes()).hexdigest() if binary and binary.is_file() else None
    inputs = {
        "repository_key": scenario_data["repository_key"],
        "task_a": scenario_data["task_a"]["instruction"],
        "task_b": scenario_data["task_b"]["instruction"],
        "limits": scenario_data["limits"],
    }
    return {
        "source_head": source_head,
        "working_diff_sha256": working_diff_sha256,
        "binary_sha256": binary_digest,
        "input_sha256": hashlib.sha256(_canonical(inputs)).hexdigest(),
    }


def fixture_run(phase: str, scenario_data: dict[str, Any] | None = None,
                *, binary: Path | None = None, source_head: str | None = None,
                working_diff_sha256: str | None = None) -> dict[str, Any]:
    """Validate the harness in fixture mode without claiming pipeline success."""
    data = scenario_data or load_scenario()
    if phase not in {"baseline", "candidate"}:
        raise ValueError("phase must be baseline or candidate")
    gates = {assertion: "not_run" for assertion in data["assertions"]}
    run = {
        "schema_version": 1, "run_id": str(uuid.uuid4()),
        "scenario_id": data["id"], "scenario_revision": data["revision"],
        "scenario_fingerprint": data["fingerprint"], "phase": phase,
        "mode": "fixture", "synthesis_mode": "not_run", "outcome": "not_run",
        "selectors": {"backend": None, "model": None, "effort": None,
                      "extractor_profile": None},
        "provenance": _source_identity(binary, data, source_head=source_head,
                                        working_diff_sha256=working_diff_sha256),
        "gates": gates, "events": [],
    }
    validate_artifact(run)
    return run


class McpHttpClient:
    """Small stateless MCP client for a configured Fabric endpoint."""

    def __init__(self, endpoint: str, token: str, *, timeout: float = 45.0):
        parsed = urllib.parse.urlparse(endpoint)
        if parsed.scheme != "https" and not (parsed.scheme == "http" and parsed.hostname == "127.0.0.1"):
            raise ValueError("Fabric endpoint must use HTTPS, except loopback test endpoints")
        if not token or "\r" in token or "\n" in token or len(token) > 8192:
            raise ValueError("Fabric token is invalid")
        self.endpoint = endpoint.rstrip("/")
        if not self.endpoint.endswith("/mcp"):
            self.endpoint += "/mcp"
        self.token = token
        self.timeout = timeout
        self.request_id = 0

    def call(self, name: str, arguments: dict[str, Any]) -> Any:
        self.request_id += 1
        request_body = _canonical({
            "jsonrpc": "2.0", "id": self.request_id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        request = urllib.request.Request(
            self.endpoint, data=request_body, method="POST",
            headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                if response.status != 200:
                    raise RuntimeError("FABRIC_HTTP_ERROR")
                raw = response.read(MAX_ARTIFACT_BYTES + 1)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"FABRIC_HTTP_{error.code}") from None
        except (TimeoutError, urllib.error.URLError):
            raise RuntimeError("FABRIC_TRANSPORT_ERROR") from None
        if len(raw) > MAX_ARTIFACT_BYTES:
            raise RuntimeError("FABRIC_RESPONSE_TOO_LARGE")
        try:
            envelope = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            raise RuntimeError("FABRIC_RESPONSE_INVALID") from None
        if "error" in envelope:
            message = envelope["error"].get("message", "")
            match = re.match(r"^([A-Z][A-Z0-9_]{1,63}):", message)
            if match:
                raise RuntimeError(match.group(1)) from None
            normalized = str(message).casefold()
            if name in {"context_status", "context_resolve"} and (
                    "missing or invalid params.arguments.session_id" in normalized):
                raise RuntimeError("SESSION_REQUIRED") from None
            if name in {"context_status", "context_resolve"} and any(
                    marker in normalized for marker in ("unknown tool", "method not found", "tool not found")):
                raise RuntimeError("CONTEXT_NOT_IMPLEMENTED") from None
            raise RuntimeError("FABRIC_MCP_ERROR") from None
        result = envelope.get("result")
        if not isinstance(result, dict):
            raise RuntimeError("FABRIC_RESULT_INVALID")
        content = result.get("content")
        if isinstance(content, list) and content and isinstance(content[0], dict):
            text = content[0].get("text")
            if isinstance(text, str):
                try:
                    return json.loads(text)
                except ValueError:
                    return text
        if "structuredContent" in result:
            return result["structuredContent"]
        return result


def _walk(value: Any, depth: int = 0):
    if depth > 12:
        return
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from _walk(child, depth + 1)
    elif isinstance(value, list):
        for child in value:
            yield from _walk(child, depth + 1)


def _strings(value: Any) -> list[str]:
    return [item for node in _walk(value) for item in node.values() if isinstance(item, str)]


KNOWLEDGE_GROUPS = {
    "relevant_facts": "fact", "relevant_decisions": "decision",
    "constraints": "constraint", "unresolved": "unresolved",
    "known_failure_patterns": "failure_pattern",
}


def _knowledge_items(context: Any) -> list[dict[str, Any]]:
    items = []
    for node in _walk(context):
        for group, kind in KNOWLEDGE_GROUPS.items():
            values = node.get(group)
            if isinstance(values, list):
                items.extend({**item, "kind": item.get("kind", kind)} for item in values
                             if isinstance(item, dict) and isinstance(item.get("text"), str))
        if isinstance(node.get("text"), str) and isinstance(node.get("kind"), str):
            items.append(node)
    return items


def _current_items(context: Any) -> list[dict[str, Any]]:
    current = {"current"}
    return [item for item in _knowledge_items(context) if item.get("status") in current]


def _supported_item(context: Any, expected: str, repository_key: str,
                    kind: str | None = None, source_task_id: str | None = None) -> dict[str, Any] | None:
    accepted_statuses = {"supported", "current"} if kind == "unresolved" else {"current"}
    for item in _knowledge_items(context):
        if item.get("status") not in accepted_statuses:
            continue
        if kind is not None and item.get("kind") != kind:
            continue
        refs = item.get("support_refs")
        if (expected.casefold() in item["text"].casefold()
                and item.get("scope_type") == "repository"
                and item.get("scope_id") == repository_key
                and item.get("authority") == "derived"
                and isinstance(refs, list) and refs
                and all(isinstance(ref, dict) and ref.get("observation_kind") == "instruction"
                        and isinstance(ref.get("observation_id"), str)
                        and type(ref.get("cloud_seq")) is int
                        and (source_task_id is None or ref.get("task_id") == source_task_id)
                        for ref in refs)):
            return item
    return None


def _summary_matches_supported_policy(context: Any, expected: str,
                                      source_task_id: str) -> bool:
    """Require the current-summary projection to match an instruction-backed policy.

    The summary is a separate public field from the typed constraint list. Keep
    its provenance check independent so a correct constraint cannot mask a
    stale or missing summary, and do not infer a status from the summary row.
    """
    for node in _walk(context):
        summary = node.get("current_summary")
        if not isinstance(summary, dict) or summary.get("knowledge_summary") != expected:
            continue
        refs = summary.get("knowledge_summary_refs")
        if not isinstance(refs, list):
            continue
        if any(
            isinstance(ref, dict)
            and ref.get("role") in {"direct_quote", "summary_quote"}
            and ref.get("observation_kind") == "instruction"
            and ref.get("task_id") == source_task_id
            and isinstance(ref.get("observation_id"), str)
            and ref["observation_id"]
            and type(ref.get("cloud_seq")) is int
            for ref in refs
        ):
            return True
    return False


def _memory_status(context: Any) -> dict[str, Any] | None:
    for node in _walk(context):
        value = node.get("memory")
        if isinstance(value, dict) and isinstance(value.get("state"), str):
            return value
    return None


def _wait_for_file(path: Path | None, validator: Any, timeout_seconds: int,
                   sleep: Any) -> bool:
    if path is None:
        return False
    deadline = time.monotonic() + timeout_seconds
    while True:
        try:
            if validator(path):
                return True
        except (OSError, ValueError, json.JSONDecodeError):
            pass
        if time.monotonic() >= deadline:
            return False
        sleep(min(0.25, max(deadline - time.monotonic(), 0)))


def _memory_projection_empty(context: Any) -> bool:
    return not _knowledge_items(context)


def _baseline_not_implemented(status: Any, context: Any) -> bool:
    memory = _memory_status(status)
    state = memory.get("state") if memory else None
    return (state in {"disabled", "not_configured", "not_implemented"}
            and (state == "not_implemented" or _memory_projection_empty(context)))


def _run_task(client: McpHttpClient, run_id: str, events: list[dict[str, Any]], *,
              scenario_data: dict[str, Any], task_key: str, session_id: str,
              host_id: str | None, backend: str, model: str | None,
              effort: str | None, max_polls: int, poll_interval: float) -> dict[str, Any]:
    tool = f"{backend}_task_start"
    args: dict[str, Any] = {
        "session_id": session_id, "operation_id": str(uuid.uuid4()),
        "task": scenario_data[task_key]["instruction"],
    }
    if host_id:
        args["host_id"] = host_id
    if model:
        args["model"] = model
    if effort:
        args["effort"] = effort
    started = time.monotonic()
    try:
        accepted = client.call(tool, args)
    except RuntimeError as error:
        _event(run_id, events, tool, list(args), "error", len(str(error)), started, str(error))
        raise
    _event(run_id, events, tool, list(args), "accepted", len(_canonical(accepted)), started)
    task_id = accepted.get("task_id") if isinstance(accepted, dict) else None
    if not isinstance(task_id, str) or not task_id:
        raise RuntimeError("TASK_ID_MISSING")
    get_tool = f"{backend}_task_get"
    last: dict[str, Any] = {}
    for index in range(max_polls):
        poll_args = {"session_id": session_id, "task_id": task_id}
        if host_id:
            poll_args["host_id"] = host_id
        started = time.monotonic()
        try:
            last = client.call(get_tool, poll_args)
        except RuntimeError as error:
            _event(run_id, events, get_tool, list(poll_args), "error", len(str(error)), started, str(error))
            raise
        state = last.get("status", "unknown") if isinstance(last, dict) else "unknown"
        _event(run_id, events, get_tool, list(poll_args), state, len(_canonical(last)), started)
        if state in TERMINAL:
            if state != "completed":
                raise RuntimeError("TASK_NOT_COMPLETED")
            return {"status": last, "task_id": task_id}
        if index + 1 < max_polls:
            time.sleep(poll_interval)
    raise RuntimeError("POLL_LIMIT_REACHED")


def live_run(*, phase: str, endpoint: str, token: str, session_a: str,
             session_b: str, host_a: str | None = None, host_b: str | None = None,
             backend: str = "opencode", model: str | None = None,
             effort: str | None = None, extractor_profile: str | None = None,
             synthesis_mode: str = "fixture", binary: Path | None = None,
             offline_proof_file: Path | None = None,
             offline_proof_wait_seconds: int = 180,
             task_a_complete_file: Path | None = None,
             task_b_ready_file: Path | None = None,
             task_b_complete_file: Path | None = None,
             task_b_ready_wait_seconds: int = 180,
             queue_replay_manifest: Path | None = None,
             queue_replay_wait_seconds: int = 180,
             source_head: str | None = None,
             working_diff_sha256: str | None = None,
             max_polls: int = 120,
             poll_interval: float = 1.0, scenario_data: dict[str, Any] | None = None,
             sleep: Any = time.sleep) -> dict[str, Any]:
    """Run ordinary Temote tasks and assert their cloud-derived continuity.

    The caller owns disposable hosts/sessions and writes bounded offline/replay
    attestations after independently observing those conditions.
    """
    data = scenario_data or load_scenario()
    if phase not in {"baseline", "candidate"}:
        raise ValueError("phase must be baseline or candidate")
    if synthesis_mode not in {"fixture", "live", "not_run"}:
        raise ValueError("synthesis_mode must be fixture, live, or not_run")
    if phase == "candidate" and synthesis_mode == "not_run":
        raise ValueError("candidate runs require fixture or live synthesis")
    if (not session_a or not session_b or max_polls < 1 or max_polls > 200
            or poll_interval < 0 or not 1 <= offline_proof_wait_seconds <= 600
            or not 1 <= task_b_ready_wait_seconds <= 600
            or not 1 <= queue_replay_wait_seconds <= 600):
        raise ValueError("live run requires two sessions and bounded polling")
    run_id = str(uuid.uuid4())
    events: list[dict[str, Any]] = []
    gates = {assertion: "not_run" for assertion in data["assertions"]}
    run: dict[str, Any] = {
        "schema_version": 1, "run_id": run_id, "scenario_id": data["id"],
        "scenario_revision": data["revision"], "scenario_fingerprint": data["fingerprint"],
        "phase": phase, "mode": "live", "synthesis_mode": synthesis_mode, "outcome": "blocked",
        "selectors": {"backend": backend, "model": model, "effort": effort,
                      "extractor_profile": extractor_profile},
        "provenance": _source_identity(binary, data, source_head=source_head,
                                        working_diff_sha256=working_diff_sha256),
        "gates": gates,
        "events": events,
    }
    client = McpHttpClient(endpoint, token)
    reader = McpHttpClient(endpoint, token)
    def context_call(tool: str, args: dict[str, Any]) -> Any:
        started = time.monotonic()
        try:
            result = reader.call(tool, args)
        except RuntimeError as error:
            _event(run_id, events, tool, list(args), "error", len(str(error)), started, str(error))
            raise
        _event(run_id, events, tool, list(args), "ok", len(_canonical(result)), started)
        return result

    def resolve(query: str | None = None) -> Any:
        args: dict[str, Any] = {
            "repository": data["repository_key"],
            "limit": data["limits"]["knowledge_items"],
        }
        if query is not None:
            args["query"] = query
        return context_call("context_resolve", args)

    def context_status() -> Any:
        return context_call("context_status", {"repository": data["repository_key"]})

    def await_task_b_ready() -> bool:
        if task_b_ready_file is None:
            return True
        started = time.monotonic()
        ready = _wait_for_file(
            task_b_ready_file,
            lambda path: _validate_marker(path, TASK_B_READY_MARKER),
            task_b_ready_wait_seconds, sleep,
        )
        _event(run_id, events, "task_b_workspace_ready", [],
               "ok" if ready else "not_run", 0, started,
               None if ready else "TASK_B_READY_NOT_OBSERVED")
        return ready

    def run_task_b() -> dict[str, Any]:
        result = _run_task(client, run_id, events, scenario_data=data, task_key="task_b",
                           session_id=session_b, host_id=host_b, backend=backend, model=model,
                           effort=effort, max_polls=max_polls, poll_interval=poll_interval)
        # _run_task returns only after it observes a completed terminal state.
        # The Node harness may now wait for this host's terminal observation
        # and projection; acceptance/running never produce this marker.
        write_task_b_complete_marker(task_b_complete_file)
        return result

    try:
        first = _run_task(client, run_id, events, scenario_data=data, task_key="task_a",
                          session_id=session_a, host_id=host_a, backend=backend, model=model,
                          effort=effort, max_polls=max_polls, poll_interval=poll_interval)
        write_task_a_complete_marker(task_a_complete_file)
        try:
            status = context_status()
        except RuntimeError as error:
            # The archived baseline only offers session-bound context. That is
            # a real, explicit absence of repository-scoped cloud context; do
            # not supply a session as a fallback and mistake it for C3.
            if phase != "baseline" or str(error) not in {"CONTEXT_NOT_IMPLEMENTED", "SESSION_REQUIRED"}:
                raise
            status = {"memory": {"state": "not_implemented"}}
        if phase == "baseline":
            baseline_memory = _memory_status(status)
            baseline_state = baseline_memory.get("state") if baseline_memory else None
            if baseline_state == "not_implemented":
                for gate in (
                    "task_a_constraint_supported", "task_a_unresolved_surfaced",
                    "cross_head_repository_context", "bounded_context_response",
                    "offline_host_context", "task_b_constraint_current",
                    "task_a_constraint_superseded", "unrelated_repository_isolated",
                ):
                    gates[gate] = "not_implemented"
                gates["queue_replay_no_growth"] = "not_run"
                if not await_task_b_ready():
                    run["outcome"] = "blocked"
                    validate_artifact(run)
                    return run
                run_task_b()
                run["outcome"] = "not_implemented"
                validate_artifact(run)
                return run
            try:
                baseline_context = resolve("report")
            except RuntimeError as error:
                if str(error) not in {"CONTEXT_NOT_IMPLEMENTED", "SESSION_REQUIRED"}:
                    raise
                baseline_context = {"memory": {"state": "not_implemented"}}
                status = baseline_context
            if _baseline_not_implemented(status, baseline_context):
                _event(run_id, events, "memory_projection", ["repository"], "not_run",
                       len(_canonical(baseline_context)), time.monotonic(), "NOT_IMPLEMENTED")
                for gate in (
                    "task_a_constraint_supported", "task_a_unresolved_surfaced",
                    "cross_head_repository_context", "bounded_context_response",
                    "offline_host_context", "task_b_constraint_current",
                    "task_a_constraint_superseded", "unrelated_repository_isolated",
                ):
                    gates[gate] = "not_implemented"
                gates["queue_replay_no_growth"] = "not_run"
                if not await_task_b_ready():
                    run["outcome"] = "blocked"
                    validate_artifact(run)
                    return run
                run_task_b()
                run["outcome"] = "not_implemented"
                validate_artifact(run)
                return run
            if baseline_state not in {"disabled", "not_configured"}:
                for gate in (
                    "task_a_constraint_supported", "task_a_unresolved_surfaced",
                    "cross_head_repository_context", "bounded_context_response",
                    "offline_host_context", "task_b_constraint_current",
                    "task_a_constraint_superseded", "unrelated_repository_isolated",
                ):
                    gates[gate] = "blocked"
                gates["queue_replay_no_growth"] = "not_run"
                if not await_task_b_ready():
                    run["outcome"] = "blocked"
                    validate_artifact(run)
                    return run
                run_task_b()
                run["outcome"] = "blocked"
                validate_artifact(run)
                return run
            if not _memory_projection_empty(baseline_context):
                for gate in (
                    "task_a_constraint_supported", "task_a_unresolved_surfaced",
                    "cross_head_repository_context", "bounded_context_response",
                    "offline_host_context", "task_b_constraint_current",
                    "task_a_constraint_superseded", "unrelated_repository_isolated",
                ):
                    gates[gate] = "blocked"
                gates["queue_replay_no_growth"] = "not_run"
                if not await_task_b_ready():
                    run["outcome"] = "blocked"
                    validate_artifact(run)
                    return run
                run_task_b()
                run["outcome"] = "blocked"
                validate_artifact(run)
                return run
            if baseline_state not in {"disabled", "not_configured"}:
                run["outcome"] = "blocked"
                validate_artifact(run)
                return run

        query = None
        deadline = time.monotonic() + data["limits"]["context_wait_seconds"]
        cross_head_context = None
        cross_head_item = None
        unresolved_item = None
        while time.monotonic() <= deadline:
            cross_head_context = resolve(query)
            cross_head_item = _supported_item(
                cross_head_context, data["task_a"]["constraint"], data["repository_key"],
                "constraint", first["task_id"],
            )
            unresolved_item = _supported_item(
                cross_head_context, data["task_a"]["unresolved"], data["repository_key"],
                "unresolved", first["task_id"],
            )
            if cross_head_item and unresolved_item:
                break
            sleep(min(poll_interval, max(deadline - time.monotonic(), 0)))
        if cross_head_context is None:
            raise RuntimeError("CONTEXT_MISSING")
        query_context = resolve("report")
        queried_policy = _supported_item(
            query_context, data["task_a"]["constraint"], data["repository_key"],
            "constraint", first["task_id"],
        )
        offline_proven = False
        if phase == "candidate":
            offline_proven = _wait_for_file(
                offline_proof_file, validate_offline_proof, offline_proof_wait_seconds, sleep,
            )
        offline_started = time.monotonic()
        offline_context = resolve(query)
        _event(run_id, events, "offline_source_host", [],
               "ok" if offline_proven else "not_run", 0, offline_started,
               None if offline_proven else "OFFLINE_PROOF_NOT_OBSERVED")
        context_sizes = [len(_canonical(cross_head_context)), len(_canonical(offline_context))]
        gates["bounded_context_response"] = (
            "pass" if all(size <= data["limits"]["context_bytes"] for size in context_sizes)
            else "fail"
        )
        offline_item = _supported_item(
            offline_context, data["task_a"]["constraint"], data["repository_key"],
            "constraint", first["task_id"],
        )
        unresolved = _supported_item(
            offline_context, data["task_a"]["unresolved"], data["repository_key"],
            "unresolved", first["task_id"],
        )
        if phase == "baseline":
            run["outcome"] = "blocked"
            for gate in ("task_a_constraint_supported", "task_a_unresolved_surfaced",
                         "cross_head_repository_context", "offline_host_context",
                         "task_b_constraint_current", "task_a_constraint_superseded"):
                gates[gate] = "blocked"
        else:
            gates["task_a_constraint_supported"] = "pass" if offline_item else "fail"
            task_a_summary = _summary_matches_supported_policy(
                cross_head_context, data["task_a"]["constraint"], first["task_id"],
            )
            offline_summary = _summary_matches_supported_policy(
                offline_context, data["task_a"]["constraint"], first["task_id"],
            )
            gates["cross_head_repository_context"] = (
                "pass" if cross_head_item and queried_policy and task_a_summary else "fail"
            )
            gates["offline_host_context"] = (
                "pass" if offline_proven and offline_item and offline_summary
                else "not_run" if not offline_proven else "fail"
            )
            gates["task_a_unresolved_surfaced"] = "pass" if unresolved else "fail"
            if not cross_head_item or not offline_item or not unresolved:
                run["outcome"] = "fail"

        if not await_task_b_ready():
            run["outcome"] = "blocked"
            validate_artifact(run)
            return run
        second = run_task_b()
        new_context = None
        deadline = time.monotonic() + data["limits"]["context_wait_seconds"]
        while time.monotonic() <= deadline:
            new_context = resolve(query)
            toml_item = _supported_item(
                new_context, data["task_b"]["constraint"], data["repository_key"],
                "constraint", second["task_id"],
            )
            if toml_item:
                break
            sleep(min(poll_interval, max(deadline - time.monotonic(), 0)))
        if not isinstance(second, dict) or new_context is None:
            raise RuntimeError("TASK_B_CONTEXT_MISSING")
        if phase == "baseline":
            gates["queue_replay_no_growth"] = "not_run"
            gates["unrelated_repository_isolated"] = "blocked"
            validate_artifact(run)
            return run
        old_text = data["task_b"]["old_constraint"].casefold()
        current_text = " ".join(item["text"] for item in _current_items(new_context)).casefold()
        historical_items = [item for node in _walk(new_context)
                            for item in (node.get("supersession_history") or [])
                            if isinstance(item, dict) and isinstance(item.get("text"), str)
                            and old_text in item["text"].casefold()]
        gates["task_b_constraint_current"] = "pass" if _supported_item(
            new_context, data["task_b"]["constraint"], data["repository_key"],
            "constraint", second["task_id"]) else "fail"
        task_b_summary = _summary_matches_supported_policy(
            new_context, data["task_b"]["constraint"], second["task_id"],
        )
        if not task_b_summary:
            gates["task_b_constraint_current"] = "fail"
        supersession_recorded = any(item.get("status") == "superseded" for item in historical_items)
        gates["task_a_constraint_superseded"] = (
            "pass" if old_text not in current_text and supersession_recorded
            else "fail"
        )
        replay_proven = _wait_for_file(
            queue_replay_manifest,
            lambda path: validate_queue_replay_manifest(path, data["repository_key"]),
            queue_replay_wait_seconds, sleep,
        )
        gates["queue_replay_no_growth"] = "pass" if replay_proven else "not_run"
        unrelated = reader.call("context_resolve", {
            "repository": UNRELATED_REPOSITORY, "query": "report",
            "limit": data["limits"]["knowledge_items"],
        })
        unrelated_text = " ".join(_strings(unrelated)).casefold()
        unrelated_scope = any(
            node.get("repository") == UNRELATED_REPOSITORY
            or node.get("repository_key") == UNRELATED_REPOSITORY
            for node in _walk(unrelated)
        )
        gates["unrelated_repository_isolated"] = (
            "pass" if unrelated_scope
            and data["task_a"]["constraint"].casefold() not in unrelated_text
            and data["task_b"]["constraint"].casefold() not in unrelated_text
            and not any(item.get("scope_id") == data["repository_key"]
                        for item in _knowledge_items(unrelated))
            else "fail"
        )
        context_sizes.extend([len(_canonical(new_context)), len(_canonical(unrelated))])
        gates["bounded_context_response"] = (
            "pass" if all(size <= data["limits"]["context_bytes"] for size in context_sizes)
            else "fail"
        )
        if any(value == "fail" for value in gates.values()):
            run["outcome"] = "fail"
        elif all(value == "pass" for value in gates.values()):
            run["outcome"] = "pass"
        else:
            run["outcome"] = "blocked"
    except RuntimeError as error:
        run["outcome"] = "blocked" if str(error) in {
            "POLL_LIMIT_REACHED", "CONTEXT_MISSING", "OFFLINE_PROOF_NOT_OBSERVED",
        } else "fail"
        # A stable error code is kept only as an event state; no tool body or task text is retained.
    validate_artifact(run)
    return run


def compare_runs(baseline: dict[str, Any], candidate: dict[str, Any], *,
                 independent_gates: dict[str, str] | None = None) -> dict[str, Any]:
    validate_artifact(baseline)
    validate_artifact(candidate)
    if baseline["phase"] != "baseline" or candidate["phase"] != "candidate":
        raise ValueError("memory continuity comparison requires baseline then candidate")
    if baseline["scenario_fingerprint"] != candidate["scenario_fingerprint"]:
        raise ValueError("memory continuity scenario revisions differ")
    same_selectors = baseline["selectors"] == candidate["selectors"]
    same_inputs = baseline["provenance"]["input_sha256"] == candidate["provenance"]["input_sha256"]
    identified_binaries = (
        baseline["provenance"]["binary_sha256"] is not None
        and candidate["provenance"]["binary_sha256"] is not None
    )
    external = independent_gates or {}
    required_external = {
        "issue_completion", "memory_pipeline", "knowledge_quality", "head_switch",
        "offline_host", "tenancy", "retry_recovery", "tests", "final_diff",
    }
    missing = required_external - set(external)
    if missing:
        external = {**external, **{name: "not_run" for name in missing}}
    if any(state not in VALID_GATE_STATES for state in external.values()):
        raise ValueError("invalid independent gate result")
    gates = {"scenario." + name: state for name, state in candidate["gates"].items()}
    gates.update(external)
    baseline_honest = baseline["outcome"] == "not_implemented" or any(
        state == "not_implemented" for state in baseline["gates"].values()
    )
    passed = (
        baseline_honest and candidate["mode"] == "live"
        and candidate["synthesis_mode"] == "live"
        and candidate["outcome"] == "pass" and same_selectors and same_inputs and identified_binaries
        and all(state == "pass" for state in candidate["gates"].values())
        and all(state == "pass" for state in external.values())
    )
    return {
        "schema_version": 1,
        "scenario_id": "memory-continuity",
        "scenario_fingerprint": baseline["scenario_fingerprint"],
        "baseline_run_id": baseline["run_id"],
        "candidate_run_id": candidate["run_id"],
        "baseline_honest_not_implemented": baseline_honest,
        "selectors_comparable": same_selectors,
        "inputs_comparable": same_inputs,
        "binaries_identified": identified_binaries,
        "gates": gates,
        "qualification": "qualified" if passed else "blocked",
    }


def _artifact_path(path: Path | None, run: dict[str, Any]) -> Path:
    if path is not None:
        return path
    return ROOT / "dogfood" / "runs" / f"memory-continuity-{run['phase']}-{run['run_id']}.json"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the bounded memory-continuity dogfood scenario")
    parser.add_argument("phase", choices=("baseline", "candidate"))
    parser.add_argument("--mode", choices=("fixture", "live"), default="fixture")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--fabric-url", default=os.environ.get("TEMOTE_MCP_FABRIC_URL"))
    parser.add_argument("--token-env", default="TEMOTE_MCP_FABRIC_CLIENT_TOKEN")
    parser.add_argument("--session-a", default=os.environ.get("TEMOTE_MCP_MEMORY_SESSION_A"))
    parser.add_argument("--session-b", default=os.environ.get("TEMOTE_MCP_MEMORY_SESSION_B"))
    parser.add_argument("--host-a", default=os.environ.get("TEMOTE_MCP_MEMORY_HOST_A"))
    parser.add_argument("--host-b", default=os.environ.get("TEMOTE_MCP_MEMORY_HOST_B"))
    parser.add_argument("--backend", choices=("codex", "opencode", "devin"), default="opencode")
    parser.add_argument("--model", default=os.environ.get("TEMOTE_MCP_MEMORY_TASK_MODEL", "gpt-5.6-luna"))
    parser.add_argument("--effort", default=os.environ.get("TEMOTE_MCP_MEMORY_TASK_EFFORT", "max"))
    parser.add_argument("--extractor-profile", default=os.environ.get("TEMOTE_MCP_MEMORY_EXTRACTOR_PROFILE"))
    parser.add_argument("--synthesis-mode", choices=("fixture", "live", "not_run"), default="fixture")
    parser.add_argument("--binary", type=Path, help="exact Temote binary used for task A and task B")
    parser.add_argument("--offline-proof-file", type=Path)
    parser.add_argument("--offline-proof-wait-seconds", type=int, default=180)
    parser.add_argument("--task-a-complete-file", type=Path)
    parser.add_argument("--task-b-ready-file", type=Path)
    parser.add_argument("--task-b-complete-file", type=Path)
    parser.add_argument("--task-b-ready-wait-seconds", type=int, default=180)
    parser.add_argument("--source-head", help="verified full source Git object id, for archived baselines")
    parser.add_argument("--working-diff-sha256", help="verified source diff SHA-256, for archived baselines")
    parser.add_argument("--queue-replay-manifest", type=Path)
    parser.add_argument("--queue-replay-wait-seconds", type=int, default=180)
    parser.add_argument("--max-polls", type=int, default=120)
    parser.add_argument("--poll-interval", type=float, default=1.0)
    args = parser.parse_args(argv)
    scenario_data = load_scenario()
    if args.mode == "fixture":
        run = fixture_run(args.phase, scenario_data, source_head=args.source_head,
                           working_diff_sha256=args.working_diff_sha256)
    else:
        token = os.environ.get(args.token_env, "")
        missing = (
            not args.fabric_url or not token or not args.session_a or not args.session_b
            or not args.binary or not args.binary.is_file() or not args.model or not args.effort
            or not args.extractor_profile
        )
        if missing:
            run = fixture_run(args.phase, scenario_data)
            run["mode"] = "live"
            run["synthesis_mode"] = args.synthesis_mode
            run["outcome"] = "blocked"
            run["selectors"] = {
                "backend": args.backend, "model": args.model, "effort": args.effort,
                "extractor_profile": args.extractor_profile,
            }
            run["provenance"] = _source_identity(
                args.binary, scenario_data, source_head=args.source_head,
                working_diff_sha256=args.working_diff_sha256,
            )
            validate_artifact(run)
        else:
            run = live_run(phase=args.phase, endpoint=args.fabric_url, token=token,
                           session_a=args.session_a, session_b=args.session_b,
                           host_a=args.host_a, host_b=args.host_b, backend=args.backend,
                           model=args.model, effort=args.effort,
                           extractor_profile=args.extractor_profile,
                           synthesis_mode=args.synthesis_mode, binary=args.binary,
                           offline_proof_file=args.offline_proof_file,
                           offline_proof_wait_seconds=args.offline_proof_wait_seconds,
                           task_a_complete_file=args.task_a_complete_file,
                           task_b_ready_file=args.task_b_ready_file,
                           task_b_complete_file=args.task_b_complete_file,
                           task_b_ready_wait_seconds=args.task_b_ready_wait_seconds,
                           queue_replay_manifest=args.queue_replay_manifest,
                           queue_replay_wait_seconds=args.queue_replay_wait_seconds,
                           source_head=args.source_head,
                           working_diff_sha256=args.working_diff_sha256,
                           max_polls=args.max_polls, poll_interval=args.poll_interval,
                           scenario_data=scenario_data)
    path = _artifact_path(args.output, run)
    _save_artifact(path, run)
    print(json.dumps({"outcome": run["outcome"], "mode": run["mode"], "phase": run["phase"],
                      "synthesis_mode": run["synthesis_mode"],
                      "artifact": str(path), "gates": run["gates"]}, sort_keys=True))
    return 0 if run["outcome"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
