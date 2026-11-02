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
    if len(tests) != len(set(tests)):
        raise ValueError("duplicate enumerated test")
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


def validate_completion(output, expected):
    summaries = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", output, re.M)
    if not expected or summaries != [(str(len(expected)), "0", "0")]:
        raise ValueError("successful process did not execute every selected test")


def validate_exclusion_audit(selected, exclusions, audit):
    names = [row["test"] for row in audit]
    if len(names) != len(set(names)) or set(names) != {row["test"] for row in exclusions}:
        raise ValueError("exclusion audit must cover each exclusion exactly once")
    for row in audit:
        if row["category"] not in {"unsupported-os", "workload-cost", "opt-in-probe"}:
            raise ValueError("unknown exclusion category")
        if row["alternative"] not in {"native-and-asan", "not-required"} or not row["remaining_gap"]:
            raise ValueError("missing alternative or remaining gap")
        if (row["category"] == "opt-in-probe") != (row["alternative"] == "not-required"):
            raise ValueError("ignored probes cannot count as required coverage")
        if not set(row["portable_tests"]).issubset(selected):
            raise ValueError("portable counterpart must actually be selected by Miri")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain", default="nightly-2026-10-12")
    parser.add_argument("--output", type=Path, default=ROOT / "target/miri-report")
    parser.add_argument("--list-only", action="store_true")
    parser.add_argument("--test", choices=["fallible_ownership"])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    exclusions = [] if args.test else json.loads((ROOT / "scripts/miri-exclusions.json").read_text())
    command = ["cargo", f"+{args.toolchain}", "miri", "test", "--locked",
               "-p", "varlit-core", "--no-default-features"]
    command += ["--test", args.test, "--"] if args.test else ["--lib", "--"]
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
    if not args.test:
        audit = json.loads((ROOT / "scripts/miri-exclusion-audit.json").read_text())
        validate_exclusion_audit(report["selected"], exclusions, audit)
        report["exclusion_audit"] = audit
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
    if result.returncode == 0:
        try:
            validate_completion(output, report["selected"])
        except ValueError as error:
            report["error"] = str(error)
            report["exit_code"] = 1
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(output[-6000:], flush=True)
    return report["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
