#!/usr/bin/env python3
"""Run Miri on the code that can have undefined behaviour, and prove that is all of it.

Safe Rust cannot cause undefined behaviour by itself, so interpreting tests of
modules with no `unsafe` re-checks the standard library at several thousand
times the cost. The three crates deny `unsafe_code`; every place that allows
it is declared in scripts/miri-scope.json, either with the unit tests Miri
interprets for it or with the reason Miri cannot (foreign calls, mapping) and
what covers it instead. This script refuses to run when the sources and the
manifest disagree, so new `unsafe` cannot appear outside the boundary unnoticed.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SCOPE = ROOT / "scripts/miri-scope.json"
CRATE_ROOTS = ["crates/varlit-core/src/lib.rs", "crates/varlit/src/lib.rs",
               "crates/varlit-macros/src/lib.rs"]
ALLOW = re.compile(r"allow\(\s*unsafe_code\b")
DENY = re.compile(r"^#!\[(deny|forbid)\(unsafe_code\)\]$", re.M)


def unsafe_sources(root=ROOT):
    """Every source file that allows `unsafe_code`, relative to the repository."""
    found = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        if ALLOW.search(path.read_text(encoding="utf-8")):
            found.append(path.relative_to(root).as_posix())
    return found


def validate_boundary(scope, sources, crate_roots):
    """The manifest names exactly the files that allow unsafe, each once."""
    for name, text in crate_roots.items():
        if not DENY.search(text):
            raise ValueError(f"{name} does not deny unsafe_code")
    declared = [row["source"] for row in scope["interpreted"] + scope["not_interpreted"]]
    if len(declared) != len(set(declared)):
        raise ValueError("a source is declared twice in the Miri scope")
    if set(declared) != set(sources):
        raise ValueError(
            "unsafe boundary changed: undeclared="
            f"{sorted(set(sources) - set(declared))}, stale={sorted(set(declared) - set(sources))}")
    for row in scope["interpreted"]:
        if not row.get("tests") or not row.get("why"):
            raise ValueError(f"interpreted source needs tests and a reason: {row['source']}")
    for row in scope["not_interpreted"]:
        if not row.get("reason") or not row.get("covered_by"):
            raise ValueError(f"uninterpreted source needs a reason and its coverage: {row['source']}")


def selected_tests(tests, prefixes, exclusions):
    """Tests under the given prefixes, minus exact documented exclusions."""
    if len(tests) != len(set(tests)):
        raise ValueError("duplicate enumerated test")
    for prefix in prefixes:
        if not any(test.startswith(prefix) for test in tests):
            raise ValueError(f"Miri scope prefix selects no test: {prefix}")
    chosen = [test for test in tests if any(test.startswith(prefix) for prefix in prefixes)]
    names = [entry["test"] for entry in exclusions]
    if len(names) != len(set(names)):
        raise ValueError("duplicate Miri exclusion")
    for entry in exclusions:
        if not entry.get("reason"):
            raise ValueError(f"missing exclusion reason: {entry['test']}")
        if entry["test"] not in chosen:
            raise ValueError(f"stale Miri exclusion: {entry['test']}")
    selected = [test for test in chosen if test not in names]
    if not selected:
        raise ValueError("Miri selection contains no tests")
    return selected


def validate_completion(output, expected):
    summaries = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", output, re.M)
    if not expected or summaries != [(str(len(expected)), "0", "0")]:
        raise ValueError("successful process did not execute every selected test")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain", default="nightly-2026-10-12")
    parser.add_argument("--output", type=Path, default=ROOT / "target/miri-report")
    parser.add_argument("--list-only", action="store_true")
    parser.add_argument("--test", choices=["fallible_ownership"])
    parser.add_argument("--target", help="interpret for another target, e.g. s390x-unknown-linux-gnu")
    parser.add_argument("--prefix", action="append",
                        help="test path prefixes to run instead of the unsafe-module tests")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    scope = json.loads(SCOPE.read_text())
    validate_boundary(scope, unsafe_sources(),
                      {name: (ROOT / name).read_text(encoding="utf-8") for name in CRATE_ROOTS})
    command = ["cargo", f"+{args.toolchain}", "miri", "test", "--locked",
               "-p", "varlit-core", "--no-default-features"]
    if args.target:
        command += ["--target", args.target]
    command += ["--test", args.test, "--"] if args.test else ["--lib", "--"]
    env = {**os.environ, "MIRIFLAGS": "-Zmiri-disable-isolation",
           "CARGO_BUILD_JOBS": "4", "RUST_TEST_THREADS": "1", "CARGO_TERM_COLOR": "never"}
    # Preserve default alias/validity checks; never use permissive provenance flags.
    report = {"command": command, "miriflags": env["MIRIFLAGS"], "scope": scope}
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
    if args.test:
        selected = selected_tests(tests, [""], [])
    else:
        prefixes = args.prefix or [prefix for row in scope["interpreted"] for prefix in row["tests"]]
        selected = selected_tests(tests, prefixes, [] if args.prefix else scope["exclusions"])
    report["selected"] = selected
    report["enumerated_count"] = len(tests)
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Miri: {len(selected)} selected / {len(tests)} enumerated"
          f"{' for ' + args.target if args.target else ''}", flush=True)
    if args.list_only:
        return 0
    filters = [] if args.test else ["--exact", *selected]
    with (args.output / "results.log").open("w") as log:
        result = subprocess.run(command + filters, cwd=ROOT, env=env,
                                stdout=log, stderr=subprocess.STDOUT)
    report["exit_code"] = result.returncode
    output = (args.output / "results.log").read_text()
    if result.returncode == 0:
        try:
            validate_completion(output, selected)
        except ValueError as error:
            report["error"] = str(error)
            report["exit_code"] = 1
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    print(output[-6000:], flush=True)
    return report["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
