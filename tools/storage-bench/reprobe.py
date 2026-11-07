#!/usr/bin/env python3
"""Repeat only resource probes with each cohort's frozen benchmark binaries."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import run as bench


def sha(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        while block:=f.read(1<<20):h.update(block)
    return h.hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--evidence-root',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--resume',action='store_true')
    args=p.parse_args();root=args.evidence_root.resolve();out=args.output.resolve();out.mkdir(parents=True,exist_ok=args.resume)
    cpus=sorted(os.sched_getaffinity(0))[:4];os.sched_setaffinity(0,cpus)
    assert Path('/sys/fs/cgroup/memory.max').read_text().strip()!='max'
    assert int(Path('/sys/fs/cgroup/memory.max').read_text())<=16*2**30
    if not args.resume:subprocess.run(['cc','-O2',str(bench.HERE/'measure.c'),'-o',str(out/'measure')],check=True)
    cohorts=['full-v4','packed-v1','hdf-mixed-v1','mapped-arrays-v1','mapped-warm-v1']
    profiles=[]
    for name in cohorts:
        source=root/name;manifest=json.loads((source/'manifest.json').read_text())
        for original in manifest['profiles']:
            q=dict(original);binary='mixed' if q['kind']=='mixed' else 'arrays';exe=source/'binaries'/binary
            assert sha(exe)==manifest['sha256']['target/release/'+binary],(name,binary)
            q.update(binary_override=str(exe),cohort=name)
            profiles.append(q)
    assert len({q['name'] for q in profiles})==len(profiles)
    save=lambda p,v:p.write_text(json.dumps(v,indent=2)+'\n')
    save(out/('resume-manifest.json' if args.resume else 'manifest.json'),dict(profiles=profiles,affinity=cpus,interval_ms=50,source_sha256=sha(bench.HERE/'run.py'),
                                 rationale='Original /proc/PID/task/PID/children files are absent on this kernel; discover descendants by stat PPid. Include direct array workload paths.',
                                 benchmark_binaries='exact hashes from each original cohort',started=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())))
    for file in ['run.py','reprobe.py','measure.c']:shutil.copy2(bench.HERE/file,out/('resume-'+file if args.resume else file))
    if not args.resume:(out/'memory-before.txt').write_text(Path('/sys/fs/cgroup/memory.events').read_text())
    completed={json.loads(x)['profile']['name'] for x in (out/'results.jsonl').read_text().splitlines()} if args.resume else set()
    for q in profiles:
        if q['name'] in completed:continue
        stem='probe-0-'+q['name']
        if args.resume:
            for file in out.glob(stem+'.*'):file.rename(out/('unqualified-'+file.name))
        if shutil.disk_usage(out).free<3*2**30:raise SystemExit('fixed disk preflight: less than 3 GiB free')
        row=bench.invoke(q,out,stem,probe=True)
        row['sample_interval_ms']=5 if q['kind']=='array' and q['total']<=16<<20 else 50
        if not row.get('error'):
            expected=q['config']['readers']+2 if q['kind']=='mixed' else 2
            assert row['sampled_peaks']['sampled_processes']==expected,(q['name'],row['sampled_peaks'])
            assert row['sampled_peaks']['rss_sum']>1<<20,(q['name'],row['sampled_peaks'])
            assert row['sampled_peaks']['visible_allocated']>0,(q['name'],row['sampled_peaks'])
        with (out/'results.jsonl').open('a')as f:f.write(json.dumps(row)+'\n')
        print(q['name'],row.get('error','OK'),flush=True)
    (out/'memory-after.txt').write_text(Path('/sys/fs/cgroup/memory.events').read_text())


if __name__=='__main__':main()
