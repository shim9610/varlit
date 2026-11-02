"""Pool initial and confirmation observations; never replace the initial run."""
import json
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1])
out = Path(sys.argv[2])
out.mkdir(parents=True, exist_ok=True)
summary = {}
for stage in ('box', 'arc'):
    folder = root / (stage + '-perf')
    if not (folder / 'summary.json').exists():
        continue
    rows = []
    for phase in ('native', 'native-confirmation'):
        path = folder / phase / 'native-samples.json'
        if path.exists():
            for row in json.loads(path.read_text()):
                rows.append(dict(row, phase=phase))
    native = []
    for case in sorted({row['case'] for row in rows}):
        selected = [row for row in rows if row['case'] == case]
        work = [{k: v for k, v in row['result'].items() if k != 'seconds'} for row in selected]
        assert all(w == work[0] for w in work)
        metrics = {}
        for field, accessor, aggregate in (
            ('operation', lambda r: r['result']['seconds'], statistics.median),
            ('cpu', lambda r: r['usage']['cpu'], statistics.median),
            ('rss_kib', lambda r: r['usage']['rss'], max),
            ('write_bytes', lambda r: r['usage']['write_blocks'] * 512, statistics.median),
        ):
            a, b = [aggregate(accessor(r) for r in selected if r['side'] == side) for side in ('before', 'after')]
            metrics[field] = dict(before=a, after=b, change_percent=100 * (b/a-1) if a else None)
        native.append(dict(case=case, pairs=len(selected)//2, work=work[0], metrics=metrics))
    swmr = {str(size): json.loads((folder / 'swmr' / f'swmr-{size}-comparison.json').read_text())
            for size in (1, 4) if (folder / 'swmr' / f'swmr-{size}-comparison.json').exists()}
    matrix = root / (stage + '-matrix-perf') / 'summary.json'
    owner = root / (stage + '-owner-perf') / 'summary.json'
    summary[stage] = dict(owner=json.loads(owner.read_text()) if owner.exists() else [], direct=json.loads((folder / 'summary.json').read_text()), native=native,
                          swmr=swmr, matrix=json.loads(matrix.read_text()) if matrix.exists() else [])
lines = ['# Per-unit measurements', '', 'Positive changes mean more time, CPU or memory. Native confirmation samples are pooled with their initial samples.', '']
for stage, data in summary.items():
    lines.extend([f'## {stage}', '', '| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |', '| --- | ---: | ---: | ---: | ---: |'])
    def line(name, pairs, m, time='operation', cpu='cpu', rss='rss'):
        a, b = m[rss]['before']/1024, m[rss]['after']/1024
        lines.append(f"| {name} | {pairs} | {m[time]['change_percent']:+.2f}% | {m[cpu]['change_percent']:+.2f}% | {a:.2f} → {b:.2f} |")
    for row in data['owner']:
        line('Owner: ' + row['case'], row['pairs'], row['metrics'])
    for row in data['direct']:
        line(row['case'], row['pairs'], row['metrics'])
    names = {0: '2M records / 200k marks', 4: '4 GiB writer / reverse marks', 7: 'Complex maintenance', 9: '64 MiB chunk encode', 10: '64 MiB full chunk decode'}
    for row in data['native']:
        line(names[row['case']], row['pairs'], row['metrics'], rss='rss_kib')
    for row in data['matrix']:
        line('Matrix: ' + row['case'] + ' (disk)', row['pairs'], row['metrics'])
    lines.extend(['', '| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |', '| --- | ---: | ---: | ---: | ---: | --- |'])
    for size, row in data['swmr'].items():
        m = row['metrics']
        lines.append(f"| {size} GiB / 8 followers + pin | {row['samples_per_version']} | {m['wall_seconds']['change_percent']:+.2f}% | {m['cpu_seconds']['change_percent']:+.2f}% | {m['observed_peak_rss_bytes']['change_percent']:+.2f}% | {row['status']} |")
    lines.append('')
for stage, label in [('final', 'Final versus baseline: original owner benchmark'),
                     ('matched-isolated-after', 'Diagnostic: dispatch outside the timed loop, matching all features')]:
    path = root / (stage + '-owner-perf') / 'summary.json'
    if not path.exists():
        continue
    rows = json.loads(path.read_text())
    summary[stage] = dict(owner=rows)
    lines.extend([f'## {label}', '', '| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |', '| --- | ---: | ---: | ---: | ---: |'])
    for row in rows:
        line(row['case'], row['pairs'], row['metrics'])
    lines.append('')
superseded = root / 'isolated-after-owner-perf' / 'summary.json'
if superseded.exists():
    summary['superseded_diagnostic'] = dict(
        status='not-qualified', reason='before used default features; after used all features',
        owner=json.loads(superseded.read_text()))
resources = []
lines.extend(['## Sampled resource peaks', '',
              'Each row is a separate resource-probe process. Cgroup memory includes filesystem cache and other resident host work; these are observations, not upper bounds.', '',
              '| Unit / workload | RSS MiB before → after | Deleted temporary allocation MiB | Open regular allocation MiB | Whole-cgroup MiB |',
              '| --- | ---: | ---: | ---: | ---: |'])
for stage in ('box', 'arc'):
    for case in ('complex', 'clone-large', 'matrix'):
        paths = [root / (stage + '-matrix-perf') / (side + '-resources.json') if case == 'matrix'
                 else root / (stage + '-perf') / f'resources-{case}-{side}.json' for side in ('before', 'after')]
        data = [json.loads(p.read_text()) for p in paths]
        assert all(row['exit'] == 0 for row in data)
        work = [{k:v for k,v in json.loads(row['stdout']).items() if k != 'seconds'} for row in data]
        assert work[0] == work[1]
        resources.append(dict(stage=stage, case=case, observations=data))
        cells = []
        for key in ('rss_bytes', 'temporary_deleted_allocated_bytes', 'open_regular_allocated_bytes', 'cgroup_current_bytes'):
            a,b = [row['peaks'][key]/2**20 for row in data]
            cells.append(f'{a:.2f} → {b:.2f}')
        lines.append(f'| {stage} / {case} | ' + ' | '.join(cells) + ' |')
summary['resource_probes'] = resources
(out / 'measurements.json').write_text(json.dumps(summary, indent=2) + '\n')
(out / 'measurements.md').write_text('\n'.join(lines).rstrip() + '\n')
