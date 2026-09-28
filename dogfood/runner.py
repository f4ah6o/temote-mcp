"""Deterministic fixture and conservative stdio MCP adapter for dogfood runs."""

from __future__ import annotations

import json
import re
import select
import subprocess
import time
import uuid
import os
import urllib.request
import urllib.error
import urllib.parse
from pathlib import Path

from .protocol import Recorder, TERMINAL, metrics, snapshot

RELEASE_GATES = frozenset({"final_diff", "tests", "git_status", "ci", "action_result"})


class AdapterError(Exception):
    def __init__(self, code: str, retryable: bool = False):
        super().__init__(code)
        self.code, self.retryable = code, retryable


class FakeAdapter:
    """Reference model: one accepted task per operation ID, with bounded fixture faults."""

    def __init__(self):
        self.tasks: dict[str, dict] = {}
        self.polls = 0

    def call(self, tool: str, arguments: dict) -> dict:
        if tool == "session_info":
            return {"session_id": arguments["session_id"], "server_contract_fingerprint": "f" * 64}
        if tool == "task_list":
            return {"tasks": list(self.tasks.values()), "backends": {
                name: {"status": "ok"} for name in ("codex", "opencode", "devin")}}
        if tool.endswith("_task_start"):
            op = arguments["operation_id"]
            if op not in self.tasks:
                self.tasks[op] = {"task_id": str(uuid.uuid5(uuid.NAMESPACE_URL, op)),
                                  "status": "running", "backend": tool.removesuffix("_task_start")}
            return self.tasks[op].copy()
        if tool.endswith("_task_get"):
            task = next((v for v in self.tasks.values() if v["task_id"] == arguments["task_id"]), None)
            if task is None:
                raise AdapterError("TASK_NOT_FOUND")
            self.polls += 1
            if self.polls >= 2:
                task["status"] = "completed"
            view = task.copy()
            if view["status"] == "completed":
                view["evidence"] = {"evidence_id": "fixture-evidence"}
            return view
        if tool == "evidence_read":
            return {"content": "fixture result"}
        raise AdapterError("UNKNOWN_TOOL")


class LiveAdapter:
    def __init__(self, binary: Path, *, lifecycle_url=None, lifecycle_token_env=None):
        if not binary.is_file():
            raise ValueError("Temote binary does not exist")
        self.binary = binary
        self.lifecycle_url = lifecycle_url
        self.lifecycle_token_env = lifecycle_token_env
        if bool(lifecycle_url) != bool(lifecycle_token_env):
            raise ValueError('HTTP lifecycle requires both URL and token environment variable')
        if lifecycle_url:
            parsed = urllib.parse.urlsplit(lifecycle_url)
            if (parsed.username or parsed.password or parsed.query or parsed.fragment
                    or parsed.path != '/mcp' or not parsed.hostname
                    or not (parsed.scheme == 'https' or
                            parsed.scheme == 'http' and parsed.hostname in {'127.0.0.1', '::1', 'localhost'})):
                raise ValueError('lifecycle URL must be HTTPS or loopback HTTP /mcp without credentials')
        self.owners: list[subprocess.Popen] = []
        self._open()

    def _open(self) -> None:
        self.process = subprocess.Popen([str(self.binary), "mcp"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        text=True, bufsize=1)
        self.owners.append(self.process)
        self.request_id = 0
        self._rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "temote-dogfood", "version": "1"}})

    def reconnect(self) -> None:
        # Keep the first MCP process alive: it owns the running child runtime.
        # A second process exercises discovery without cancelling that task.
        self._open()

    def close(self) -> None:
        for process in self.owners:
            self._close_process(process)
        self.owners.clear()

    @staticmethod
    def _close_process(process: subprocess.Popen) -> None:
        if process.stdin:
            process.stdin.close()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=5)

    def release_prior_owners(self) -> None:
        for process in self.owners[:-1]:
            self._close_process(process)
        self.owners = self.owners[-1:]

    def _rpc(self, method: str, params: dict) -> dict:
        self.request_id += 1
        request = {"jsonrpc": "2.0", "id": self.request_id, "method": method, "params": params}
        try:
            assert self.process.stdin and self.process.stdout
            self.process.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
            self.process.stdin.flush()
            ready, _, _ = select.select([self.process.stdout], [], [], 45)
            if not ready:
                raise AdapterError("MCP_TIMEOUT", retryable=True)
            answer = json.loads(self.process.stdout.readline())
        except (BrokenPipeError, OSError, ValueError) as error:
            raise AdapterError("MCP_RESPONSE_INVALID", retryable=True) from error
        if answer.get("id") != self.request_id:
            raise AdapterError("MCP_RESPONSE_ID_MISMATCH", retryable=True)
        if "error" in answer:
            message = answer["error"].get("message", "")
            match = re.match(r"^([A-Z][A-Z0-9_]{2,63}):", message)
            raise AdapterError(match.group(1) if match else "MCP_TOOL_ERROR")
        result = answer.get("result", {})
        if not isinstance(result, dict):
            raise AdapterError("MCP_RESULT_INVALID")
        return result

    def call(self, tool: str, arguments: dict) -> dict:
        if tool == 'session_start' and self.lifecycle_url:
            token = os.environ.get(self.lifecycle_token_env, '')
            if not token:
                raise AdapterError('LIFECYCLE_AUTH_UNAVAILABLE')
            body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                               'params': {'name': tool, 'arguments': arguments}}).encode()
            request = urllib.request.Request(self.lifecycle_url, data=body,
                      headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + token})
            try:
                class NoRedirect(urllib.request.HTTPRedirectHandler):
                    def redirect_request(self, req, fp, code, msg, headers, newurl):
                        return None
                with urllib.request.build_opener(NoRedirect).open(request, timeout=45) as response:
                    payload = response.read(1048577)
                if len(payload) > 1048576:
                    raise AdapterError('LIFECYCLE_RESPONSE_TOO_LARGE')
                answer = json.loads(payload)
            except urllib.error.HTTPError as error:
                raise AdapterError('LIFECYCLE_HTTP_ERROR') from error
            except (OSError, ValueError) as error:
                raise AdapterError('LIFECYCLE_TRANSPORT_UNAVAILABLE', retryable=True) from error
            if not isinstance(answer, dict) or answer.get('id') != 1 or 'error' in answer:
                raise AdapterError('LIFECYCLE_MCP_ERROR')
            result = answer.get('result', {})
        else:
            result = self._rpc("tools/call", {"name": tool, "arguments": arguments})
        if isinstance(result, dict) and "content" in result:
            try:
                return json.loads(result["content"][0]["text"])
            except (ValueError, IndexError, KeyError, TypeError) as error:
                raise AdapterError("MCP_CONTENT_INVALID") from error
        if not isinstance(result, dict):
            raise AdapterError("MCP_RESULT_INVALID")
        return result


def execute(scenario_data: dict, phase: str, adapter: FakeAdapter | LiveAdapter, *,
            session_id: str, repository_head: str, binary_identity: str,
            backend: str = "codex", model: str | None = None, effort: str | None = None,
            max_polls: int = 20, poll_interval: float = 1.0,
            terminal_read_strategy: str = "reuse",
            gates: dict[str, str] | None = None,
            root: str = "src", source: str = "src/temote-mcp-df", destination: str | None = None) -> dict:
    if scenario_data["id"] == "repository-setup":
        from .repository_setup import execute_setup
        return execute_setup(scenario_data, phase, adapter, session_id=session_id,
                             repository_head=repository_head, binary_identity=binary_identity,
                             backend=backend, model=model, effort=effort, max_polls=max_polls,
                             poll_interval=poll_interval, root=root, source=source,
                             destination=destination, gates=gates)
    if backend not in {"codex", "opencode", "devin"}:
        raise ValueError("unsupported backend")
    if max_polls < 1 or max_polls > 200 or poll_interval < 0:
        raise ValueError("invalid bounded wait")
    if terminal_read_strategy not in {"reuse", "reread"}:
        raise ValueError("invalid terminal read strategy")
    run_id = str(uuid.uuid4())
    recorder = Recorder(run_id, scenario_data, phase)
    task_id: str | None = None
    operation_id = str(uuid.uuid4())
    accepted_start_ids: set[str] = set()
    original_args: dict | None = None
    prior_ids: set[str] = set()
    assertions = {key: "not_run" for key in scenario_data["assertions"]}
    outcome = "pass"
    contract = "f" * 64 if isinstance(adapter, FakeAdapter) else "unknown"
    last_state = "not_run"
    terminal_view: dict | None = None
    recover_next_poll = False

    def observed(operation: str, tool: str, args: dict, *, recovery: bool = False) -> dict:
        nonlocal outcome, last_state
        started = time.monotonic()
        try:
            response = adapter.call(tool, args)
            state = response.get("status", response.get("state", "ok"))
            if not isinstance(state, str):
                state = "unknown"
            recorder.call(operation, tool, args, response, state=state,
                          next_action="continue" if state not in TERMINAL else "inspect_result",
                          duration_ms=int((time.monotonic() - started) * 1000), recovery=recovery)
            last_state = state
            return response
        except AdapterError as error:
            recorder.call(operation, tool, args, {"error_code": error.code}, state="error",
                          error_code=error.code, retryable=error.retryable,
                          next_action="retry_same_operation" if error.retryable else "inspect_state",
                          duration_ms=int((time.monotonic() - started) * 1000), recovery=recovery)
            raise

    try:
        for operation in scenario_data["operations"]:
            if operation == "inspect_session":
                info = observed(operation, "session_info", {"session_id": session_id})
                contract = info.get("server_contract_fingerprint", contract)
                if not isinstance(contract, str) or not contract:
                    raise AdapterError("CONTRACT_IDENTITY_UNAVAILABLE")
            elif operation in {"start_agent", "start_agent_uncertain"}:
                if "rediscover_task" in scenario_data["operations"]:
                    listing = observed("rediscover_task", "task_list", {"session_id": session_id, "limit": 100})
                    prior_ids = {x["task_id"] for x in listing.get("tasks", []) if "task_id" in x}
                original_args = {"session_id": session_id, "operation_id": operation_id,
                                 "task": "Read the current repository status and return a short factual report. Do not modify files."}
                if model:
                    original_args["model"] = model
                if effort and backend == "codex":
                    original_args["effort"] = effort
                response = observed(operation, f"{backend}_task_start", original_args)
                task_id = response.get("task_id")
                if not isinstance(task_id, str) or not task_id:
                    raise AdapterError("TASK_ID_MISSING")
                accepted_start_ids.add(task_id)
                if operation == "start_agent_uncertain":
                    task_id = None  # Drop the response as if transport failed after acceptance.
            elif operation == "retry_same_start":
                if original_args is None:
                    raise AdapterError("START_NOT_ATTEMPTED")
                response = observed(operation, f"{backend}_task_start", original_args, recovery=True)
                task_id = response.get("task_id")
                accepted_start_ids.add(task_id)
                assertions["exact_retry"] = "pass" if len(accepted_start_ids) == 1 else "fail"
            elif operation == "lose_task_id":
                task_id = None
                if isinstance(adapter, LiveAdapter):
                    adapter.reconnect()
            elif operation == "rediscover_task":
                listing = observed(operation, "task_list", {"session_id": session_id, "limit": 100}, recovery=True)
                backend_state = listing.get("backends", {}).get(backend, {}).get("status")
                if backend_state == "unavailable":
                    raise AdapterError("BACKEND_LIST_UNAVAILABLE", retryable=True)
                candidates = [x["task_id"] for x in listing.get("tasks", [])
                              if x.get("backend") == backend and x.get("task_id") not in prior_ids]
                if len(candidates) != 1:
                    raise AdapterError("TASK_REDISCOVERY_AMBIGUOUS")
                task_id = candidates[0]
                assertions["tasks_can_be_rediscovered"] = "pass"
            elif operation == "inject_poll_failure":
                # A transport fault is observed without contacting Temote or replaying start.
                recorder.call(operation, "transport", {"task_id": task_id}, {"error_code": "TRANSIENT_POLL"},
                              state="error", error_code="TRANSIENT_POLL", retryable=True,
                              next_action="retry_get", recovery=True)
                assertions["retry_is_machine_decidable"] = "pass"
                recover_next_poll = True
            elif operation == "wait_until_terminal":
                if not task_id:
                    raise AdapterError("TASK_ID_MISSING")
                for index in range(max_polls):
                    view = observed(operation, f"{backend}_task_get",
                                    {"session_id": session_id, "task_id": task_id}, recovery=recover_next_poll)
                    recover_next_poll = False
                    last_state = view.get("status", "unknown")
                    if last_state in TERMINAL:
                        terminal_view = view
                        break
                    if index + 1 < max_polls:
                        time.sleep(poll_interval)
                if last_state not in TERMINAL:
                    raise AdapterError("POLL_LIMIT_REACHED", retryable=True)
                assertions["terminal_state_is_unambiguous"] = "pass"
                if last_state != "completed":
                    raise AdapterError("TASK_NOT_COMPLETED")
            elif operation == "read_terminal_result":
                if not task_id:
                    raise AdapterError("TASK_ID_MISSING")
                # The terminal poll already carries the evidence reference.
                view = terminal_view if terminal_read_strategy == "reuse" else None
                if view is None:
                    view = observed(operation, f"{backend}_task_get", {"session_id": session_id, "task_id": task_id})
                if view.get("status") not in TERMINAL:
                    raise AdapterError("TERMINAL_RESULT_MISSING")
                evidence_id = (view.get("evidence") or {}).get("evidence_id")
                if not evidence_id and view.get("report") is None:
                    if isinstance(adapter, LiveAdapter) and len(adapter.owners) > 1:
                        adapter.release_prior_owners()
                    view = observed(operation, f"{backend}_task_get",
                                    {"session_id": session_id, "task_id": task_id}, recovery=True)
                    evidence_id = (view.get("evidence") or {}).get("evidence_id")
                if evidence_id:
                    observed(operation, "evidence_read", {"session_id": session_id,
                                                          "evidence_id": evidence_id, "max_bytes": 16384})
                elif view.get("report") is None:
                    raise AdapterError("TERMINAL_EVIDENCE_MISSING")
                assertions["bounded_result"] = "pass"
        if "no_duplicate_task" in assertions:
            assertions["no_duplicate_task"] = "pass" if len(accepted_start_ids) == 1 else "fail"
        if "identity_is_separate" in assertions:
            assertions["identity_is_separate"] = "pass" if repository_head != binary_identity else "fail"
        if "repository_gates_recorded" in assertions:
            assertions["repository_gates_recorded"] = (
                "pass" if gates and RELEASE_GATES <= gates.keys()
                and all(gates[key] == "pass" for key in RELEASE_GATES) else "blocked")
        if any(v != "pass" for v in assertions.values()):
            outcome = "blocked" if any(v in {"blocked", "not_run"} for v in assertions.values()) else "fail"
    except AdapterError as error:
        outcome = "blocked" if error.retryable or error.code.endswith("UNAVAILABLE") or error.code == "POLL_LIMIT_REACHED" else "fail"

    run = {
        "schema_version": 1, "run_id": run_id, "phase": phase,
        "scenario_id": scenario_data["id"], "scenario_revision": scenario_data["revision"],
        "scenario_fingerprint": scenario_data["fingerprint"],
        "snapshot": snapshot(repository_head, binary_identity, contract, {"backend": backend, "model": model,
                                                                           "effort": effort, "terminal_read_strategy": terminal_read_strategy}),
        "outcome": outcome, "assertions": assertions, "events": recorder.events,
        "metrics": metrics(recorder.events), "last_state": last_state,
        "gates": gates or {},
    }
    return run
