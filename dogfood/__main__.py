"""Run with ``python3 -m dogfood`` from the repository root."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

from .protocol import compare, load, save, scenario
from .runner import FakeAdapter, LiveAdapter, execute

ROOT = Path(__file__).resolve().parent.parent
SCENARIOS = ROOT / "dogfood" / "scenarios"


def main() -> None:
    parser = argparse.ArgumentParser(description="Temote repository-owned dogfood harness")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("validate", help="validate all checked-in scenarios")
    run = commands.add_parser("run", help="run a scenario and save bounded observations")
    run.add_argument("phase", choices=("baseline", "candidate"))
    run.add_argument("scenario", choices=sorted(p.stem for p in SCENARIOS.glob("*.json")))
    run.add_argument("--adapter", choices=("fake", "live"), default="fake")
    run.add_argument("--binary", type=Path)
    run.add_argument("--session-id", default="dogfood-fixture")
    run.add_argument("--backend", choices=("codex", "opencode", "devin"), default="codex")
    run.add_argument("--model")
    run.add_argument("--effort")
    run.add_argument("--max-polls", type=int, default=20)
    run.add_argument("--poll-interval", type=float, default=1.0)
    run.add_argument("--terminal-read-strategy", choices=("reuse", "reread"), default="reuse")
    run.add_argument("--gates", type=Path, help="JSON map of independently observed gate outcomes")
    run.add_argument("--output", type=Path)
    comparison = commands.add_parser("compare", help="compare immutable baseline and candidate runs")
    comparison.add_argument("baseline", type=Path)
    comparison.add_argument("candidate", type=Path)
    comparison.add_argument("--gates", type=Path)
    comparison.add_argument("--target-metric", action="append", default=[])
    comparison.add_argument("--target-operation", action="append", default=[])
    comparison.add_argument("--target-assertion", action="append", default=[])
    comparison.add_argument("--output", type=Path)
    args = parser.parse_args()

    if args.command == "validate":
        checked = [scenario(path)["id"] for path in sorted(SCENARIOS.glob("*.json"))]
        print(json.dumps({"validated": checked}))
        return
    if args.command == "compare":
        gates = json.loads(args.gates.read_text()) if args.gates else {}
        result = compare(load(args.baseline), load(args.candidate), gates=gates,
                         target_metrics=args.target_metric, target_operations=args.target_operation,
                         target_assertions=args.target_assertion)
        if args.output:
            if args.output.exists():
                raise FileExistsError(args.output)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
        print(json.dumps({"qualification": result["qualification"], "report": str(args.output) if args.output else result}))
        if result["qualification"] != "qualified":
            sys.exit(1)
        return

    source = scenario(SCENARIOS / f"{args.scenario}.json")
    gates = json.loads(args.gates.read_text()) if args.gates else {}
    if args.adapter == "live":
        if not args.binary or args.session_id == "dogfood-fixture":
            parser.error("live runs require --binary and an existing --session-id")
        binary = args.binary.resolve()
        binary_identity = hashlib.sha256(binary.read_bytes()).hexdigest()
        adapter = LiveAdapter(binary)
    else:
        binary_identity = "fixture-binary"
        adapter = FakeAdapter()
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
                          capture_output=True, check=True).stdout.strip()
    try:
        result = execute(source, args.phase, adapter, session_id=args.session_id,
                         repository_head=head, binary_identity=binary_identity,
                         backend=args.backend, model=args.model, effort=args.effort,
                         max_polls=args.max_polls, poll_interval=args.poll_interval,
                         terminal_read_strategy=args.terminal_read_strategy,
                         gates=gates)
    finally:
        if isinstance(adapter, LiveAdapter):
            adapter.close()
    output = args.output or ROOT / "dogfood" / "runs" / f"{result['run_id']}.json"
    save(output, result)
    print(json.dumps({"outcome": result["outcome"], "run_id": result["run_id"],
                      "artifact": str(output), "tool_calls": result["metrics"]["tool_calls"]}))
    if result["outcome"] != "pass":
        sys.exit(1)


if __name__ == "__main__":
    main()
