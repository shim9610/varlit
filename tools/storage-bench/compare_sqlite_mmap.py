#!/usr/bin/env python3
"""Compare SQLite mmap settings with static and append-only Varlit readers."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import time

import run as bench


def profiles(smoke=False):
    grid = []
    for live in (False, True):
        sizes = [32 << 20] if smoke else ([1 << 30] if live else [128 << 20, 1 << 30])
        readers = [2] if smoke else ([1, 8] if live else [8])
        for total in sizes:
            for group in (65536, 1 << 20):
                for count in readers:
                    workload = f"{'live' if live else 'static'}-n{total}-g{group}-r{count}"
                    common = dict(kind="array", workload=workload, total=total, group=group,
                                  page=65536, points=0 if live else 4096,
                                  ranges=0 if live else 128, readers=count, mmap=0,
                                  live=live, warm_scan=not live)
                    for mode in ("native-view", "native-mmap"):
                        for policy in ("full", "crc-only"):
                            variant = f"varlit-{mode}-{policy}"
                            grid.append(dict(common, name=f"{workload}--{variant}",
                                             mode=mode, variant=variant, read_validation=policy))
                    for limit in (0, 1 << 30, 2 << 30):
                        variant = f"sqlite-m{limit}"
                        grid.append(dict(common, name=f"{workload}--{variant}",
                                         mode="sql-chunks", variant=variant, mmap=limit))
    for group in (512, 8192):
        items = 32768 if smoke else 8 << 20
        producers, readers = (1, 2) if smoke else (4, 8)
        publication = 8192 if smoke else 131072
        workload = f"grouped-n{items}-g{group}-t{publication}-w{producers}-r{readers}"
        config = dict(bench.BASE, records=items // group, payload=group * 128,
                      group_items=group, per_tx=publication // group, variable=False,
                      producers=producers, readers=readers, lookups=True, telemetry=True,
                      page=65536)
        for policy in ("full", "crc-only"):
            variant = f"varlit-indexed-{policy}"
            grid.append(dict(name=f"{workload}--{variant}", kind="mixed", workload=workload,
                             variant=variant, engine="varlit-indexed", config=dict(config),
                             read_validation=policy))
        for limit in (0, 1 << 30, 2 << 30):
            variant = f"sqlite-m{limit}"
            grid.append(dict(name=f"{workload}--{variant}", kind="mixed", workload=workload,
                             variant=variant, engine="sqlite", config=dict(config, mmap=limit)))
    return grid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary-dir", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--repeats", type=int, default=7)
    ap.add_argument("--smoke", action="store_true")
    ap.add_argument("--live-snapshots", action="store_true", help="Matched live-only study including whole-prefix mmap snapshots")
    ap.add_argument("--filter", default="")
    args = ap.parse_args()
    repeats = 1 if args.smoke else args.repeats
    assert repeats >= 3 or args.smoke
    cpus = sorted(os.sched_getaffinity(0))[:4]
    os.sched_setaffinity(0, cpus)
    memory = Path("/sys/fs/cgroup/memory.max").read_text().strip()
    quota, period = Path("/sys/fs/cgroup/cpu.max").read_text().split()
    assert memory != "max" and int(memory) <= 16 * 2**30
    assert quota != "max" and int(quota) <= 4 * int(period)
    sqlite = ctypes.CDLL("libsqlite3.so.0")
    sqlite.sqlite3_libversion.restype = ctypes.c_char_p
    sqlite_version = sqlite.sqlite3_libversion().decode()
    sqlite_threadsafe = sqlite.sqlite3_threadsafe()
    assert sqlite_threadsafe != 0, "static reader-thread cases require a thread-safe SQLite build"
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    assert shutil.disk_usage(output).total <= 32 * 2**30
    subprocess.run(["cc", "-O2", str(bench.HERE / "measure.c"), "-o", str(output / "measure")], check=True)
    selected = profiles(args.smoke)
    if args.live_snapshots:
        selected = [p for p in selected if p.get("live")]
        for p in list(selected):
            if p.get("mode") == "native-mmap":
                variant = p["variant"].replace("native-mmap-", "native-mmap-snapshot-")
                selected.append(dict(p, mode="native-mmap-snapshot", variant=variant,
                                     name=f"{p['workload']}--{variant}"))
    selected = [p for p in selected if args.filter in p["name"]]
    assert selected
    hashes = {}
    for profile in selected:
        binary = (args.binary_dir / ("mixed" if profile["kind"] == "mixed" else "arrays")).resolve()
        profile["binary_override"] = str(binary)
        hashes[str(binary)] = hashlib.sha256(binary.read_bytes()).hexdigest()
    source = output / "source"
    source.mkdir()
    source_files = [bench.HERE / n for n in ["Cargo.toml", "Cargo.lock", "build.rs", "run.py", "compare_sqlite_mmap.py", "measure.c", "README.md"]]
    source_files += list(bench.HERE.glob("src/**/*.rs"))
    for path in source_files:
        dest = source / path.relative_to(bench.HERE)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, dest)
    (output / "worktree.patch").write_bytes(subprocess.check_output(["git", "diff", "--binary"], cwd=bench.REPO))
    runtime = {str(p.relative_to(bench.REPO)): hashlib.sha256(p.read_bytes()).hexdigest()
               for p in (bench.REPO / "crates").glob("*/src/**/*.rs")}
    bench.save(output / "manifest.json", dict(
        profiles=selected, repetitions=repeats, smoke=args.smoke, live_snapshots=args.live_snapshots, random_seed=9610,
        affinity=cpus, memory_max=memory, cpu_max=f"{quota} {period}", disk_budget_gib=32,
        started_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        runtime_head=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=bench.REPO, text=True).strip(),
        binaries=hashes, runtime_source_sha256=runtime,
        sqlite_version=sqlite_version, sqlite_threadsafe=sqlite_threadsafe,
        benchmark_source_sha256={str(p.relative_to(bench.HERE)): hashlib.sha256(p.read_bytes()).hexdigest() for p in source_files},
        cache_policy="Fresh files; writes warm the OS cache; no privileged eviction.",
        sqlite="64 KiB pages and 8 MiB connection cache; WAL/FULL; mmap requests 0/1/2 GiB, with effective settings and actual mapped primary extents recorded.",
        live_mmap="Independent reader processes map only confirmed immutable records; writer never truncates/replaces/rewrites them. Window mode follows new tail entries without copying the whole index. Snapshot mode copies the captured index at each rebuild and records that cost.",
        measurements="Fixed complete randomized workload blocks; independent timing rounds, separate process-tree resource probes and untimed publication gates. No cross-cohort pooling."))
    (output / "memory-before.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    phases = [(p, f"gate-{p['name']}", False, True) for p in selected if p["kind"] == "mixed"]
    rng = random.Random(9610)
    workloads = sorted({p["workload"] for p in selected})
    for repeat in range(repeats):
        order = workloads.copy()
        rng.shuffle(order)
        for workload in order:
            variants = [p for p in selected if p["workload"] == workload]
            rng.shuffle(variants)
            phases.extend((p, f"timing-{repeat}-{p['name']}", False, False) for p in variants)
    order = selected.copy()
    rng.shuffle(order)
    phases.extend((p, f"probe-{p['name']}", True, False) for p in order)
    rows = []
    for index, (profile, stem, probe, gate) in enumerate(phases, 1):
        assert shutil.disk_usage(output).free >= 3 * 2**30, "fixed filesystem preflight"
        row = bench.invoke(profile, output, stem, probe=probe, gate=gate)
        rows.append(row)
        with (output / "results.jsonl").open("a") as f:
            f.write(json.dumps(row) + "\n")
        print(f"{index}/{len(phases)} {stem} {row.get('error', 'OK')} {row['wall_seconds']:.3f}s", flush=True)
        if row.get("error"):
            raise SystemExit("Failed attempt retained; diagnose before starting another cohort")
        if probe and profile["kind"] == "mixed":
            assert row["sampled_peaks"]["sampled_processes"] >= profile["config"]["readers"] + 2
        if probe and profile.get("live"):
            assert row["sampled_peaks"]["sampled_processes"] >= profile["readers"] + 2
        if probe and profile.get("mode") == "sql-chunks" and profile["mmap"] == 0:
            assert row["sampled_peaks"]["primary_mmap_virtual_bytes"] == 0
    for path, expected in hashes.items():
        assert hashlib.sha256(Path(path).read_bytes()).hexdigest() == expected
    after = Path("/sys/fs/cgroup/memory.events").read_text()
    (output / "memory-after.txt").write_text(after)
    before = dict(line.split() for line in (output / "memory-before.txt").read_text().splitlines())
    events = dict(line.split() for line in after.splitlines())
    assert all(events[k] == before[k] for k in ("oom", "oom_kill")), "OOM event during the fixed-budget campaign"
    bench.summarize(rows, output)


if __name__ == "__main__":
    main()
