#!/usr/bin/env python3
"""Keep diagnostic cohorts separate; preserve all repeats and observed alerts."""
import argparse
import json
from pathlib import Path
import statistics
from analyze_read_validation import observed_gate


def analyze(root):
    manifest = json.loads((root / 'manifest.json').read_text())
    rows = [json.loads(x) for x in (root / 'results.jsonl').read_text().splitlines()]
    assert len(rows) == 170
    assert all(not r.get('error') and r['result']['verified'] for r in rows)
    profiles = {}
    for p in manifest['profiles']:
        selected = [r for r in rows if r['profile']['name'] == p['name']]
        timing = [r for r in selected if not r['probe']]
        probes = [r for r in selected if r['probe']]
        keys = (['api_wall_seconds', 'api_cpu_seconds', 'oracle_seconds', 'open_seconds', 'write_seconds']
                if p['kind'] == 'payload' else
                ['initialization_seconds', 'writer_seconds', 'finalize_seconds', 'writer_thread_cpu_seconds',
                 'producer_thread_cpu_seconds', 'readback_seconds', 'readback_cpu_seconds']
                if p['workload'] == 'writer-only' else ['range_seconds', 'point_seconds', 'scan_seconds'])
        if p['workload'] == 'memory-replication':
            assert not timing and len(probes) == 3
            keys = []
        else:
            assert len(timing) == 6
        values = {k: [r['result'][k] for r in timing] for k in keys}
        if timing:
            values['completion'] = [r['usage']['elapsed_seconds'] for r in timing]
            values['cpu'] = [r['usage']['cpu'] for r in timing]
        for k in ['pss_sum', 'rss_sum', 'temporary_allocated', 'deleted_open_allocated', 'visible_allocated']:
            if probes:
                values[k + '_mib'] = [r['sampled_peaks'][k] / 2**20 for r in probes]
        profiles[p['name']] = dict(profile=p, timing_observations=len(timing), resource_probes=len(probes),
            values=values, medians={k:statistics.median(v) for k,v in values.items()},
            ranges={k:[min(v),max(v)] for k,v in values.items()})
    comparisons = []
    for w in ['payload-g65536', 'payload-g1048576', 'writer-only', 'range-repeated']:
        modes = {p['profile']['variant']:p for p in profiles.values() if p['profile']['workload'] == w}
        for metric in modes['before-a']['medians']:
            if metric.endswith('_mib'): continue
            a,b,n = [modes[k]['medians'][metric] for k in ['before-a','before-b','after-default']]
            gate = observed_gate(a,b,n)
            spreads = [(modes[k]['ranges'][metric][1]-modes[k]['ranges'][metric][0])/modes[k]['medians'][metric]
                       if modes[k]['medians'][metric] else 0 for k in ['before-a','before-b']]
            row = dict(workload=w, metric=metric, **gate, baseline_repeat_spread=spreads,
                       aa_median_status=gate['status'])
            if max(spreads) > .05: row['status'] = 'unqualified'
            row['full_over_mode'] = {k:modes['full']['medians'][metric]/modes[k]['medians'][metric]
                                     for k in ['crc-only','structure-only','trusted'] if k in modes}
            comparisons.append(row)
    before = (root/'memory-before.txt').read_text(); after = (root/'memory-after.txt').read_text()
    assert before == after, 'resource events changed; inspect instead of publishing a pass'
    return dict(profiles=profiles, comparisons=comparisons, timing_observations=144, resource_probes=26,
        resource_caps=dict(cpu=4,memory_gib=16,disk_gib=32), memory_events_unchanged=True,
        qualification='Supplemental cohorts, never pooled with primary. Six balanced timing repeats. Single payload probes and three memory probes are observations, not 5% certification.')


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('run',type=Path);ap.add_argument('--output',type=Path,required=True)
    args=ap.parse_args();args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(analyze(args.run),indent=2)+'\n')


if __name__ == '__main__': main()
