#!/usr/bin/env python3
"""Confirm the initial 2M-record CPU signal with 15 A/B and 10 A/A pairs."""
import argparse
import json
import os
from pathlib import Path
import resource
import statistics
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--before', type=Path, required=True)
p.add_argument('--after', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
a = p.parse_args()
os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
env = dict(os.environ, TMPDIR=str(a.output / 'disk-data'))
rows = []
for experiment, count in [('confirmation', 15), ('same_binary_control', 10)]:
    for pair in range(count):
        for side in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
            binary = a.before if side == 'before' or experiment == 'same_binary_control' else a.after
            usage = a.output / 'confirmation-usage.json'
            previous = resource.getrusage(resource.RUSAGE_CHILDREN)
            run = subprocess.run([str(a.output / 'measure-child'), str(usage), str(binary),
                                  'writer', 'fixed', 'forward', '2000000', '200000'],
                                 env=env, text=True, capture_output=True, check=True, timeout=120)
            current = resource.getrusage(resource.RUSAGE_CHILDREN)
            row = dict(experiment=experiment, pair=pair, side=side, binary=str(binary),
                       result=json.loads(run.stdout), usage=json.loads(usage.read_text()),
                       diagnostic_user_seconds=current.ru_utime - previous.ru_utime,
                       diagnostic_system_seconds=current.ru_stime - previous.ru_stime)
            rows.append(row)
            (a.output / 'cpu-confirmation-samples.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(experiment, pair, side, row['usage']['cpu'], flush=True)
initial = [r for r in json.loads((a.output / 'native-samples.json').read_text()) if r['case'] == 0]
summary = {}
for name, samples in [('initial', initial),
                      ('confirmation', [r for r in rows if r['experiment'] == 'confirmation']),
                      ('pooled', initial + [r for r in rows if r['experiment'] == 'confirmation']),
                      ('same_binary_control', [r for r in rows if r['experiment'] == 'same_binary_control'])]:
    work = [{k: v for k, v in r['result'].items() if k != 'seconds'} for r in samples]
    assert all(w == work[0] for w in work)
    metrics = {}
    for field, accessor, aggregate in [
        ('cpu_seconds', lambda r: r['usage']['cpu'], statistics.median),
        ('operation_seconds', lambda r: r['result']['seconds'], statistics.median),
        ('peak_rss_kib', lambda r: r['usage']['rss'], max),
        ('write_bytes', lambda r: r['usage']['write_blocks'] * 512, statistics.median),
    ]:
        values = {s: [accessor(r) for r in samples if r['side'] == s] for s in ['before', 'after']}
        b, c = aggregate(values['before']), aggregate(values['after'])
        metrics[field] = dict(before=b, after=c, change_percent=(c / b - 1) * 100,
                              before_range=[min(values['before']), max(values['before'])],
                              after_range=[min(values['after']), max(values['after'])])
    summary[name] = dict(pairs=len(samples) // 2, metrics=metrics)
(a.output / 'cpu-confirmation-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2), flush=True)
