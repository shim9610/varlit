#!/usr/bin/env python3
"""Render one Markdown table per scenario from a run.py summary.json, with
the metrics that decide that scenario (the objective first).

usage: report.py RUN_DIR
"""
import json, sys
from pathlib import Path

S = json.loads((Path(sys.argv[1]) / "summary.json").read_text())

# label -> (title, [(column, metric, scale, digits)]). Times are seconds
# unless the column says ms; MB is allocated disk space.
KEY = {
    "grid": ("grid: 6-D f32 array, growing time axis, 4 GiB memory cap (HDF5's case)", [
        ("all reads s", "reads_s", 1, 2), ("time series ms", "line0_s", 1e3, 0),
        ("16 sub-cubes ms", "cubes_s", 1e3, 0), ("ensemble means ms", "ensmean_s", 1e3, 0),
        ("write s", "write_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
    "oltp": ("oltp: accounts with dense ids, durable transfers (SQLite's case)", [
        ("1000 txn s", "txn_s", 1, 2), ("txn p99 ms", "txn_p99_ms", 1, 2),
        ("20k cold points s", "cold_points_s", 1, 2), ("20k warm points s", "warm_points_s", 1, 3),
        ("200 branch sums s", "warm_branch_s", 1, 3), ("load s", "load_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
    "oltp-sparse": ("oltp-sparse: the same accounts with sparse 64-bit keys", [
        ("1000 txn s", "txn_s", 1, 2), ("txn p99 ms", "txn_p99_ms", 1, 2),
        ("20k cold points s", "cold_points_s", 1, 2), ("20k warm points s", "warm_points_s", 1, 3),
        ("200 branch sums s", "warm_branch_s", 1, 3), ("load s", "load_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
    "analytics": ("analytics: append-only 12-column table, cold queries (Parquet's case)", [
        ("Q1 group-by s", "cold_q1_s", 1, 3), ("Q2 20 ranges s", "cold_q2_s", 1, 3),
        ("write s", "write_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
    "mixed": ("mixed: three typed streams flat out, four live followers (Varlit's case)", [
        ("x real time", "realtime_factor", 1, 2), ("publish p99 ms", "publish_p99_ms", 1, 2),
        ("inconsistent views", "all_inconsistent", 1, 0), ("late reader catch-up s", "catchup_s", 1, 2),
        ("write s", "write_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
    "mixed-live": ("mixed-live: the same in real time, 10 ms freshness, every window durable", [
        ("late windows %", "all_late_frac", 100, 2), ("B-only follower late %", "b_late_frac", 100, 2),
        ("inconsistent views", "all_inconsistent", 1, 0), ("publish p99 ms", "publish_p99_ms", 1, 2),
        ("MB", "bytes", 1e-6, 0)]),
    "cells": ("cells: fixed 1 KiB cells updated in place, 64 per atomic update, four followers", [
        ("updates/s", "batches_per_s", 1, 0), ("follower lag p99 ms", "lag_p99_ms", 1, 2),
        ("torn + corrupt reads", None, 1, 0), ("growth MB", "growth_bytes", 1e-6, 0),
        ("initial load s", "init_s", 1, 1), ("MB", "bytes", 1e-6, 0)]),
}


def cell(m, metric, scale, digits):
    if metric is None:
        v = m.get("torn", 0) + m.get("corrupt", 0)
    elif metric not in m:
        return "-"
    else:
        v = m[metric] * scale
    return f"{v:.{digits}f}"


out = []
for label, (title, cols) in KEY.items():
    rows = [(k.split("/", 1)[1], m) for k, m in S.items() if k.split("/", 1)[0] == label]
    if not rows:
        continue
    gb = next((m.get("data_gb") for _, m in rows if "data_gb" in m), None)
    out.append(f"### {title}" + (f" ({gb:g} GB)" if gb else "") + "\n")
    out.append("| engine | " + " | ".join(c for c, *_ in cols) + " |")
    out.append("|---|" + "---|" * len(cols))
    for fmt, m in rows:
        if "failed" in m:
            out.append(f"| {fmt} | failed: {m['failed']} |" + " |" * (len(cols) - 1))
            continue
        out.append(f"| {fmt} | " + " | ".join(cell(m, *c[1:]) for c in cols) + " |")
    out.append("")
print("\n".join(out))
