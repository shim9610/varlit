#!/usr/bin/env python3
"""Summarize a `compare` run: per scenario, each engine's medians and latencies.

Standard library only. Reads <run>/results.jsonl and writes summary.json and
summary.md into --output. Timing observations and resource probes are kept
apart: times, CPU and latencies come from timing runs, memory and storage
from the probe run when there is one.

Latencies need `telemetry`:
- commit: how long the writer's commit (record publication, transaction or
  flush) took, per commit.
- visible: from the start of a commit to the end of the reader poll that
  first returned it. A commit can become visible to readers before the
  writer's call returns, so this counts from the start; it includes the commit
  itself and the reader's polling cadence, and is an upper bound.
"""

import argparse
import json
import statistics
from pathlib import Path


def pct(values, q):
    if not values:
        return None
    values = sorted(values)
    return values[min(len(values) - 1, int(round(q * (len(values) - 1))))]


def latencies(result):
    spans = result.get("publication_spans") or []
    commit = [(end - start) / 1e6 for _, start, end in spans]
    published = {txns: start for txns, start, _ in spans}
    visible = []
    for reader in result.get("readers", []):
        for start, end, old, new, _bytes in reader.get("consume_intervals", []):
            for txn in range(old + 1, new + 1):
                if txn in published:
                    visible.append((end - published[txn]) / 1e6)
    return commit, visible


def scenario_of(name):
    for prefix in ["mixed", "commit", "grouped", "table", "array"]:
        if name.startswith(prefix + "-"):
            return prefix
    return "other"


def engine_of(profile):
    return profile.get("engine") or profile.get("mode")


def base_of(profile):
    name, engine = profile["name"], engine_of(profile)
    return name[: -len(engine) - 1] if name.endswith("-" + engine) else name


def summarize(run):
    rows = [json.loads(line) for line in (run / "results.jsonl").read_text().splitlines()]
    failures = [r["profile"]["name"] for r in rows if r.get("error") or r.get("exit")]
    timing = {}
    probes = {}
    for r in rows:
        if r.get("visibility_gate"):
            continue
        (probes if r["probe"] else timing).setdefault(r["profile"]["name"], []).append(r)
    out = {}
    for name, runs in timing.items():
        profile = runs[0]["profile"]
        med = lambda f: statistics.median(f(r) for r in runs)
        entry = {
            "scenario": scenario_of(name),
            "case": base_of(profile),
            "engine": engine_of(profile),
            "favours": profile.get("favours"),
            "observations": len(runs),
            "elapsed": med(lambda r: r["usage"]["elapsed_seconds"]),
            "elapsed_min": min(r["usage"]["elapsed_seconds"] for r in runs),
            "elapsed_max": max(r["usage"]["elapsed_seconds"] for r in runs),
            "cpu": med(lambda r: r["usage"]["cpu"]),
        }
        result = runs[0]["result"]
        if profile["kind"] == "array":
            for key in ["write_seconds", "scan_seconds", "point_seconds", "range_seconds"]:
                values = [r["result"].get(key) for r in runs if r["result"].get(key) is not None]
                entry[key] = statistics.median(values) if values else None
        else:
            entry["write_seconds"] = med(lambda r: r["result"]["seconds"])
            commits, visible = [], []
            for r in runs:
                c, v = latencies(r["result"])
                commits += c
                visible += v
            entry["commits"] = len(commits) // len(runs) if runs else 0
            entry["commit_ms_p50"] = pct(commits, 0.50)
            entry["commit_ms_p99"] = pct(commits, 0.99)
            entry["visible_ms_p50"] = pct(visible, 0.50)
            entry["visible_ms_p99"] = pct(visible, 0.99)
        for p in probes.get(name, []):
            entry["rss_mib"] = p["usage"]["rss_kib"] / 1024
            storage = p.get("final_storage") or {}
            entry["disk_mib"] = storage.get("visible_allocated", p["result"].get("allocated_bytes", 0)) / 2**20
        if "disk_mib" not in entry:
            entry["disk_mib"] = result.get("allocated_bytes", 0) / 2**20
        out[name] = entry
    return out, failures


def table(entries, columns):
    head = "| case | engine | " + " | ".join(label for label, _ in columns) + " |"
    rule = "|---|---|" + "---:|" * len(columns)
    lines = [head, rule]
    for e in sorted(entries, key=lambda e: (e["case"], e["engine"])):
        cells = []
        for _, key in columns:
            v = e.get(key)
            cells.append("" if v is None else (f"{v:.4g}" if isinstance(v, float) else str(v)))
        lines.append(f"| {e['case']} | {e['engine']} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("run", type=Path)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    entries, failures = summarize(args.run)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "summary.json").write_text(json.dumps({"failures": failures, "profiles": entries}, indent=2) + "\n")
    common = [("elapsed s", "elapsed"), ("min", "elapsed_min"), ("max", "elapsed_max"), ("CPU s", "cpu"),
              ("RSS MiB", "rss_mib"), ("disk MiB", "disk_mib")]
    logged = [("write s", "write_seconds"), ("commit p50 ms", "commit_ms_p50"), ("commit p99 ms", "commit_ms_p99"),
              ("visible p50 ms", "visible_ms_p50"), ("visible p99 ms", "visible_ms_p99")]
    arrays = [("write s", "write_seconds"), ("scan s", "scan_seconds"), ("points s", "point_seconds"),
              ("ranges s", "range_seconds")]
    lines = [f"# Comparison summary\n\nFailures: {failures or 'none'}\n"]
    for scenario in ["mixed", "commit", "grouped", "table", "array"]:
        group = [e for e in entries.values() if e["scenario"] == scenario]
        if group:
            lines.append(f"\n## {scenario}\n")
            lines.append(table(group, (arrays if scenario == "array" else logged) + common))
    (args.output / "summary.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
