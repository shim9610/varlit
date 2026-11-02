#!/usr/bin/env python3
"""Equal-work standalone zstd decoder comparisons, measured without other jobs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess

p = argparse.ArgumentParser()
p.add_argument('--before', type=Path, required=True)
p.add_argument('--after', type=Path, required=True)
p.add_argument('--repo', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--pairs', type=int, default=5)
p.add_argument('--cases', nargs='+', type=int, choices=range(4))
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
bins = {'before': a.before.resolve() / 'decompression_load', 'after': a.after.resolve() / 'decompression_load'}
(a.output / 'binary-hashes.json').write_text(json.dumps({s: hashlib.sha256(p.read_bytes()).hexdigest() for s, p in bins.items()}, indent=2) + '\n')
measure = a.output / 'measure-child'
subprocess.run(['cc', '-O2', str(a.repo / 'docs/performance/boundary-redesign-20261101/measure-child.c'), '-o', str(measure)], check=True)
samples, summaries = [], []
for case, (mode, length) in enumerate([('known', 1048576), ('known', 67108864), ('unknown', 67108864), ('concatenated', 67108864)]):
    if a.cases and case not in a.cases:
        continue
    rows = []
    for pair in range(a.pairs):
        for side in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
            usage = a.output / 'usage.json'
            result = subprocess.run([str(measure), str(usage), str(bins[side]), mode, str(length), str(1000 if length == 1048576 else 20)], check=True, capture_output=True, text=True)
            row = dict(case=case, pair=pair, side=side, result=json.loads(result.stdout), usage=json.loads(usage.read_text()))
            rows.append(row)
            samples.append(row)
            (a.output / 'samples.json').write_text(json.dumps(samples, indent=2) + '\n')
    work = [{k: v for k, v in r['result'].items() if k != 'seconds'} for r in rows]
    assert all(w == work[0] for w in work)
    metrics = {}
    for name, get, aggregate in [('operation_seconds', lambda r: r['result']['seconds'], statistics.median), ('cpu_seconds', lambda r: r['usage']['cpu'], statistics.median), ('peak_rss_kib', lambda r: r['usage']['rss'], max)]:
        values = {side: aggregate(get(r) for r in rows if r['side'] == side) for side in bins}
        metrics[name] = dict(values, change_percent=(values['after'] / values['before'] - 1) * 100)
    summaries.append(dict(case=case, work=work[0], pairs=a.pairs, metrics=metrics))
    (a.output / 'summary.json').write_text(json.dumps(summaries, indent=2) + '\n')
    print(json.dumps(summaries[-1]), flush=True)
