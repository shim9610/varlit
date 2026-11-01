#!/usr/bin/env python3
"""Equal-work writer resume timing on real committed files; no concurrent builds."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--before', type=Path, required=True)
p.add_argument('--after', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--pairs', type=int, default=5)
p.add_argument('--opens', type=int, default=1000)
a = p.parse_args()
assert a.pairs >= 3 and a.opens > 0
out = a.output.resolve()
out.mkdir(parents=True, exist_ok=False)
bins = {'before': a.before.resolve(), 'after': a.after.resolve()}
report = {'pairs': a.pairs, 'opens_per_sample': a.opens,
          'binary_sha256': {s: hashlib.sha256(b.read_bytes()).hexdigest() for s, b in bins.items()},
          'scope': 'warm repeated opens; divergent heads stay divergent; not power-loss latency',
          'samples': [], 'summaries': [], 'syscalls': []}
os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})

def save():
    (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')

for gib, epoch in [(1, 16), (4, 64)]:
    data = out / f'data-{gib}'
    data.mkdir()
    common = ['--mode', 'indexed', '--root', str(data), '--total-bytes', str(gib * 1024**3),
              '--epoch-bytes', str(epoch * 1024**2), '--readers', '0', '--pinned-readers', '0']
    with (out / f'create-{gib}.log').open('w') as log:
        subprocess.run([str(bins['after']), '--phase', 'write', *common], stdout=log, stderr=subprocess.STDOUT, check=True, timeout=600)
    files = {f.name: f.stat().st_size for f in data.iterdir() if f.is_file()}
    sidecars = [f for f in data.iterdir() if f.is_file() and f.open('rb').read(8) == b'VARVEIX5']
    assert len(sidecars) == 1
    sidecar = sidecars[0]
    with sidecar.open('r+b') as f:
        f.seek(8192)
        saved_head = f.read(64)
    for state in ['matching', 'divergent']:
        if state == 'divergent':
            # One invalid old copy; the other retains the complete current head.
            # Flush fixture construction outside timing. This measures the extra
            # conditional sync call on a clean cache, not dirty-device latency.
            with sidecar.open('r+b') as f:
                f.seek(8192)
                f.write(bytes(64))
                f.flush()
                os.fsync(f.fileno())
        rows = []
        for pair in range(a.pairs):
            for side in (['before', 'after'] if pair % 2 == 0 else ['after', 'before']):
                command = [str(bins[side]), '--phase', 'writer-open', '--open-count', str(a.opens), *common]
                run = subprocess.run(command, capture_output=True, text=True, check=True, timeout=600)
                value = json.loads(run.stdout)
                assert value['event'] == 'writer_open_complete' and value['opens'] == a.opens
                row = dict(gib=gib, state=state, side=side, pair=pair, **value)
                rows.append(row)
                report['samples'].append(row)
                save()
        medians = {side: statistics.median(r['seconds'] for r in rows if r['side'] == side) for side in bins}
        report['summaries'].append(dict(gib=gib, state=state, seconds=medians,
            microseconds_per_open={s: v * 1e6 / a.opens for s, v in medians.items()},
            change_percent=(medians['after'] / medians['before'] - 1) * 100, files=files))
        for side in bins:
            trace = out / f'trace-{gib}-{state}-{side}.log'
            subprocess.run(['strace', '-qq', '-o', str(trace), '-e', 'trace=fsync,fdatasync,pread64',
                str(bins[side]), '--phase', 'writer-open', '--open-count', '10', *common],
                capture_output=True, check=True, timeout=600)
            text = trace.read_text()
            report['syscalls'].append(dict(gib=gib, state=state, side=side, opens=10,
                fdatasync=text.count('fdatasync('), fsync=text.count('fsync('), pread=text.count('pread64('), log=trace.name))
        assert files == {f.name: f.stat().st_size for f in data.iterdir() if f.is_file()}
        save()
        print(json.dumps(report['summaries'][-1]), flush=True)
    with sidecar.open('r+b') as f:
        f.seek(8192)
        f.write(saved_head)
        f.flush()
        os.fsync(f.fileno())
    # Only this newly-created directory is owned by this invocation.
    shutil.rmtree(data)
