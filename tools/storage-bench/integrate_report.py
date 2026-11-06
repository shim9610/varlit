#!/usr/bin/env python3
"""Build the existing comparison dashboard from separate, non-pooled cohorts."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import statistics

from analyze_grouped import telemetry


def append_only_study(data, root):
    """Keep prototype, corrected runtime and fixed calibration distinct."""
    data = copy.deepcopy(data)
    data['profiles'] = {n: r for n, r in data['profiles'].items()
                        if r.get('presentation') != 'append-only'}
    data['cohorts'] = [c for c in data['cohorts'] if c.get('kind') != 'append-only']
    manifests, events, changes = {}, {}, {}
    cohorts = ['primary-v1', 'primary-v2', 'primary-final-v1', 'calibration-v1', 'calibration-alerts-v1']
    for extra in ['lifecycle-v1', 'lifecycle-calibration-v1']:
        if (root / extra).is_dir():
            cohorts.append(extra)
    for cohort in cohorts:
        manifest = json.loads((root / cohort / 'manifest.json').read_text())
        manifests[cohort] = manifest
        rows = [json.loads(s) for s in (root / cohort / 'results.jsonl').read_text().splitlines()]
        assert not any(r.get('error') or not r['result']['verified'] for r in rows)
        for profile in manifest['profiles']:
            selected = [r for r in rows if r['profile']['name'] == profile['name']]
            times = [r for r in selected if not r['probe']]
            probes = [r for r in selected if r['probe']]
            assert len(times) == manifest['repeats'] and len(probes) == 1

            def metrics(row):
                x, u = row['result'], row['usage']
                m = dict(write=x['writer_seconds'], read_active=x['reader_active_seconds'],
                         completion=u['elapsed_seconds'], cpu=u['cpu'],
                         writer_cpu=x['writer_cpu_seconds'],
                         reader_cpu=sum(r['cpu'] for r in x['reader_stats']),
                         rss_mib=u['rss_kib'] / 1024, allocated_mib=x['allocated_bytes'] / 2**20)
                if x['warm_seconds']:
                    m['warm_scan'] = max(x['warm_seconds'])
                else:
                    begin, end = x['writer_start_ns'], x['writer_end_ns']
                    m['min_reader_during_fraction'] = min(
                        sum(n for a, b, n in r['intervals'] if a >= begin and b <= end)
                        for r in x['reader_stats']) / profile['total']
                return m

            values = [metrics(r) for r in times]
            medians = {k: statistics.median(v[k] for v in values) for k in values[0]}
            spread = {}
            for key in values[0]:
                vs = [v[key] for v in values]
                q = statistics.quantiles(vs, n=4, method='inclusive')
                spread[key] = dict(min=min(vs), p25=q[0], p75=q[2], max=max(vs))
            name = f'appendcheck-{cohort}-{profile["name"]}'
            p = dict(profile, name=name, cohort=cohort)
            p.pop('binary_override', None)
            data['profiles'][name] = dict(profile=p, observations=len(times), metrics=medians,
                metric_spread=spread, probe=probes[0]['sampled_peaks'],
                probe_baseline=probes[0]['probe_baseline'], presentation='append-only',
                origin_report=f'Append-only ownership / {cohort}; reader threads, separate cohort')
        study = [r for r in data['profiles'].values()
                 if r.get('presentation') == 'append-only' and r['profile']['cohort'] == cohort]
        changes[cohort] = {}
        calibration = cohort.startswith('calibration')
        for workload in sorted({r['profile']['workload'] for r in study}):
            variants = {r['profile']['variant']: r['metrics'] for r in study
                        if r['profile']['workload'] == workload}
            references = [('raw-b', 'raw-a'), ('guarded', 'raw-a')] if calibration else [
                ('baseline-b', 'baseline-a'), ('current', 'baseline-a')] if cohort.startswith('lifecycle') else [
                ('before-b', 'before-a'), ('after-raw', 'before-a'), ('after-guarded', 'after-raw')]
            if 'reconnect' in variants:
                references.append(('reconnect', 'current'))
            changes[cohort][workload] = {f'{candidate}/{reference}': {
                k: 100 * (variants[candidate][k] / variants[reference][k] - 1)
                for k in variants[candidate] if variants[reference][k] > 0}
                for candidate, reference in references}
        def memory_events(name):
            return {k: int(v) for k, v in (line.split() for line in (root / cohort / name).read_text().splitlines())}
        before, after = memory_events('memory-before.txt'), memory_events('memory-after.txt')
        events[cohort] = {k: after[k] - before[k] for k in ['oom', 'oom_kill']}
        assert all(v == 0 for v in events[cohort].values())
        data['cohorts'].append(dict(name=f'Append-only ownership / {cohort}', kind='append-only',
            configurations=len(manifest['profiles']), timing_observations=len(times)*len(manifest['profiles']),
            resource_probes=len(manifest['profiles']), refused_attempts=0))
    data['append_only'] = dict(date='2026-11-06', budgets=dict(cpu=4, memory_gib=16, filesystem_gib=32),
        manifests={c: {k: m[k] for k in ['started_utc', 'baseline', 'binaries', 'affinity', 'memory_max', 'cpu_max']}
                   for c, m in manifests.items()}, memory_event_deltas=events, changes_percent=changes,
        evidence='append-only-mmap-20261106.tar.gz', lifecycle_evidence='append-only-lifecycle-20261106.tar.gz' if 'lifecycle-v1' in cohorts else None,
        qualification='Separate implementation and fixed 30-round phase-check cohorts. Identical-binary controls and all phase alerts retained; no universal zero-overhead or 5% guarantee.',
        runtime_notes='primary-v1 used the earlier resource-options replacement API with fully resolved FULL limits. primary-v2 and both calibrations use open_reader_with_resource_limits and preserve missing format defaults. primary-final-v1 additionally includes metadata-only mmap Debug formatting and predates lifecycle-v1. lifecycle-v1 compares the c5 guarded baseline with current guarded operation and actual reader-first repeated writer leases. lifecycle-calibration-v1 retains fixed 30-round before A/B/current phase checks. No cohort pooling.',
        scope='One process, one active writer and independent reader threads; live per-record windows or two-pass fixed snapshots. No new SQLite/HDF5 comparison.')
    data['timing_observations'] = sum(c['timing_observations'] for c in data['cohorts'])
    data['resource_probes'] = sum(c['resource_probes'] for c in data['cohorts'])
    return data


def mmap_study(data, root):
    """Integrate fixed cohorts without pooling repeats or selecting metric-wise winners."""
    data = copy.deepcopy(data)
    removed = {n for n, r in data["profiles"].items() if r.get("presentation") == "mmap"}
    data["profiles"] = {n: r for n, r in data["profiles"].items() if n not in removed}
    data["comparisons"] = [r for r in data["comparisons"] if r["profile"] not in removed]
    data["cohorts"] = [c for c in data["cohorts"] if c.get("kind") != "sqlite-mmap"]
    qualification = {}
    for cohort in ["control-v1", "control-calibration-v1", "control-final-v1"]:
        rows = [json.loads(s) for s in (root / cohort / "results.jsonl").read_text().splitlines()]
        assert not any(r.get("error") for r in rows)
        cases = {}
        for workload in sorted({r["profile"]["workload"] for r in rows}):
            medians = {}
            for variant in ["before-a", "before-b", "after"]:
                times = [r for r in rows if r["profile"]["workload"] == workload
                         and r["profile"]["variant"] == variant and not r["probe"]]
                medians[variant] = {k: statistics.median(r["usage"][k] for r in times)
                                    for k in ["elapsed_seconds", "cpu"]}
                medians[variant]["n"] = len(times)
            cases[workload] = dict(medians=medians, percent_vs_before_a={
                variant: {k: 100 * (medians[variant][k] / medians["before-a"][k] - 1)
                          for k in ["elapsed_seconds", "cpu"]}
                for variant in ["before-b", "after"]})
        qualification[cohort] = cases
    manifests = {}
    for cohort in ["primary-v1", "snapshots-v1"]:
        manifest = json.loads((root / cohort / "manifest.json").read_text())
        manifests[cohort] = manifest
        rows = [json.loads(s) for s in (root / cohort / "results.jsonl").read_text().splitlines()]
        assert not any(r.get("error") or not r["result"]["verified"] for r in rows)
        for row in rows:
            if row["profile"]["kind"] == "mixed":
                c = row["profile"]["config"]
                assert all(r["point_items"] == c["records"] * c["group_items"]
                           and r["records"] == c["records"] for r in row["result"]["readers"])
        for profile in manifest["profiles"]:
            selected = [r for r in rows if r["profile"]["name"] == profile["name"]]
            times = [r for r in selected if not r["probe"] and not r["visibility_gate"]]
            probes = [r for r in selected if r["probe"]]
            assert len(times) == 7 and len(probes) == 1

            def metrics(row):
                x, u = row["result"], row["usage"]
                m = dict(completion=u["elapsed_seconds"], cpu=u["cpu"], rss_mib=u["rss_kib"] / 1024,
                         allocated_mib=x["allocated_bytes"] / 2**20)
                if profile["kind"] == "mixed":
                    m.update(write=sum(x[k] for k in ["initialization_seconds", "writer_seconds", "finalize_seconds"]),
                             ingest=x["writer_seconds"],
                             read_active=max(a["consume_seconds"] + a["refresh_seconds"] for a in x["readers"]),
                             drain=x["reader_drain_seconds"])
                    m.update(telemetry(x))
                elif profile.get("live"):
                    begin, end = x["writer_active_start_ns"], x["writer_active_end_ns"]
                    during = [sum(n for a, b, n in r["intervals"] if a >= begin and b <= end)
                              for r in x["reader_stats"]]
                    m.update(write=x["writer_seconds"], ingest=x["writer_seconds"],
                             read_active=x["reader_active_seconds"],
                             min_reader_during_fraction=min(during) / profile["total"],
                             fully_during_write_mib=x["reader_fully_during_write_bytes"] / 2**20,
                             crossing_write_end_mib=x["reader_crossing_write_end_bytes"] / 2**20,
                             snapshot_rebuilds=sum(r.get("snapshot_rebuilds", 0) for r in x["reader_stats"]),
                             snapshot_index_entries_copied=sum(r.get("snapshot_index_entries_copied", 0) for r in x["reader_stats"]))
                else:
                    m.update(write=x["write_seconds"], scan=x["scan_e2e_seconds"],
                             scan_backend=x["scan_seconds"], warm_scan=x["warm_scan_seconds"],
                             point=x["point_seconds"], range=x["range_seconds"],
                             parallel=x["parallel_seconds"], open=x["open_seconds"])
                return m

            values = [metrics(r) for r in times]
            medians = {k: statistics.median(v[k] for v in values) for k in values[0]}
            spread = {}
            for key in values[0]:
                vs = [v[key] for v in values]
                q = statistics.quantiles(vs, n=4, method="inclusive")
                spread[key] = dict(min=min(vs), p25=q[0], p75=q[2], max=max(vs))
            prefix = "primary" if cohort == "primary-v1" else "snapshots"
            name = f"mmapcheck-{prefix}-{profile['name']}"
            p = dict(profile, name=name, cohort=cohort)
            p.pop("binary_override", None)
            probe = probes[0]
            data["profiles"][name] = dict(profile=p, observations=7, metrics=medians,
                metric_spread=spread, probe=probe["sampled_peaks"], probe_baseline=probe["probe_baseline"],
                presentation="mmap", origin_report=f"SQLite mmap recheck / {cohort}; separate matched cohort",
                sqlite_facts=times[0]["result"].get("sqlite_facts", times[0]["result"].get("sqlite_settings")))
        data["cohorts"].append(dict(name=f"SQLite mmap recheck / {cohort}", kind="sqlite-mmap",
            configurations=len(manifest["profiles"]), timing_observations=sum(not r["probe"] and not r["visibility_gate"] for r in rows),
            resource_probes=sum(r["probe"] for r in rows), prefix_gates=sum(r["visibility_gate"] for r in rows), refused_attempts=0))
    study = {n: r for n, r in data["profiles"].items() if r.get("presentation") == "mmap"}
    for name, row in study.items():
        if row["profile"]["variant"].startswith("sqlite-"):
            continue
        candidates = [(n, s) for n, s in study.items()
                      if s["profile"]["cohort"] == row["profile"]["cohort"]
                      and s["profile"]["workload"] == row["profile"]["workload"]
                      and s["profile"]["variant"].startswith("sqlite-")]
        baseline, sql = min(candidates, key=lambda pair: pair[1]["metrics"]["completion"])
        for metric in ["write", "ingest", "scan", "warm_scan", "point", "range", "parallel", "read_active", "completion", "cpu"]:
            if metric in row["metrics"]:
                data["comparisons"].append(dict(profile=name, sqlite=baseline, metric=metric,
                    ratio=sql["metrics"][metric] / row["metrics"][metric], repetitions=7,
                    selection="One complete SQLite configuration by median completion within the same cohort/workload"))
    events = {}
    for cohort in ["primary-v1", "snapshots-v1", *qualification]:
        def read_events(name):
            return {k: int(v) for k, v in (line.split() for line in (root / cohort / name).read_text().splitlines())}
        before, after = read_events("memory-before.txt"), read_events("memory-after.txt")
        events[cohort] = {k: after[k] - before[k] for k in ["oom", "oom_kill"]}
        assert all(v == 0 for v in events[cohort].values())
    data["mmap_recheck"] = dict(date="2026-11-06", runtime_head=manifests["snapshots-v1"]["runtime_head"],
        budgets=dict(cpu=4, memory_gib=16, filesystem_gib=32), manifests={c: {
            k: m[k] for k in ["started_utc", "runtime_head", "binaries", "sqlite_version", "sqlite_threadsafe", "affinity", "memory_max", "cpu_max"]}
            for c, m in manifests.items()},
        controls=qualification, memory_event_deltas=events,
        evidence="sqlite-mmap-20261106.tar.gz",
        ratio_policy="Same cohort, same logical work and physical group size; one complete SQLite configuration selected by completion for all metrics.",
        qualification="Seven-run observational medians with separate probes. Controls assess harness changes only; cross-engine ratios are not a 5% regression qualification.")
    data["timing_observations"] = sum(c["timing_observations"] for c in data["cohorts"])
    data["resource_probes"] = sum(c["resource_probes"] for c in data["cohorts"])
    data["compared_operations"] = sum(data["profiles"][r["profile"]]["presentation"] != "historical" for r in data["comparisons"])
    data["qualification"] = "Observational medians only. Original cross-engine cohorts have no identical-binary noise control; separate mmap harness controls are retained. No universal 5% regression qualification."
    return data


def build_current(original, grouped, read_validation=None):
    data = copy.deepcopy(original)
    for name, row in data["profiles"].items():
        row["presentation"] = "historical" if name.startswith("objects-") else "adverse" if name.startswith("table-") else "primary"
        row["origin_report"] = "initial cohort (original metrics retained)"
    for name, source in grouped["profiles"].items():
        assert name not in data["profiles"]
        row = copy.deepcopy(source)
        row["metrics"]["allocated_mib"] = row["metrics"]["final_mib"]
        row["observations"] = len(row["observation_files"])
        row["presentation"] = "historical" if name.startswith("individual-") else "primary"
        row["origin_report"] = "corrected grouped cohort"
        data["profiles"][name] = row
    for name, row in grouped["profiles"].items():
        if row["profile"]["engine"] not in ["varlit-stream", "varlit-indexed"]:
            continue
        if not name.startswith("grouped-"):
            continue
        target = name.removesuffix(row["profile"]["engine"]) + "sqlite"
        sql = grouped["profiles"].get(target)
        if sql is None:
            continue
        for metric in ["write", "read_active", "completion"]:
            ratio = sql["metrics"][metric] / row["metrics"][metric]
            data["comparisons"].append(dict(profile=name, sqlite=target, metric=metric,
                                            ratio=ratio,
                                            repetitions=3))
    data["cohorts"] = [
        dict(name="Initial mixed/array/table cohort", configurations=122, timing_observations=360,
             resource_probes=120, refused_attempts=8),
        dict(name="Corrected grouped/control cohort", configurations=len(grouped["profiles"]),
             timing_observations=3 * len(grouped["profiles"]), resource_probes=len(grouped["profiles"]),
             refused_attempts=0),
    ]
    if read_validation is not None:
        for name, row in read_validation["profiles"].items():
            assert name not in data["profiles"]
            data["profiles"][name] = copy.deepcopy(row)
        data["read_validation_comparisons"] = copy.deepcopy(read_validation["read_validation_comparisons"])
        data["cohorts"].append(dict(name="Independent read checks (separate cohort)",
            configurations=len(read_validation["profiles"]),
            timing_observations=read_validation["timing_observations"],
            resource_probes=read_validation["resource_probes"], refused_attempts=0,
            prefix_gates=read_validation["prefix_gates"]))
    active = [r for r in data["comparisons"]
              if data["profiles"][r["profile"]]["presentation"] != "historical"]
    data["timing_observations"] = sum(c["timing_observations"] for c in data["cohorts"])
    data["resource_probes"] = sum(c["resource_probes"] for c in data["cohorts"])
    data["compared_operations"] = len(active)
    data["ratio_count_scope"] = "Nonhistorical operation observations only; not a qualification."
    data["presentation_policy"] = "Individual-record stress/control rows are historical and excluded from the current view. Source medians remain separate; no pooled timing or universal speed qualification."
    return data


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--original", type=Path)
    ap.add_argument("--grouped", type=Path)
    ap.add_argument("--read-validation", type=Path)
    ap.add_argument("--current", type=Path, help="Retain an already integrated report")
    ap.add_argument("--sqlite-mmap-evidence", type=Path, help="Frozen primary, snapshot and harness-control cohorts")
    ap.add_argument("--append-only-evidence", type=Path, help="Prototype, corrected runtime and fixed phase calibration")
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    if args.current:
        current = json.loads(args.current.read_text())
    else:
        if not args.original or not args.grouped:
            ap.error("provide --current or both --original and --grouped")
        current = build_current(json.loads(args.original.read_text()), json.loads(args.grouped.read_text()),
                                json.loads(args.read_validation.read_text()) if args.read_validation else None)
    if args.sqlite_mmap_evidence:
        current = mmap_study(current, args.sqlite_mmap_evidence)
    if args.append_only_evidence:
        current = append_only_study(current, args.append_only_evidence)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "current-comparison.json").write_text(json.dumps(current, indent=2) + "\n")
    template = Path(__file__).with_name("report.html").read_text()
    template = template.replace('href="comparison.json"', 'href="current-comparison.json"')
    (args.output / "index.html").write_text(template.replace("__DATA__", json.dumps(current).replace("</", "<\\/")))
    (args.output / "CURRENT_SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((args.output/name).read_bytes()).hexdigest()}  {name}\n"
        for name in ["README.md", "current-comparison.json", "index.html", "sqlite-mmap-20261106.tar.gz", "append-only-mmap-20261106.tar.gz", "append-only-lifecycle-20261106.tar.gz"]
        if (args.output / name).exists()))
    print("Integrated", len(current["profiles"]), "measured configurations; historical controls excluded by default")


if __name__ == "__main__":
    main()
