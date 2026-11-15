#!/usr/bin/env python3
"""Retain interrupted evidence; retry refused timings and reprobe frozen grouped binaries."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import run as bench


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--original", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    original, output = args.original.resolve(), args.output.resolve()
    manifest = json.loads((original / "manifest.json").read_text())
    output.mkdir(parents=True)
    os.sched_setaffinity(0, manifest["affinity"])
    assert Path("/sys/fs/cgroup/memory.max").read_text().strip() == manifest["memory_max"]
    assert Path("/sys/fs/cgroup/cpu.max").read_text().strip() == manifest["cpu_max"]
    for name in ["mixed", "arrays"]:
        p = original / "binaries" / name
        assert hashlib.sha256(p.read_bytes()).hexdigest() == manifest["sha256"][f"target/release/{name}"]
    shutil.copy2(original / "measure", output / "measure")
    (output / "source").mkdir()
    for name in ["run.py", "followup_grouped.py"]:
        shutil.copy2(Path(__file__).parent / name, output / "source" / name)
    prior = [json.loads(x) for x in (original / "results.jsonl").read_text().splitlines()]
    failed = [x for x in prior if x.get("error") and not x["probe"]]
    bench.save(output / "manifest.json", dict(original_manifest=manifest,
               original_path=str(original), retained_failed_timings=[x["profile"]["name"] for x in failed],
               resource_policy="All grouped probes repeated; include a final quiescent file observation. Original incomplete probe retained.",
               binary_sha256=manifest["sha256"]["target/release/mixed"]))
    (output / "memory-before.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    phases = [(x["profile"], f"timing-retry-{x['profile']['name']}", False) for x in failed]
    phases += [(p, f"probe-0-{p['name']}", True) for p in manifest["profiles"]]
    rows = []
    for profile, stem, probe in phases:
        assert shutil.disk_usage(output).free >= 3 * 2**30, "fixed-budget preflight"
        p = dict(profile, binary_override=str(original / "binaries/mixed"))
        row = bench.invoke(p, output, stem, probe=probe)
        rows.append(row)
        with (output / "results.jsonl").open("a") as f:
            f.write(json.dumps(row) + "\n")
        print(stem, row.get("error", "OK"), f"{row['wall_seconds']:.3f}s", flush=True)
    (output / "memory-after.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    assert all(not x.get("error") for x in rows)


if __name__ == "__main__":
    main()
