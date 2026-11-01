#!/usr/bin/env python3
"""Alternating equal-work dependency qualification; run without other builds/tests."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys

p = argparse.ArgumentParser()
p.add_argument('--before', type=Path, required=True, help='release/examples directory')
p.add_argument('--after', type=Path, required=True)
p.add_argument('--repo', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--pairs', type=int, default=5)
p.add_argument('--sizes', nargs='+', choices=['1', '4'], default=['1', '4'])
p.add_argument('--native-cases', nargs='+', type=int, choices=range(11))
p.add_argument('--phase', choices=['native', 'concurrent'], required=True)
a = p.parse_args()
assert a.pairs >= 3
a.output = a.output.resolve()
a.repo = a.repo.resolve()
a.output.mkdir(parents=True, exist_ok=True)
scratch = a.output / 'disk-data'
scratch.mkdir(exist_ok=True)
env = dict(os.environ, TMPDIR=str(scratch))
bins = {'before': a.before.resolve(), 'after': a.after.resolve()}
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
provenance = {s: {n: sha(d / n) for n in ['record_mutation_load', 'oom_maintenance_load', 'scalable_load']} for s, d in bins.items()}
(a.output / 'binary-hashes.json').write_text(json.dumps(provenance, indent=2) + '\n')

if a.phase == 'concurrent':
    sys.path.insert(0, str(a.repo / 'scripts'))
    from compare_load import compare
    failures = []
    for total, epoch in [('1', '16'), ('4', '64')]:
        if total not in a.sizes:
            continue
        reports = {'before': [], 'after': []}
        for pair in range(a.pairs):
            for side in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                name = f'swmr-{total}-{pair}-{side}'
                out, data = a.output / name, scratch / name
                assert not out.exists() and not data.exists()
                command = ['python3', str(a.repo / 'scripts/load_scalable.py'), '--suite', 'large',
                           '--only', 'indexed-r8-20g', '--total-gib', total, '--epoch-mib', epoch,
                           '--reader-checks-per-epoch', '16384', '--read-seconds', '1',
                           '--data-root', str(data), '--output', str(out),
                           '--binary', str(bins[side] / 'scalable_load')]
                print('RUN', name, flush=True)
                with (a.output / (name + '.log')).open('w') as log:
                    subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=600)
                reports[side].append(json.loads((out / 'report.json').read_text()))
                assert (data / '.varlit-load-owner').read_text() == str(out)
                shutil.rmtree(data)
        result = compare(reports['before'], reports['after'])
        (a.output / f'swmr-{total}-comparison.json').write_text(json.dumps(result, indent=2) + '\n')
        print('SUMMARY', total, json.dumps(result['metrics']), flush=True)
        if result['status'] != 'passed':
            failures.append(total)
    if failures:
        raise SystemExit('Resource regression in GiB workloads: ' + ', '.join(failures))
else:
    # Same CPU for each single-process sample. The concurrent phase retains all CPUs.
    os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
    measure = a.output / 'measure-child'
    subprocess.run(['cc', '-O2', str(a.repo / 'docs/performance/boundary-redesign-20261101/measure-child.c'), '-o', str(measure)], check=True)
    cases = [
        ('record_mutation_load', ['writer', 'fixed', 'forward', '2000000', '200000']),
        ('record_mutation_load', ['writer', 'variable', 'random', '200000', '20000']),
        ('record_mutation_load', ['rewrite', 'variable', 'random', '200000', '20000']),
        ('record_mutation_load', ['recovery', 'fixed', 'forward', '200000', '20000']),
        ('record_mutation_load', ['writer', 'large', 'reverse', '65536', '4096']),
        ('record_mutation_load', ['lazy', 'large', 'reverse', '65536', '4096']),
        ('record_mutation_load', ['writer', 'mixed', 'random', '131072', '8192']),
        ('oom_maintenance_load', ['complex', '65536', '512']),
        ('oom_maintenance_load', ['checkpoint', '200000', '256']),
        ('oom_maintenance_load', ['chunks', '67108864', '1048576']),
    ]
    encoded = scratch / 'decode-fixture.bin'
    subprocess.run([str(bins['before'] / 'oom_maintenance_load'), 'prepare-decode', '67108864', '1048576', str(encoded)], env=env, check=True)
    cases.append(('oom_maintenance_load', ['decode', '67108864', '1048576', str(encoded)]))
    samples, summaries = [], []
    for number, (binary, args) in enumerate(cases):
        if a.native_cases and number not in a.native_cases:
            continue
        print('CASE', number, binary, args, flush=True)
        selected = []
        for pair in range(a.pairs):
            for side in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                usage = a.output / 'usage.json'
                run = subprocess.run([str(measure), str(usage), str(bins[side] / binary), *args],
                                     capture_output=True, text=True, env=env, check=True, timeout=600)
                row = {'case': number, 'binary': binary, 'args': args, 'pair': pair, 'side': side,
                       'result': json.loads(run.stdout), 'usage': json.loads(usage.read_text())}
                selected.append(row)
                samples.append(row)
                (a.output / 'native-samples.json').write_text(json.dumps(samples, indent=2) + '\n')
                print(pair, side, row['result']['seconds'], flush=True)
        work = [{k: v for k, v in r['result'].items() if k != 'seconds'} for r in selected]
        assert all(w == work[0] for w in work), 'work/resource geometry differs'
        summary = {'case': number, 'binary': binary, 'args': args, 'work': work[0], 'pairs': a.pairs, 'metrics': {}}
        for field, accessor, aggregate in [
            ('operation_seconds', lambda r: r['result']['seconds'], statistics.median),
            ('process_cpu_seconds', lambda r: r['usage']['cpu'], statistics.median),
            ('process_peak_rss_kib', lambda r: r['usage']['rss'], max),
            ('process_write_bytes', lambda r: r['usage']['write_blocks'] * 512, statistics.median),
        ]:
            values = {side: aggregate(accessor(r) for r in selected if r['side'] == side) for side in bins}
            delta = (values['after'] / values['before'] - 1) * 100 if values['before'] else None
            summary['metrics'][field] = dict(values, change_percent=delta)
        summaries.append(summary)
        (a.output / 'native-summary.json').write_text(json.dumps(summaries, indent=2) + '\n')
        print('SUMMARY', json.dumps(summary['metrics']), flush=True)
