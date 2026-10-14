#!/usr/bin/env python3
"""Run the portable Miri gate with explicit, checked exclusions and saved logs."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def selected_tests(tests, exclusions):
    """Refuse stale names and substring collisions in libtest's --skip filters."""
    names = [entry["test"] for entry in exclusions]
    if len(names) != len(set(names)):
        raise ValueError("duplicate Miri exclusion")
    for entry in exclusions:
        name = entry["test"]
        if not entry.get("reason"):
            raise ValueError(f"missing exclusion reason: {name}")
        if [test for test in tests if name in test] != [name]:
            raise ValueError(f"stale or ambiguous Miri exclusion: {name}")
    selected = [test for test in tests if test not in names]
    if not selected:
        raise ValueError("Miri selection contains no tests")
    return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain", default="nightly-2026-10-12")
    parser.add_argument("--output", type=Path, default=ROOT / "target/miri-report")
    parser.add_argument("--list-only", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    exclusions = json.loads((ROOT / "scripts/miri-exclusions.json").read_text())
    command = ["cargo", f"+{args.toolchain}", "miri", "test", "--locked",
               "-p", "varlit-core", "--no-default-features", "--lib", "--"]
    env = {**os.environ, "MIRIFLAGS": "-Zmiri-disable-isolation",
           "CARGO_BUILD_JOBS": "4", "RUST_TEST_THREADS": "1", "CARGO_TERM_COLOR": "never"}
    # Preserve default alias/validity checks; never use permissive provenance flags.
    report = {"command": command, "miriflags": env["MIRIFLAGS"], "exclusions": exclusions}
    for label, cmd in [("rustc", ["rustc", f"+{args.toolchain}", "-vV"]),
                       ("miri", ["cargo", f"+{args.toolchain}", "miri", "--version"]),
                       ("head", ["git", "rev-parse", "HEAD"]),
                       ("dirty", ["git", "status", "--porcelain"])]:
        report[label] = subprocess.check_output(cmd, cwd=ROOT, env=env, text=True).strip()
    report_path = args.output / "selection.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    with (args.output / "test-list.log").open("w") as log:
        result = subprocess.run(command + ["--list"], cwd=ROOT, env=env,
                                stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        return result.returncode
    listing = (args.output / "test-list.log").read_text()
    tests = [line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test")]
    report["selected"] = selected_tests(tests, exclusions)
    report["enumerated_count"] = len(tests)
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Miri: {len(report['selected'])} selected / {len(tests)} enumerated; "
          f"{len(exclusions)} documented exclusions", flush=True)
    if args.list_only:
        return 0
    filters = [part for entry in exclusions for part in ("--skip", entry["test"])]
    with (args.output / "results.log").open("w") as log:
        result = subprocess.run(command + filters, cwd=ROOT, env=env,
                                stdout=log, stderr=subprocess.STDOUT)
    report["exit_code"] = result.returncode
    output = (args.output / "results.log").read_text()
    match = re.search(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored;", output)
    if result.returncode == 0 and (not match or sum(map(int, match.groups())) != len(report["selected"])):
        report["error"] = "successful process did not account for every selected test"
        report["exit_code"] = 1
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(output[-6000:], flush=True)
    return report["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
