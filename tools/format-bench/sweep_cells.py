#!/usr/bin/env python3
"""Cells scenario (in-place cell updates + followers), disk-gated.

usage: sweep_cells.py OUT_DIR [REPEATS]
"""
import json, os, shutil, statistics, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import BIN, control  # noqa: E402

FMTS = ["varlit-matrix", "varlit-native", "sqlite", "hdf5", "parquet"]
LOSS = [0, 1000]


def main():
    out = Path(sys.argv[1]).resolve()
    reps = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    out.mkdir(parents=True, exist_ok=True)
    results, rep = [], 0
    while rep < reps:
        while (t := control(out)) >= 0.7:
            print(f"control {t:.2f}s, waiting", flush=True)
            time.sleep(60)
        batch = []
        for loss in LOSS:
            for fmt in FMTS:
                d = out / "run"
                shutil.rmtree(d, ignore_errors=True)
                d.mkdir()
                env = dict(os.environ, FB_LOSS_MS=str(loss))
                try:
                    p = subprocess.run([str(BIN), "bench", "cells", fmt, str(d)],
                                       capture_output=True, text=True, env=env, timeout=600)
                except subprocess.TimeoutExpired:
                    print(f"TIMEOUT {fmt} loss={loss}", flush=True)
                    batch.append({"format": fmt, "loss_ms": loss, "rep": rep, "failed": "timeout"})
                    continue
                finally:
                    shutil.rmtree(d, ignore_errors=True)
                if p.returncode != 0:
                    msg = p.stderr.strip().splitlines()[-1] if p.stderr.strip() else "error"
                    print(f"FAIL {fmt} loss={loss}: {msg}", flush=True)
                    batch.append({"format": fmt, "loss_ms": loss, "rep": rep, "failed": msg,
                                  "stderr": p.stderr[-4000:]})
                    continue
                r = json.loads(p.stdout.strip().splitlines()[-1])
                r.update(rep=rep)
                fc = [l for l in p.stderr.splitlines() if l.startswith("first corrupt read:")]
                if fc:
                    r["first_corrupt"] = fc[0]
                batch.append(r)
                print(f"rep{rep} loss={loss:4} {fmt:14} {r['batches_per_s']:7.0f} batch/s "
                      f"lag={r['lag_p50_ms']:.2f}/{r['lag_p99_ms']:.2f}ms torn={r['torn']} corrupt={r.get('corrupt', 0)} "
                      f"MB={r['bytes'] / 1e6:.0f}", flush=True)
        t = control(out)
        if t >= 0.7:
            print(f"disk degraded during rep{rep} ({t:.2f}s); redoing", flush=True)
            continue
        results.extend(batch)
        rep += 1
    (out / "results.json").write_text(json.dumps(results, indent=1))
    med, fails = {}, {}
    for r in results:
        k = f"{r['format']}/{r['loss_ms']}"
        if r.get("failed"):
            fails.setdefault(k, []).append(r["failed"])
            continue
        for m, v in r.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                med.setdefault(k, {}).setdefault(m, []).append(v)
    summary = {}
    for k in sorted(set(med) | set(fails), key=lambda k: list(dict.fromkeys(
            f"{r['format']}/{r['loss_ms']}" for r in results)).index(k)):
        row = {m: (sum(x) if m in ("torn", "corrupt") else statistics.median(x)) for m, x in med.get(k, {}).items()}
        if k in fails:
            row.update(failed_runs=len(fails[k]), failed=fails[k][0])
        summary[k] = row
    (out / "summary.json").write_text(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
