#!/usr/bin/env python3
"""Summarize read modes with matched A/A controls and retained exceedances."""
import argparse
import json
from pathlib import Path
import statistics
from analyze_grouped import telemetry


def observed_gate(before_a, before_b, after, tolerance=.05):
    """Noise cannot turn an observed exceedance into a passing measurement."""
    assert min(before_a, before_b, after) >= 0
    variation = abs(before_b / before_a - 1) if before_a else float(before_b != 0)
    ratios = [after / value if value else None for value in [before_a, before_b]]
    exceeds = after > min(before_a, before_b) * (1 + tolerance)
    return dict(control_variation=variation, after_over_before=ratios,
                observed_exceedance=exceeds,
                status="unqualified" if variation > tolerance else "exceeds" if exceeds else "within")


def metrics(row):
    r = row["result"]
    m = dict(completion=row["usage"]["elapsed_seconds"], cpu=row["usage"]["cpu"],
             rss_mib=row["usage"]["rss_kib"] / 1024,
             allocated_mib=r["allocated_bytes"] / 2**20)
    if row["profile"]["kind"] == "mixed":
        m.update(write=sum(r[k] for k in ["initialization_seconds", "writer_seconds", "finalize_seconds"]),
                 ingest=r["writer_seconds"],
                 read_active=max(x["consume_seconds"]+x["refresh_seconds"] for x in r["readers"]),
                 drain=r["reader_drain_seconds"])
        m.update(telemetry(r, logical_bytes=r["readers"][0]["payload_bytes"]))
    else:
        for target, source in [("write", "write_seconds"), ("scan", "scan_seconds"),
                               ("warm_scan", "warm_scan_seconds"), ("point", "point_seconds"), ("range", "range_seconds")]:
            if r.get(source) is not None: m[target] = r[source]
    return m


def analyze(root):
    manifest = json.loads((root/"manifest.json").read_text())
    assert not manifest["smoke"]
    rows = [json.loads(line) for line in (root/"results.jsonl").read_text().splitlines()]
    assert all(not r.get("error") and r["result"]["verified"] for r in rows)
    profiles, work = {}, {}
    for p in manifest["profiles"]:
        selected = [r for r in rows if r["profile"]["name"] == p["name"]]
        times = [r for r in selected if not r["probe"] and not r["visibility_gate"]]
        probes = [r for r in selected if r["probe"]]
        gates = [r for r in selected if r["visibility_gate"]]
        assert len(times) == manifest["repetitions"] and len(probes) == 1, p["name"]
        assert len(gates) == int(p["kind"] == "mixed"), p["name"]
        if p["kind"] == "mixed":
            c = p["config"]; factor = max(1, c.get("group_items", 0))
            for row in selected:
                signatures = []
                for reader in row["result"]["readers"]:
                    assert reader["records"] == c["records"] and reader["logical_records"] == c["records"] * factor
                    if c["lookups"] and factor > 1:
                        assert reader["point_items"] == reader["logical_records"]
                    signatures.append(tuple(reader[k] for k in ["records", "logical_records", "payload_bytes", "point_items", "lookups"]))
                signature = tuple(signatures)
                assert signature == work.setdefault(p["workload"], signature), p["name"]
        measures = [metrics(row) for row in times]
        keys = measures[0].keys()
        medians = {k: statistics.median(r[k] for r in measures) for k in keys}
        peak = probes[0]["sampled_peaks"]
        assert peak["sampled_processes"] >= (p["config"]["readers"] + 2 if p["kind"] == "mixed" else 2)
        assert peak["visible_allocated"] >= probes[0]["result"]["allocated_bytes"]
        profiles["readcheck-"+p["name"]] = dict(profile=p, observations=len(times), metrics=medians,
            metric_ranges={k:[min(r[k] for r in measures), max(r[k] for r in measures)] for k in keys},
            probe=peak, probe_baseline=probes[0]["probe_baseline"],
            observation_files=[Path(r["command"][-1]).name.replace(".config.json", ".json") if p["kind"] == "mixed" else
                               "see results.jsonl command and profile" for r in times],
            presentation="validation", origin_report="Independent read-check comparison; separate cohort")
    comparisons = []
    for workload in sorted({p["workload"] for p in manifest["profiles"]}):
        by_mode = {v["profile"]["variant"]:v for v in profiles.values() if v["profile"]["workload"] == workload}
        a,b,new = (by_mode[k]["metrics"] for k in ["before-a", "before-b", "after-default"])
        for metric in ["write", "read_active", "completion", "cpu", "scan", "warm_scan", "point", "range", "allocated_mib", "drain", "delay_p99_lower_ms", "delay_p99_upper_ms"]:
            if metric not in a: continue
            comparisons.append(dict(workload=workload, metric=metric, **observed_gate(a[metric],b[metric],new[metric]),
                full_over_mode={k:by_mode["full"]["metrics"][metric]/by_mode[k]["metrics"][metric]
                                for k in ["crc-only", "structure-only", "trusted"]}))
        for metric,key,scale in [("sampled_pss_mib","pss_sum",2**20), ("temporary_mib","temporary_allocated",2**20)]:
            values=[by_mode[k]["probe"][key]/scale for k in ["before-a", "before-b", "after-default"]]
            comparisons.append(dict(workload=workload,metric=metric,**observed_gate(*values),
                scope="One separate sampled probe per condition; observational resource control only"))
    for row in comparisons:
        row["aa_median_status"] = row["status"]
        matches = {v["profile"]["variant"]:v for v in profiles.values() if v["profile"]["workload"] == row["workload"]}
        metric = row["metric"]
        if metric in matches["before-a"]["metric_ranges"]:
            spreads = []
            for mode in ["before-a", "before-b"]:
                lo,hi = matches[mode]["metric_ranges"][metric]
                median = matches[mode]["metrics"][metric]
                spreads.append((hi-lo)/median if median else float(hi != lo))
            row["baseline_repeat_spread"] = spreads
            if max(spreads) > .05:
                row["status"] = "unqualified"
                row["calibration_gap"] = "Within-baseline repetitions vary by more than 5%; AA medians alone are insufficient."
        else:
            row["status"] = "unqualified"
            row["calibration_gap"] = "Only one sampled resource probe per baseline condition; see separate repeated diagnostics."
    return dict(profiles=profiles, comparisons=[], read_validation_comparisons=comparisons, failures=[],
        timing_observations=sum(not r["probe"] and not r["visibility_gate"] for r in rows),
        resource_probes=sum(r["probe"] for r in rows), prefix_gates=sum(r["visibility_gate"] for r in rows),
        qualification="Observed 5% gates with AA median and within-baseline repeat controls; AA-only statuses and exceedances retained. Single resource probes are unqualified. Not statistical or library-wide certification.",
        resource_caps=dict(cpu=4,memory_gib=16,disk_gib=32))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("run",type=Path);ap.add_argument("--output",type=Path,required=True)
    args=ap.parse_args(); result=analyze(args.run)
    args.output.mkdir(parents=True,exist_ok=True)
    (args.output/"comparison.json").write_text(json.dumps(result,indent=2)+"\n")
    print(len(result["profiles"]), "profiles;", result["timing_observations"], "timings;",result["resource_probes"],"sampled probes")


if __name__ == "__main__": main()
