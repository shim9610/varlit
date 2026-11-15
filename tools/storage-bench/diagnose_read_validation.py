#!/usr/bin/env python3
"""Separate payload API work, writer-only work and sampled memory replication."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import run as bench
import compare_read_validation as primary


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--before',type=Path,required=True);ap.add_argument('--after',type=Path,required=True)
    ap.add_argument('--output',type=Path,required=True);args=ap.parse_args()
    out=args.output.resolve();out.mkdir(parents=True)
    cpus=sorted(os.sched_getaffinity(0))[:4];os.sched_setaffinity(0,cpus)
    assert int(Path('/sys/fs/cgroup/memory.max').read_text())<=16*2**30
    quota,period=Path('/sys/fs/cgroup/cpu.max').read_text().split();assert int(quota)<=4*int(period)
    subprocess.run(['cc','-O2',str(bench.HERE/'measure.c'),'-o',str(out/'measure')],check=True)
    cases=primary.cases();group=next(p for p in cases if p['name']=='grouped-n8388608-g512-t131072-w4-r8-varlit-indexed')
    native=next(p for p in cases if p['name'].startswith('array-native-owned-'))
    variants=[('before-a',args.before,'legacy'),('after-default',args.after,'legacy'),
              ('full',args.after,'full'),('crc-only',args.after,'crc-only'),
              ('structure-only',args.after,'structure-only'),('trusted',args.after,'trusted'),
              ('before-b',args.before,'legacy')]
    payloads=[];writers=[];ranges=[];resources=[]
    for variant,binaries,policy in variants:
        for size in [65536,1<<20]:
            payloads.append(dict(name=f'payload-g{size}--{variant}',workload=f'payload-g{size}',kind='payload',
                total=1<<30,group=size,passes=3,variant=variant,read_validation=policy,
                binary_override=str((binaries/'payload').resolve())))
        if variant in ['before-a','before-b','after-default','full','trusted']:
            writers.append(dict(group,name='writer-only--'+variant,workload='writer-only',
                config=dict(group['config'],readers=0),variant=variant,read_validation=policy,
                binary_override=str((binaries/'writer').resolve())))
            ranges.append(dict(native,name='range-repeated--'+variant,workload='range-repeated',
                ranges=2048,variant=variant,read_validation=policy,
                binary_override=str((binaries/'arrays').resolve())))
        if variant in ['before-a','after-default','full','trusted']:
            resources.append(dict(group,name='memory-replication--'+variant,workload='memory-replication',
                variant=variant,read_validation=policy,binary_override=str((binaries/'mixed').resolve())))
    profiles=payloads+writers+ranges+resources
    hashes={p['binary_override']:hashlib.sha256(Path(p['binary_override']).read_bytes()).hexdigest() for p in profiles}
    bench.save(out/'manifest.json',dict(profiles=profiles,binaries=hashes,affinity=cpus,
        scope='Diagnostics only, separate from the primary 69-condition cohort. Same work within each workload/variant. Reader-only policy; no resource cap increase.',
        memory_max=Path('/sys/fs/cgroup/memory.max').read_text().strip(),cpu_max=Path('/sys/fs/cgroup/cpu.max').read_text().strip(),disk_budget_gib=32))
    (out/'memory-before.txt').write_text(Path('/sys/fs/cgroup/memory.events').read_text())
    phases=[]
    for repeat in range(6):
        ps=payloads+writers+ranges
        for p in ps if repeat%2==0 else reversed(ps): phases.append((p,f'timing-{repeat}-'+p['name'],False))
    for p in payloads:phases.append((p,'probe-'+p['name'],True))
    for repeat in range(3):
        for p in resources if repeat%2==0 else reversed(resources):phases.append((p,f'probe-{repeat}-'+p['name'],True))
    for p,stem,probe in phases:
        assert shutil.disk_usage(out).free>=3*2**30,'fixed-budget preflight'
        assert hashlib.sha256(Path(p['binary_override']).read_bytes()).hexdigest()==hashes[p['binary_override']]
        row=bench.invoke(p,out,stem,probe=probe)
        with (out/'results.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
        print(stem,row.get('error','OK'),f"{row['wall_seconds']:.3f}s",flush=True)
        if row.get('error'):raise SystemExit('Failure retained; diagnose before continuing')
    (out/'memory-after.txt').write_text(Path('/sys/fs/cgroup/memory.events').read_text())


if __name__=='__main__':main()
