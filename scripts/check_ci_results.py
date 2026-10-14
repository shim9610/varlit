#!/usr/bin/env python3
"""Fail the stable PR gate on absent, failed, cancelled or skipped jobs."""
import json
import os
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def validate(results, expected):
    if not isinstance(results, dict) or not results:
        raise ValueError("CI job results are missing")
    if set(results) != set(expected):
        raise ValueError(f"CI dependency mismatch: expected {sorted(expected)}, got {sorted(results)}")
    rejected = [name for name, job in results.items()
                if not isinstance(job, dict) or job.get("result") != "success"]
    if rejected:
        raise ValueError(f"CI jobs did not succeed: {', '.join(sorted(rejected))}")


def main():
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    jobs = workflow.split("\njobs:\n", 1)[1]
    expected = set(re.findall(r"^  ([A-Za-z_][A-Za-z0-9_-]*):$", jobs, re.MULTILINE))
    expected.remove("required-gate")
    validate(json.loads(os.environ["VARLIT_CI_NEEDS"]), expected)
    print(f"All {len(expected)} CI job groups succeeded")


if __name__ == "__main__":
    main()
