#!/usr/bin/env python3
"""Situation-only sweep: each engine tunes its own storage configuration.

usage: sweep.py SCENARIO OUT_DIR [--repeats N] [--engines a,b] [--variants JSON]
                [--objective METRIC:max|min] [--env K=V ...]

For every variant (a dict of FB_* requirement settings) and engine:
  1. tuning: run every candidate the engine's adapter offers once;
  2. pick the best candidate by the scenario objective (failed runs excluded;
     no successful candidate = FAIL);
  3. measure the chosen candidate REPEATS times, disk-gated. A candidate that
     fails during measurement (crash, timeout, killed under memory pressure)
     is rejected with its reason and the next-best tuned candidate is measured
     instead; the rejections are reported. Each run waits for a healthy disk.
Every run executes inside a 4 GiB memory cgroup (page cache included) and its
data directory is deleted as soon as the metrics are read. The effort of each
chosen adapter (non-comment lines, structures the application had to build) is
recorded next to the results.
"""
import argparse, json, os, shutil, statistics, subprocess, sys, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "target/release/varlit-format-bench"
MEMCAP = "4G"
GATE = 0.7  # probe seconds; healthy disk ~0.48 s
OBJECTIVE = {"cells": ("batches_per_s", "max"), "oltp": ("total_s", "min"), "analytics": ("query_s", "min"),
             "mixed": ("realtime_factor", "max"), "grid": ("reads_s", "min")}
# Requirement checks on measured rows: a violated requirement is a FAIL even
# when the run completed (the numbers stay for reference).
REQUIRE = {
    "cells": lambda r: "atomic updates violated (torn/corrupt reads)" if r.get("torn") or r.get("corrupt") else None,
    "mixed": lambda r: ("types not published atomically (inconsistent observations)" if r.get("all_inconsistent")
                        else f"freshness missed ({r['all_late_frac'] * 100:.2f} % of windows late)"
                        if r.get("all_late_frac", 0) > 0 else None),
}
# Mixed: tune on a short flat-out run (throughput headroom), measure a
# 10-minute real-time run with four live followers and a catch-up reader.
TUNE_ENV = {"mixed": {"FB_WINDOWS": "3000", "FB_ALL_READERS": "4"}}
MEASURE_ENV = {"mixed": {"FB_WINDOWS": "60000", "FB_PACE": "1", "FB_ALL_READERS": "4"}}
VARIANTS = {"cells": [{"FB_LOSS_MS": "0"}, {"FB_LOSS_MS": "1000"}],
            "oltp": [{"FB_DIST": "uniform"}, {"FB_DIST": "zipf"}],
            "analytics": [{}],
            "mixed": [{"FB_FRESH_MS": "10", "FB_LOSS_MS": "0"}, {"FB_FRESH_MS": "100", "FB_LOSS_MS": "1000"}],
            "grid": [{}]}
ENGINES = {"cells": ["varlit", "sqlite", "hdf5", "parquet"],
           "oltp": ["varlit", "sqlite", "hdf5", "parquet"],
           "analytics": ["varlit", "sqlite", "hdf5", "parquet"],
           "mixed": ["varlit", "sqlite", "hdf5", "parquet"],
           "grid": ["varlit", "sqlite", "hdf5", "parquet"]}


def probe(out):
    p = subprocess.run([str(BIN), "probe", str(out / "probe")], capture_output=True, text=True, check=True)
    shutil.rmtree(out / "probe", ignore_errors=True)
    return float(p.stdout.strip())


def wait_healthy(out, log):
    while (t := probe(out)) >= GATE:
        log(f"probe {t:.2f}s, waiting")
        time.sleep(60)


def run_one(scen, engine, cand, env, out, timeout):
    d = out / "run"
    shutil.rmtree(d, ignore_errors=True)
    unit = f"fb-{os.getpid()}-{int(time.time() * 1000)}"
    cmd = ["systemd-run", "--user", "--scope", "-q", f"--unit={unit}", "-p", f"MemoryMax={MEMCAP}", "-p", "MemorySwapMax=0",
           str(BIN), "bench", scen, engine, str(d)]
    # Temporary files go to the same disk, never to a memory-backed /tmp.
    tmp = out / "tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir()
    e = dict(os.environ, **env, FB_CAND=cand, TMPDIR=str(tmp))
    t = time.time()
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, env=e, timeout=timeout)
    except subprocess.TimeoutExpired:
        subprocess.run(["systemctl", "--user", "stop", f"{unit}.scope"], check=False)
        return {"failed": f"timeout {timeout}s"}
    finally:
        shutil.rmtree(d, ignore_errors=True)
        shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        lines = [l for l in p.stderr.strip().splitlines() if l.strip()]
        why = lines[-1] if lines else f"exit {p.returncode}"
        res = subprocess.run(["systemctl", "--user", "show", "-p", "Result", "--value", f"{unit}.scope"],
                             capture_output=True, text=True).stdout.strip()
        if res == "oom-kill":
            why = f"killed under memory pressure (cap {MEMCAP}, oom-kill)"
        elif p.returncode in (-9, 137):
            why = f"killed (signal 9): {why}"
        subprocess.run(["systemctl", "--user", "reset-failed", f"{unit}.scope"], capture_output=True)
        return {"failed": why, "stderr": p.stderr[-3000:]}
    r = json.loads(p.stdout.strip().splitlines()[-1])
    fc = [l for l in p.stderr.splitlines() if l.startswith("first corrupt read:")]
    if fc:
        r["first_corrupt"] = fc[0]
    r["wall_s"] = time.time() - t
    return r


def effort(scen, engine, cand):
    fs = subprocess.run([str(BIN), "adapter", scen, engine, cand], capture_output=True, text=True).stdout.strip()
    if not fs:
        return {"files": [], "lines": 0, "app_built": []}
    lines, built = 0, []
    for f in fs.split(","):
        src = (HERE / f).read_text().splitlines()
        lines += len([l for l in src if l.strip() and not l.strip().startswith("//")])
        built += [l.split("app-built:", 1)[1].strip() for l in src if "app-built:" in l]
    return {"files": fs.split(","), "lines": lines, "app_built": built}


def better(a, b, metric, sense):
    return b is None or (a[metric] > b[metric] if sense == "max" else a[metric] < b[metric])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario")
    ap.add_argument("out")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--engines")
    ap.add_argument("--variants")
    ap.add_argument("--objective")
    ap.add_argument("--timeout", type=int, default=3600)
    ap.add_argument("--env", action="append", default=[], help="extra K=V for every run (e.g. FB_SMALL=1)")
    a = ap.parse_args()
    scen = a.scenario
    out = Path(a.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    logf = open(out / "log.txt", "a")

    def log(s):
        print(s, flush=True)
        logf.write(s + "\n")
        logf.flush()

    metric, sense = (a.objective.split(":") if a.objective else OBJECTIVE[scen])
    variants = json.loads(a.variants) if a.variants else VARIANTS[scen]
    engines = a.engines.split(",") if a.engines else ENGINES[scen]
    extra = dict(kv.split("=", 1) for kv in a.env)
    state_f = out / "state.json"
    st = json.loads(state_f.read_text()) if state_f.exists() else {"tuning": [], "measured": []}

    def save():
        state_f.write_text(json.dumps(st, indent=1))

    def vkey(v):
        return ",".join(f"{k}={x}" for k, x in sorted(v.items()))

    st.setdefault("rejected", {})
    keys = [(v, eng) for v in variants for eng in engines]

    def k_of(v, eng):
        return f"{vkey(v)}/{eng}"

    def tuned(v, eng):
        return [r for r in st["tuning"] if r["variant"] == vkey(v) and r["engine"] == eng]

    def chosen(v, eng):
        out_ = {x["cand"] for x in st["rejected"].get(k_of(v, eng), [])}
        best = None
        for r in tuned(v, eng):
            if not r.get("failed") and r["cand"] not in out_ and better(r, best, metric, sense):
                best = r
        return best["cand"] if best else None

    def reject(v, eng, cand, why):
        st["rejected"].setdefault(k_of(v, eng), []).append({"cand": cand, "why": why})
        st["measured"] = [r for r in st["measured"] if not (r["variant"] == vkey(v) and r["engine"] == eng)]
        log(f"reject {vkey(v)} {eng}/{cand}: {why}; next: {chosen(v, eng) or 'FAIL'}")

    # Older states kept failed measured rows: turn them into rejections.
    for r in [r for r in st["measured"] if r.get("failed")]:
        v = next(v for v in variants if vkey(v) == r["variant"])
        if r["cand"] not in {x["cand"] for x in st["rejected"].get(k_of(v, r["engine"]), [])}:
            reject(v, r["engine"], r["cand"], r["failed"])
    st["measured"] = [r for r in st["measured"] if not r.get("failed")]
    save()

    # 1. tuning
    for v, eng in keys:
        cands = json.loads(subprocess.run([str(BIN), "candidates", scen, eng], capture_output=True,
                                          text=True, check=True).stdout)
        for c in cands:
            prev = [r for r in tuned(v, eng) if r["cand"] == c]
            if prev:
                r = prev[0]
            else:
                wait_healthy(out, log)
                r = run_one(scen, eng, c, {**TUNE_ENV.get(scen, {}), **extra, **v}, out, a.timeout)
                r.update(variant=vkey(v), engine=eng, cand=c)
                st["tuning"].append(r)
                save()
            log(f"tune {vkey(v)} {eng}/{c}: " + (r.get("failed") or f"{metric}={r[metric]:.4g}"))
        log(f"chosen {vkey(v)} {eng}: {chosen(v, eng) or 'FAIL'}")

    # 2. measurement: rounds over every (variant, engine) still short of
    # REPEATS successful runs of its current choice. Every run starts on a
    # healthy disk (gate before the run; a large run degrading the disk after
    # it finished does not void it).
    def done(v, eng):
        return len([r for r in st["measured"] if r["variant"] == vkey(v) and r["engine"] == eng])

    while need := [(v, eng) for v, eng in keys if chosen(v, eng) and done(v, eng) < a.repeats]:
        for v, eng in need:
            c = chosen(v, eng)
            wait_healthy(out, log)
            r = run_one(scen, eng, c, {**MEASURE_ENV.get(scen, {}), **extra, **v}, out, a.timeout)
            r.update(variant=vkey(v), engine=eng, cand=c, rep=done(v, eng))
            log(f"rep{r['rep']} {vkey(v)} {eng}/{c}: " + (r.get("failed") or f"{metric}={r[metric]:.4g}"))
            if r.get("failed"):
                reject(v, eng, c, r["failed"])
            else:
                st["measured"].append(r)
            save()

    # 3. summary
    summary = {}
    for v, eng in keys:
        k = k_of(v, eng)
        rej = st["rejected"].get(k, [])
        c = chosen(v, eng)
        if c is None:
            fails = [f"{r['cand']}: {r['failed']}" for r in tuned(v, eng) if r.get("failed")]
            fails += [f"{x['cand']}: {x['why']}" for x in rej]
            summary[k] = {"failed": fails or ["no candidate"], "effort": effort(scen, eng, "-")}
            continue
        ok = [r for r in st["measured"] if r["variant"] == vkey(v) and r["engine"] == eng]
        row = {"candidate": c, "rejected": rej, "effort": effort(scen, eng, c)}
        for m in {m for r in ok for m, x in r.items() if isinstance(x, (int, float)) and not isinstance(x, bool)}:
            xs = [r[m] for r in ok if m in r]
            row[m] = sum(xs) if m in ("torn", "corrupt") else statistics.median(xs)
        if scen in REQUIRE:
            why = next((REQUIRE[scen](r) for r in ok if REQUIRE[scen](r)), None)
            if why:
                row["requirement_failed"] = why
        row["tuning"] = {r["cand"]: (r.get("failed") or r.get(metric)) for r in tuned(v, eng)}
        summary[k] = row
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    log("done")


if __name__ == "__main__":
    main()
