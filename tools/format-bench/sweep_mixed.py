#!/usr/bin/env python3
"""Mixed-record stream scenario across application requirements, disk-gated.

usage: sweep_mixed.py OUT_DIR [REPEATS]

Axes are requirements the application states, not sync schedules:
  freshness  FB_FRESH_MS  followers must see each window within this time
  loss bound FB_LOSS_MS   data a power loss may lose (0 = all visible data durable)
  readers    FB_ALL_READERS  followers reading every type (plus one B-only)
mode max:   producers run flat out, 200 windows (2 s of data) -> realtime factor
mode paced: real-time emission, 100 windows (1 s)              -> lag, deadline misses
"""
import json, os, shutil, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import BIN, control  # noqa: E402

FMTS = ["varlit", "varlit-native", "sqlite", "hdf5", "parquet"]
FRESH = [10, 100]
LOSS = [0, 1000]
READERS = [1, 4]
MODES = {"max": ("200", None), "paced": ("100", "1")}


def main():
    out = Path(sys.argv[1]).resolve()
    reps = int(sys.argv[2]) if len(sys.argv) > 2 else 2
    out.mkdir(parents=True, exist_ok=True)
    results, rep = [], 0
    while rep < reps:
        while (t := control(out)) >= 0.7:
            print(f"control {t:.2f}s, waiting", flush=True)
            time.sleep(60)
        batch = []
        for nr in READERS:
            for mode, (wins, pace) in MODES.items():
                for fresh in FRESH:
                    for loss in LOSS:
                        for fmt in FMTS:
                            d = out / "run"
                            shutil.rmtree(d, ignore_errors=True)
                            env = dict(os.environ, FB_WINDOWS=wins, FB_FRESH_MS=str(fresh),
                                       FB_LOSS_MS=str(loss), FB_ALL_READERS=str(nr))
                            env.pop("FB_PACE", None)
                            if pace:
                                env["FB_PACE"] = pace
                            try:
                                p = subprocess.run([str(BIN), "bench", "mixed", fmt, str(d)],
                                                   capture_output=True, text=True, env=env, timeout=300)
                            except subprocess.TimeoutExpired:
                                print(f"TIMEOUT {fmt} {mode} fresh={fresh} loss={loss} r={nr}", flush=True)
                                continue
                            finally:
                                shutil.rmtree(d, ignore_errors=True)
                            if p.returncode != 0:
                                print(f"FAIL {fmt} {mode} fresh={fresh} loss={loss} r={nr}\n{p.stderr[-1500:]}", flush=True)
                                continue
                            r = json.loads(p.stdout.strip().splitlines()[-1])
                            r.update(mode=mode, rep=rep, readers=nr)
                            batch.append(r)
                            print(f"rep{rep} r={nr} {mode:5} fresh={fresh:3} loss={loss:4} {fmt:14} "
                                  f"rt={r['realtime_factor']:.2f} lag={r['all_lag_p50_ms']:.1f}/{r['all_lag_p99_ms']:.1f}ms "
                                  f"late={r['all_late_frac']:.3f} incons={r['all_inconsistent']}", flush=True)
        t = control(out)
        if t >= 0.7:
            print(f"disk degraded during rep{rep} ({t:.2f}s); redoing", flush=True)
            continue
        results.extend(batch)
        rep += 1
    (out / "results.json").write_text(json.dumps(results, indent=1))


if __name__ == "__main__":
    main()
