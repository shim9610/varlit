#!/usr/bin/env python3
"""Produce operation-by-operation ratios without mixing unsupported query sets."""
import argparse
import json
from pathlib import Path
import statistics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    observations = [json.loads(x) for x in (args.run / "results.jsonl").read_text().splitlines()]
    groups, probes, failures = {}, {}, []
    for row in observations:
        if row.get("error"):
            failures.append(row)
        elif row["probe"]:
            probes[row["profile"]["name"]] = row
        elif not row["visibility_gate"]:
            groups.setdefault(row["profile"]["name"], []).append(row)
    medians = {}
    for name, rows in groups.items():
        p = rows[0]["profile"]
        median = lambda f: statistics.median(f(x) for x in rows)
        metrics = dict(completion=median(lambda x: x["usage"]["elapsed_seconds"]),
                       cpu=median(lambda x: x["usage"]["cpu"]),
                       rss_mib=median(lambda x: x["usage"]["rss_kib"] / 1024),
                       allocated_mib=median(lambda x: x["result"]["allocated_bytes"] / 2**20))
        if p["kind"] == "mixed":
            metrics.update(write=median(lambda x: x["result"]["initialization_seconds"]+x["result"]["writer_seconds"]+x["result"]["finalize_seconds"]),
                           ingest=median(lambda x:x["result"]["writer_seconds"]),
                           read_active=median(lambda x: max(r["consume_seconds"] + r["refresh_seconds"] for r in x["result"]["readers"])))
        else:
            for target, source in [("write", "write_seconds"), ("scan", "scan_seconds"),
                                   ("point", "point_seconds"), ("range", "range_seconds")]:
                if rows[0]["result"][source] is not None:
                    metrics[target] = median(lambda x: x["result"][source])
            if rows[0]["result"].get("warm_scan_seconds") is not None:
                metrics["warm_scan"]=median(lambda x:x["result"]["warm_scan_seconds"])
        medians[name] = dict(profile=p, observations=len(rows), metrics=metrics,
                             probe=probes.get(name, {}).get("sampled_peaks"),
                             probe_baseline=probes.get(name, {}).get("probe_baseline"),
                             completion_min=min(x["usage"]["elapsed_seconds"] for x in rows),
                             completion_max=max(x["usage"]["elapsed_seconds"] for x in rows))
    comparisons = []
    for name, value in medians.items():
        p = value["profile"]
        if p["kind"] == "mixed" and p["engine"].startswith("varlit-"):
            keys=["scenario","records","payload","variable","per_tx","producers","readers","lookups"]
            candidates=[n for n,v in medians.items() if v["profile"].get("engine")=="sqlite" and all(v["profile"]["config"].get(k)==p["config"].get(k) for k in keys)]
            if not candidates:continue
            for metric in ["write", "ingest", "completion", "read_active"]:
                sql=min(candidates,key=lambda n:medians[n]["metrics"][metric])
                ratio = medians[sql]["metrics"][metric] / value["metrics"][metric]
                comparisons.append(dict(profile=name, sqlite=sql, metric=metric, ratio=ratio,
                                        repetitions=value["observations"]))
        elif p["kind"] == "array" and not p["mode"].startswith(("sql-", "hdf-")):
            candidates = [n for n, v in medians.items() if v["profile"]["kind"] == "array"
                          and v["profile"].get("total") == p["total"]
                          and v["profile"].get("mode", "").startswith("sql-")]
            for metric in ["write", "scan", "warm_scan", "point", "range"]:
                if metric not in value["metrics"] or not candidates:
                    continue
                eligible=[n for n in candidates if metric in medians[n]["metrics"] and (metric in ["write","scan"] or bool(medians[n]["profile"].get("warm_scan"))==bool(p.get("warm_scan")))]
                if not eligible:continue
                sql = min(eligible, key=lambda n: medians[n]["metrics"][metric])
                ratio = medians[sql]["metrics"][metric] / value["metrics"][metric]
                comparisons.append(dict(profile=name, sqlite=sql, metric=metric, ratio=ratio,
                                        repetitions=value["observations"]))
    args.output.mkdir(parents=True, exist_ok=True)
    result = dict(profiles=medians, comparisons=comparisons, failures=failures,
                  timing_observations=sum(len(v) for v in groups.values()), resource_probes=len(probes),
                  compared_operations=len(comparisons),
                  qualification="observational medians only; no identical-binary noise control; not a 5% regression qualification")
    (args.output / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    lines = ["# Operation comparisons", "", "Ratio = SQLite elapsed / Varlit elapsed.",
             "Array baselines identify the fastest complete measured SQLite layout across block geometries for each operation; all layout costs remain reported separately.",
             "Stream's unsupported point/range work is excluded. Read-active includes refresh and verification, not idle sleeps.", "",
             "| Varlit profile | SQLite profile | Operation | Ratio |", "|---|---|---|---:|"]
    for row in comparisons:
        lines.append(f"| {row['profile']} | {row['sqlite']} | {row['metric']} | {row['ratio']:.3f} |")
    (args.output / "operations.md").write_text("\n".join(lines) + "\n")
    template=(Path(__file__).resolve().parent/"report.html").read_text()
    (args.output/"index.html").write_text(template.replace("__DATA__",json.dumps(result).replace("</","<\\/")))


if __name__ == "__main__":
    main()
