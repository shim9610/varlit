#!/usr/bin/env python3
"""Fail the stable PR gate on absent, failed, cancelled or skipped jobs.

A documentation-only pull request (`ci_changes.py`) skips the jobs that build
and run code. Then those jobs must be skipped or succeed, and the jobs that
still run for documentation (`DOCS_JOBS`) must succeed. Anything else must
succeed.
"""
import json
import os
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


# Jobs that run on every pull request, documentation-only or not.
DOCS_JOBS = {"changes", "fmt", "rustdoc"}


def validate(results, expected, docs_only=False):
    if not isinstance(results, dict) or not results:
        raise ValueError("CI job results are missing")
    if set(results) != set(expected):
        raise ValueError(f"CI dependency mismatch: expected {sorted(expected)}, got {sorted(results)}")
    if docs_only and not DOCS_JOBS <= set(results):
        raise ValueError(f"documentation-only run without its jobs: {sorted(DOCS_JOBS - set(results))}")

    def accepted(name, job):
        if not isinstance(job, dict):
            return False
        result = job.get("result")
        if docs_only and name not in DOCS_JOBS:
            return result in ("success", "skipped")
        return result == "success"

    rejected = [name for name, job in results.items() if not accepted(name, job)]
    if rejected:
        raise ValueError(f"CI jobs did not succeed: {', '.join(sorted(rejected))}")


def main():
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    jobs = workflow.split("\njobs:\n", 1)[1]
    expected = set(re.findall(r"^  ([A-Za-z_][A-Za-z0-9_-]*):$", jobs, re.MULTILINE))
    expected.remove("required-gate")
    docs_only = os.environ.get("VARLIT_CI_DOCS_ONLY") == "true"
    validate(json.loads(os.environ["VARLIT_CI_NEEDS"]), expected, docs_only)
    if docs_only:
        print(f"Documentation-only change: {', '.join(sorted(DOCS_JOBS))} succeeded; "
              "the other job groups were skipped or succeeded")
    else:
        print(f"All {len(expected)} CI job groups succeeded")


if __name__ == "__main__":
    main()
