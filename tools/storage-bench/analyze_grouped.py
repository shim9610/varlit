#!/usr/bin/env python3
"""Summarize equal-work grouped/control observations and conservative overlap telemetry."""
import argparse
import csv
import json
import math
from pathlib import Path
import statistics

MIB = 1 << 20


def percentile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)]


def telemetry(result, logical_bytes=None):
    begin, end = result["writer_active_start_ns"], result["writer_active_end_ns"]
    publications = {seq: (start, stop) for seq, start, stop in result["publication_spans"]}
    assert len(publications) == result["config"]["records"] // result["config"]["per_tx"]
    total = result["config"]["records"] * result["config"]["payload"] if logical_bytes is None else logical_bytes
    before, crossing, after, lower, upper = [], [], [], [], []
    for reader in result["readers"]:
        a = b = c = 0
        for start, stop, old, new, nbytes in reader["consume_intervals"]:
            assert stop >= start
            if start >= begin and stop <= end:
                a += nbytes
            elif start >= end:
                c += nbytes
            else:
                b += nbytes
            for seq in range(old + 1, new + 1):
                first, last = publications[seq]
                assert stop >= first
                lower.append(max(0, stop - last) / 1e6)
                upper.append((stop - first) / 1e6)
        assert a + b + c == total
        before.append(a)
        crossing.append(b)
        after.append(c)
    return dict(min_reader_during_fraction=min(before) / total,
                min_reader_during_mib_s=min(before) / MIB / ((end - begin) / 1e9),
                max_reader_crossing_bytes=max(crossing), max_reader_after_bytes=max(after),
                delay_p50_lower_ms=percentile(lower, .50), delay_p50_upper_ms=percentile(upper, .50),
                delay_p95_lower_ms=percentile(lower, .95), delay_p95_upper_ms=percentile(upper, .95),
                delay_p99_lower_ms=percentile(lower, .99), delay_p99_upper_ms=percentile(upper, .99))


def analyze(base, grouped_cohort="full-v2", control_cohort="control-v3"):
    groups = {}
    excluded = []
    for cohort in [grouped_cohort, control_cohort]:
        for line in (base / cohort / "results.jsonl").read_text().splitlines():
            row = json.loads(line)
            assert not row.get("error"), (cohort, row)
            row["evidence_cohort"] = cohort
            assert row["result"]["verified"]
            groups.setdefault(row["profile"]["name"], []).append(row)
    profiles = {}
    for name, rows in groups.items():
        times = [r for r in rows if not r["probe"]]
        probes = [r for r in rows if r["probe"]]
        assert len(times) == 3 and len(probes) == 1, name
        p = rows[0]["profile"]
        c = p["config"]
        factor = max(1, c.get("group_items", 0))
        for row in rows:
            for reader in row["result"]["readers"]:
                assert reader["records"] == c["records"]
                assert reader["logical_records"] == c["records"] * factor
                if c["lookups"] and factor > 1:
                    assert reader["point_items"] == reader["logical_records"]
        median = lambda fun: statistics.median(fun(r) for r in times)
        ts = [telemetry(r["result"]) for r in times]
        metrics = dict(write=median(lambda r: sum(r["result"][k] for k in
                       ["initialization_seconds", "writer_seconds", "finalize_seconds"])),
                       read_active=median(lambda r: max(x["consume_seconds"] + x["refresh_seconds"] for x in r["result"]["readers"])),
                       completion=median(lambda r: r["usage"]["elapsed_seconds"]),
                       cpu=median(lambda r: r["usage"]["cpu"]),
                       kernel_write_mib=median(lambda r: r["usage"]["write_bytes"] / MIB),
                       max_observed_pending_items=median(lambda r: max(x["max_observed_pending_records"] for x in r["result"]["readers"]) * factor),
                       final_mib=median(lambda r: r["result"]["allocated_bytes"] / MIB),
                       drain=median(lambda r: r["result"]["reader_drain_seconds"]),
                       completion_min=min(r["usage"]["elapsed_seconds"] for r in times),
                       completion_max=max(r["usage"]["elapsed_seconds"] for r in times))
        metrics.update({k: statistics.median(t[k] for t in ts) for k in ts[0]})
        probe = probes[0]
        peak = probe["sampled_peaks"]
        assert peak["sampled_processes"] == c["readers"] + 2
        assert peak["visible_allocated"] >= probe["result"]["allocated_bytes"]
        profiles[name] = dict(profile=p, logical_items=c["records"] * factor,
                              logical_bytes=c["records"] * c["payload"],
                              physical_blocks=c["records"], publications=c["records"] // c["per_tx"],
                              metrics=metrics, probe=peak, probe_baseline=probe["probe_baseline"],
                              final_files=[r["result"]["final_files"] for r in times],
                              observation_files=[str(Path(r["evidence_cohort"]) / Path(r["command"][-1]).name.replace(".config.json", ".json")) for r in times])
    return profiles, excluded


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--evidence-root", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--grouped-cohort", default="full-v2")
    ap.add_argument("--control-cohort", default="control-v3")
    args = ap.parse_args()
    out = args.output
    out.mkdir(parents=True, exist_ok=True)
    ps, excluded = analyze(args.evidence_root, args.grouped_cohort, args.control_cohort)
    (out / "comparison.json").write_text(json.dumps(dict(profiles=ps, retained_excluded_attempts=excluded, superseded_cohorts=["full-v1", "followup-v1", "control-v1", "control-serial-v1", "control-v2"], supersession_reason="Earlier scattered composite group keys and 1 MiB publication fixture retained separately; final v2 uses ordered block keys, 16 MiB publication and an explicit 1 MiB publication contrast."), indent=2) + "\n")
    columns = ["profile", "items", "blocks", "publications", "write_s", "reader_active_s", "completion_s", "cpu_s",
               "final_mib", "pss_mib", "rss_sum_mib", "peak_visible_mib", "temporary_mib", "deleted_open_mib",
               "min_reader_during_percent", "min_reader_during_mib_s", "p99_delay_lower_ms", "p99_delay_upper_ms", "shared_cgroup_peak_total_mib", "shared_cgroup_peak_file_mib", "shared_cgroup_peak_anon_mib", "shared_cgroup_baseline_total_mib", "max_reader_crossing_payload_mib", "max_reader_after_payload_mib"]
    with (out / "metrics.csv").open("w", newline="") as f:
        w = csv.writer(f, lineterminator="\n")
        w.writerow(columns)
        for name, p in ps.items():
            m, v = p["metrics"], p["probe"]
            w.writerow([name, p["logical_items"], p["physical_blocks"], p["publications"],
                        *[m[k] for k in ["write", "read_active", "completion", "cpu", "final_mib"]],
                        *[v[k] / MIB for k in ["pss_sum", "rss_sum", "visible_allocated", "temporary_allocated", "deleted_open_allocated"]],
                        100 * m["min_reader_during_fraction"], m["min_reader_during_mib_s"],
                        m["delay_p99_lower_ms"], m["delay_p99_upper_ms"],
                        *[v[k] / MIB for k in ["cgroup_memory_current", "cgroup_memory_file", "cgroup_memory_anon"]],
                        p["probe_baseline"]["cgroup_memory_current"] / MIB, m["max_reader_crossing_bytes"]/MIB, m["max_reader_after_bytes"]/MIB])
    lines = ["# All grouped-item and individual-record observations", "",
             "Three-run medians; all timing attempts successful. Resource values are separate sampled probes.", "",
             "| Profile | Write s | Reader-active s | Complete s | CPU s | Final MiB | PSS MiB | During-write % |",
             "|---|---:|---:|---:|---:|---:|---:|---:|"]
    for name, p in ps.items():
        m = p["metrics"]
        lines.append(f"| {name} | {m['write']:.3f} | {m['read_active']:.3f} | {m['completion']:.3f} | {m['cpu']:.3f} | {m['final_mib']:.3f} | {p['probe']['pss_sum']/MIB:.2f} | {100*m['min_reader_during_fraction']:.2f} |")
    lines += ["", "During-write % is the minimum reader's bytes in consume intervals entirely within the writer-active window, divided by its full workload. Crossing intervals are excluded, not apportioned.",
              "PSS includes the full observed process tree; it can miss brief peaks. File cache outside process mappings is recorded separately in the evidence."]
    (out / "all-results.md").write_text("\n".join(lines) + "\n")
    print(f"Validated {len(ps)} configurations, {3*len(ps)} timing observations and {len(ps)} resource probes")


if __name__ == "__main__":
    main()
