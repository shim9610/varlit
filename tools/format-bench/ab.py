#!/usr/bin/env python3
"""Varlit A/B: the same harness built against two library revisions, run
alternately on one machine, so the comparison is relative to that machine.

usage: ab.py --base BIN --head BIN [--data-gb GB] [--repeats N]
             [--only SCENARIO,...] [--out DIR] [--summary FILE]

Each repeat runs every scenario once with the base binary and once with the
head binary, alternating which goes first, so slow drift on a shared machine
(a CI runner) falls on both. For every key metric of `report.py` it reports
the median, minimum and maximum of each side and the median change. A change
is flagged only when the two ranges do not overlap, the medians differ by
5 % or more and there were at least three runs: on a shared runner,
differences inside the run-to-run spread are noise. Nothing here fails a
build; it writes a table.

Only the Varlit engine runs, so the binaries may be built without the other
engines (`cargo build --release --no-default-features` leaves out HDF5).
Works on Linux and Windows; the memory cap `run.py` puts on `grid` needs
cgroups and is not applied here.
"""
import argparse, json, os, platform, shutil, statistics, subprocess, sys, tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from run import PLAN  # noqa: E402
from report import KEY  # noqa: E402

LOWER_IS_BETTER_HINTS = ("_s", "_ms", "frac", "bytes", "inconsistent", "torn", "corrupt")


def lower_is_better(metric):
    if metric in ("realtime_factor", "batches_per_s"):
        return False
    return any(h in metric for h in LOWER_IS_BETTER_HINTS)


def run_once(binary, label, scenario, env_extra, data_gb, work):
    d = Path(tempfile.mkdtemp(prefix=f"ab-{label}-", dir=work))
    env = {**os.environ, **env_extra, "FB_DATA_GB": str(data_gb)}
    try:
        p = subprocess.run([str(binary), "bench", scenario, "varlit", str(d)],
                           env=env, capture_output=True, text=True, timeout=3600)
        line = p.stdout.strip().splitlines()[-1] if p.stdout.strip() else ""
        if p.returncode != 0 or not line.startswith("{"):
            return {"_error": (p.stderr or p.stdout)[-400:]}
        return json.loads(line)
    finally:
        shutil.rmtree(d, ignore_errors=True)


def summarize(values):
    values = [v for v in values if isinstance(v, (int, float))]
    if not values:
        return None
    return statistics.median(values), min(values), max(values)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True, type=Path)
    ap.add_argument("--head", required=True, type=Path)
    ap.add_argument("--base-label", default="base")
    ap.add_argument("--head-label", default="head")
    ap.add_argument("--data-gb", type=float, default=1.0)
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--only", help="comma list of plan labels")
    ap.add_argument("--out", type=Path, default=HERE / "target/ab")
    ap.add_argument("--summary", type=Path, help="append the markdown table here")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    work = a.out / "work"
    work.mkdir(exist_ok=True)
    plan = [p for p in PLAN if not a.only or p[0] in a.only.split(",")]
    results = {}  # (label, side) -> list of metric dicts
    raw = (a.out / "results.jsonl").open("w")
    for rep in range(a.repeats):
        for label, scenario, env_extra, _engines in plan:
            sides = [("base", a.base), ("head", a.head)]
            if rep % 2:
                sides.reverse()
            for side, binary in sides:
                m = run_once(binary, side, scenario, env_extra, a.data_gb, work)
                results.setdefault((label, side), []).append(m)
                raw.write(json.dumps({"rep": rep, "label": label, "side": side, "m": m}) + "\n")
                raw.flush()
                status = "failed" if "_error" in m else "ok"
                print(f"rep{rep} {label:12s} {side} {status}", flush=True)
    raw.close()

    lines = [f"## Varlit A/B on {platform.system()} {platform.machine()}",
             "",
             f"{a.base_label} vs {a.head_label}, {a.repeats} alternating runs per side, "
             f"{a.data_gb:g} GB per scenario. Median (min to max); change is head vs base. "
             "Flagged only when the ranges do not overlap and the medians differ by 5 % or more.",
             ""]
    for label, _scenario, _env, _engines in plan:
        title, cols = KEY[label]
        lines += [f"### {title}", "",
                  "| metric | base | head | change | |", "|---|---|---|---|---|"]
        base = results.get((label, "base"), [])
        head = results.get((label, "head"), [])
        errors = [m["_error"] for m in base + head if "_error" in m]
        for name, metric, scale, digits in cols:
            if metric is None:
                continue
            b = summarize([m.get(metric) for m in base if "_error" not in m])
            h = summarize([m.get(metric) for m in head if "_error" not in m])
            if not b or not h:
                lines.append(f"| {name} | - | - | | |")
                continue
            f = lambda s: f"{s[0]*scale:.{digits}f} ({s[1]*scale:.{digits}f} to {s[2]*scale:.{digits}f})"
            change = (h[0] - b[0]) / b[0] * 100 if b[0] else 0.0
            flag = ""
            # Flagged only with enough runs to have a spread, ranges that do
            # not overlap, and a median change worth the name.
            if a.repeats >= 3 and abs(change) >= 5 and (h[1] > b[2] or h[2] < b[1]):
                worse = (h[0] > b[0]) == lower_is_better(metric)
                flag = "regression" if worse else "improvement"
            lines.append(f"| {name} | {f(b)} | {f(h)} | {change:+.1f}% | {flag} |")
        if errors:
            lines += ["", f"{len(errors)} run(s) failed; first error: `{errors[0].strip()[:200]}`"]
        lines.append("")
    text = "\n".join(lines) + "\n"
    (a.out / "summary.md").write_text(text)
    if a.summary:
        with a.summary.open("a") as s:
            s.write(text)
    print(text)
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
