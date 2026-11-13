#!/usr/bin/env python3
"""Run every scenario x format N times in fresh directories; report medians.

usage: run.py [--repeats N] [--out DIR] [--only SCENARIO]
DIR must be on a real disk (fsync costs matter); default target/runs.
"""
import argparse, json, shutil, statistics, subprocess, sys, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "target/release/varlit-format-bench"
# Final plan: per scenario, the recommended Varlit configuration(s) for that
# workload (see README) and the three other formats.
PLAN = {
    "grid": ["varlit", "varlit+trusted", "sqlite", "hdf5", "parquet"],
    "oltp": ["varlit-append", "varlit-append+trusted", "varlit-matrix-packed+trusted", "sqlite", "hdf5", "parquet"],
    "analytics": ["varlit", "varlit+trusted", "sqlite", "hdf5", "parquet"],
}

# Scenarios run inside a memory-capped cgroup (page cache counts toward it).
MEMCAP = {"grid": "4G"}

def control(out):
    """Disk-health probe: 1000 x (4 KiB write + fdatasync); healthy ~0.48 s."""
    d = out / "control"
    p = subprocess.run([str(BIN), "probe", str(d)], capture_output=True, text=True, check=True)
    shutil.rmtree(d, ignore_errors=True)
    return float(p.stdout.strip())

def wait_healthy(out, limit=0.7):
    while True:
        t = control(out)
        print(f"control txn_s={t:.2f}", flush=True)
        if t < limit:
            return
        time.sleep(60)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--out", default=str(HERE / "target/runs"))
    ap.add_argument("--only")
    ap.add_argument("--formats", help="comma list; run only these format names")
    ap.add_argument("--gate", action="store_true", help="wait for a healthy disk before each repeat; redo a repeat if the disk degraded during it")
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    results = []
    # Interleave formats within each repeat so drift affects all equally.
    rep = 0
    while rep < a.repeats:
        if a.gate:
            wait_healthy(out)
        batch = []
        for scen, fmts in PLAN.items():
            if a.only and scen != a.only:
                continue
            for fmt in fmts:
                if a.formats and fmt not in a.formats.split(","):
                    continue
                d = out / f"{scen}-{fmt.replace('+', '_')}-{rep}"
                shutil.rmtree(d, ignore_errors=True)
                t = time.time()
                cmd = [str(BIN), "bench", scen, fmt, str(d)]
                if scen in MEMCAP:
                    # Linux harness only: the data must exceed the memory the
                    # engine may use, page cache included.
                    cmd = ["systemd-run", "--user", "--scope", "-q", "-p", f"MemoryMax={MEMCAP[scen]}",
                           "-p", "MemorySwapMax=0"] + cmd
                p = subprocess.run(cmd, capture_output=True, text=True)
                shutil.rmtree(d, ignore_errors=True)
                if p.returncode != 0:
                    print(f"FAIL {scen} {fmt} rep{rep}\n{p.stderr[-2000:]}", file=sys.stderr)
                    batch.append({"scenario": scen, "format": fmt, "rep": rep, "failed": True})
                    continue
                r = json.loads(p.stdout.strip().splitlines()[-1])
                r["rep"] = rep
                batch.append(r)
                print(f"rep{rep} {scen:9} {fmt:30} {time.time() - t:6.1f}s", flush=True)
        if a.gate:
            t = control(out)
            if t >= 0.7:
                print(f"disk degraded during rep{rep} (control {t:.2f}s); redoing", flush=True)
                continue
        results.extend(batch)
        rep += 1
    (out / "results.json").write_text(json.dumps(results, indent=1))
    # Median per metric.
    med = {}
    for r in results:
        if r.get("failed"):
            continue
        k = (r["scenario"], r["format"])
        for m, v in r.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool) and m != "rep":
                med.setdefault(k, {}).setdefault(m, []).append(v)
    summary = {f"{s}/{f}": {m: statistics.median(v) for m, v in ms.items()} for (s, f), ms in med.items()}
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    for k, ms in summary.items():
        print(k, json.dumps({m: round(v, 4) for m, v in ms.items()}))

if __name__ == "__main__":
    main()
