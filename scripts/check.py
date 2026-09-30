#!/usr/bin/env python3
"""Run the same deterministic gates locally and in CI, retaining logs and timings."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="use only cached Cargo dependencies")
    parser.add_argument("--artifacts", type=Path, help="new directory for logs and results.json")
    args = parser.parse_args()
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or ROOT / ".work" / "checks" / f"{timestamp}-{os.getpid()}"
    artifacts = artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=False)
    cargo_flags = ["--locked", *(["--offline"] if args.offline else [])]
    gates = [
        ("rust-fmt", ["cargo", "fmt", "--all", "--", "--check"]),
        ("browser-build", ["npm", "run", "check:browser"]),
        ("browser-tests", ["npm", "test"]),
        ("contracts", [sys.executable, "scripts/validate-contracts.py"]),
        ("rust-tests", ["cargo", "test", *cargo_flags]),
        ("rust-clippy", ["cargo", "clippy", *cargo_flags, "--all-targets", "--", "-D", "warnings"]),
    ]
    print(f"Artifacts: {artifacts}", flush=True)
    results = []
    started = time.monotonic()
    for name, command in gates:
        print(f"Running {name}…", flush=True)
        stage_started = time.monotonic()
        log = artifacts / f"{name}.log"
        with log.open("w", encoding="utf-8") as output:
            try:
                status = subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT).returncode
            except OSError as error:
                output.write(f"Could not start {command[0]}: {error}\n")
                status = 127
        elapsed = time.monotonic() - stage_started
        results.append({"name": name, "command": command, "seconds": elapsed, "exit_code": status, "log": str(log)})
        print(f"{name}: {'PASS' if status == 0 else 'FAIL'} ({elapsed:.3f}s)", flush=True)
        if status:
            print(log.read_text(encoding="utf-8", errors="replace"), end="", flush=True)
    elapsed = time.monotonic() - started
    passed = all(stage["exit_code"] == 0 for stage in results)
    summary = {"passed": passed, "seconds": elapsed, "stages": results}
    (artifacts / "results.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(f"{'PASS' if passed else 'FAIL'}: {elapsed:.3f}s; {artifacts / 'results.json'}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
