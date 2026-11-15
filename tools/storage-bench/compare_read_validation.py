#!/usr/bin/env python3
"""Compare identical workloads across snapshots and independent read checks.

Builds must finish before invocation. Records failed attempts; never pools
sampled resource timings with uninstrumented timing observations.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import run as bench


def cases():
    grouped = bench.profiles("grouped")
    names = [
        "grouped-n1048576-g512-t131072-w1-r1-varlit-indexed",
        "grouped-n8388608-g512-t131072-w1-r8-varlit-indexed",
        "grouped-n8388608-g512-t131072-w4-r8-varlit-indexed",
        "grouped-n8388608-g8192-t131072-w4-r8-varlit-indexed",
        "grouped-follow-n8388608-g512-t131072-w4-r8-varlit-stream",
    ]
    selected = [next(p for p in grouped if p["name"] == name) for name in names]
    selected.append(dict(name="mixed-variable-p65536-w4-r8-varlit-indexed", kind="mixed",
        engine="varlit-indexed", config=dict(bench.BASE, records=(1 << 30)//65536,
        payload=65536, per_tx=256, producers=4, readers=8, lookups=True, telemetry=True)))
    for mode in ["native-owned", "native-mmap", "matrix-owned"]:
        selected.append(dict(name=f"array-{mode}-n1073741824-g1048576", kind="array", mode=mode,
            total=1 << 30, group=1 << 20, page=4096, mmap=0, points=4096, ranges=128,
            readers=0, warm_scan=mode == "native-mmap"))
    return selected


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--before", type=Path, required=True, help="Frozen pre-change mixed/arrays binary directory")
    ap.add_argument("--after", type=Path, required=True, help="Frozen new mixed/arrays binary directory")
    ap.add_argument("--pre-security", type=Path, required=True, help="Pre-PR main mixed binary using the same grouped fixture")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--smoke", action="store_true")
    args = ap.parse_args()
    assert args.repeats >= 3 or args.smoke
    cpus = sorted(os.sched_getaffinity(0))[:4]
    os.sched_setaffinity(0, cpus)
    memory = Path("/sys/fs/cgroup/memory.max").read_text().strip()
    cpu = Path("/sys/fs/cgroup/cpu.max").read_text().strip()
    assert memory != "max" and int(memory) <= 16 * 2**30
    quota, period = cpu.split()
    assert quota != "max" and int(quota) <= 4 * int(period)
    output = args.output.resolve(); output.mkdir(parents=True)
    subprocess.run(["cc", "-O2", str(bench.HERE/"measure.c"), "-o", str(output/"measure")], check=True)
    profiles = cases()
    if args.smoke:
        profiles = profiles[:1]
        for p in profiles:
            p["config"] = dict(p["config"], records=32, per_tx=8, readers=2)
    variants = [("before-a", args.before, "legacy"), ("after-default", args.after, "legacy"),
        ("full", args.after, "full"), ("crc-only", args.after, "crc-only"),
        ("structure-only", args.after, "structure-only"), ("trusted", args.after, "trusted"),
        ("before-b", args.before, "legacy")]
    grid = []
    for case in profiles:
        for variant, binaries, policy in variants:
            binary = binaries / ("mixed" if case["kind"] == "mixed" else "arrays")
            grid.append(dict(case, name=case["name"]+"--"+variant, workload=case["name"],
                variant=variant, read_validation=policy, binary_override=str(binary.resolve())))
        if case["kind"] == "mixed":
            grid.append(dict(case, name=case["name"]+"--pre-security", workload=case["name"],
                variant="pre-security", read_validation="legacy", binary_override=str(args.pre_security.resolve())))
    hashes = {p["binary_override"]: hashlib.sha256(Path(p["binary_override"]).read_bytes()).hexdigest() for p in grid}
    bench.save(output/"manifest.json", dict(profiles=grid, repetitions=args.repeats, smoke=args.smoke,
        affinity=cpus, memory_max=memory, cpu_max=cpu, disk_budget_gib=32, binaries=hashes,
        source_sha256={name:hashlib.sha256((bench.HERE/name).read_bytes()).hexdigest()
                       for name in ["compare_read_validation.py", "run.py", "measure.c"]},
        measurements="Balanced independent timing observations; separate qualified process-tree resource probes. Before A/B are identical binaries."))
    (output/"memory-before.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    rows = []
    phases = []
    # Prefix gates deliberately precede performance. All reader work is checked.
    for p in grid:
        if p["kind"] == "mixed": phases.append((p, "gate-"+p["name"], False, True))
    for repeat in range(args.repeats):
        order = grid if repeat % 2 == 0 else list(reversed(grid))
        for p in order: phases.append((p, f"timing-{repeat}-"+p["name"], False, False))
    for p in grid: phases.append((p, "probe-"+p["name"], True, False))
    for p, stem, probe, gate in phases:
        assert hashlib.sha256(Path(p["binary_override"]).read_bytes()).hexdigest() == hashes[p["binary_override"]]
        assert shutil.disk_usage(output).free >= 3 * 2**30, "fixed-budget preflight"
        row = bench.invoke(p, output, stem, probe=probe, gate=gate)
        rows.append(row)
        with (output/"results.jsonl").open("a") as f: f.write(json.dumps(row)+"\n")
        print(stem, row.get("error", "OK"), f"{row['wall_seconds']:.3f}s", flush=True)
        if row.get("error"):
            raise SystemExit("Failed attempt retained; diagnose before a new cohort")
    (output/"memory-after.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    bench.summarize(rows, output)


if __name__ == "__main__":
    main()
