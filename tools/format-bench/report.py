#!/usr/bin/env python3
"""Render the README results tables from a run.py summary.json.

usage: report.py RUN_DIR   (prints Markdown)
"""
import json, sys
from pathlib import Path

S = json.loads((Path(sys.argv[1]) / "summary.json").read_text())


def g(scen, fmt, key):
    return S[f"{scen}/{fmt}"][key]


def ms(v):
    return f"{v * 1e3:.1f}"


def mb(v):
    return f"{v / 1e6:.0f}"


def table(head, rows):
    out = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out) + "\n"


def fmts(scen):
    return [k.split("/", 1)[1] for k in S if k.startswith(scen + "/")]


parts = []
AX = ["time", "ens", "var", "level", "lat", "lon"]
gf = fmts("grid")
sel = [("write s", "write_s", 1)] + [("all reads ms", "reads_s", 1e3)]
sel += [(f"8 lines along {AX[d]} ms", f"line{d}_s", 1e3) for d in range(6)]
sel += [(f"4 planes {AX[a]} x {AX[b]} ms", f"plane{a}{b}_s", 1e3) for a in range(6) for b in range(a + 1, 6)]
sel += [("16 cubes ms", "cubes_s", 1e3), ("4 ensemble means ms", "ensmean_s", 1e3)]
parts.append("### grid: 6-D f32 array larger than memory (HDF5-favored)\n")
parts.append(table(["selection"] + gf, [[n] + [f"{g('grid', f, k) * sc:.1f}" for f in gf] for n, k, sc in sel]))
parts.append("\n### oltp: 1M accounts, durable transactions (SQLite-favored)\n")
parts.append(table(
    ["format", "load s", "1000 txn s", "txn p50 ms", "txn p99 ms", "20k points ms", "200 branch aggregates ms", "MB"],
    [[f, f"{g('oltp', f, 'load_s') + g('oltp', f, 'maint_s'):.2f}", f"{g('oltp', f, 'txn_s'):.2f}",
      f"{g('oltp', f, 'txn_p50_ms'):.2f}", f"{g('oltp', f, 'txn_p99_ms'):.2f}",
      ms(g('oltp', f, 'warm_points_s')), ms(g('oltp', f, 'warm_branch_s')), mb(g('oltp', f, 'bytes'))]
     for f in fmts("oltp")]))
parts.append("\n### analytics: 4M rows x 12 columns (Parquet-favored)\n")
parts.append(table(
    ["format", "write s", "Q1 group-by ms", "Q2 20 ranges ms", "MB"],
    [[f, f"{g('analytics', f, 'write_s'):.2f}", ms(g('analytics', f, 'warm_q1_s')),
      ms(g('analytics', f, 'warm_q2_s')), mb(g('analytics', f, 'bytes'))] for f in fmts("analytics")]))
print("\n".join(parts))
