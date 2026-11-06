#!/usr/bin/env python3
"""Fixed-work raw/guarded mmap comparison; timings and resource probes stay separate."""
import argparse
import hashlib
import itertools
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import time
import run as bench


def profiles(before, after, smoke=False):
    cases=[]
    if smoke:
        shapes=[('session-live',32<<20,g,2,p,c) for g in [65536,1<<20] for p,c in [(1,False),(4,True)]]
        shapes += [('session-warm',32<<20,g,2,1,False) for g in [65536,1<<20]]
    else:
        shapes=[('session-live',1<<30,g,r,1,False) for g in [65536,1<<20] for r in [1,8]]
        shapes += [('session-live',1<<30,g,8,4,True) for g in [65536,1<<20]]
        shapes += [('session-warm',n,g,r,1,False) for n,r in [(128<<20,8),(1<<30,1),(1<<30,8)] for g in [65536,1<<20]]
    for phase,total,group,readers,producers,complex_ in shapes:
        workload=f'{phase}-n{total}-g{group}-r{readers}-p{producers}-c{int(complex_)}'
        for label in ['before-a','before-b','after-raw','after-guarded']:
            binary=before if label.startswith('before') else after
            cases.append(dict(name=f'{workload}--{label}',workload=workload,variant=label,kind='array',
                mode='session-guarded' if label=='after-guarded' else 'session-raw',total=total,group=group,
                page=65536,mmap=0,points=0,ranges=0,readers=readers,producers=producers,complex=complex_,
                command_tail=[phase,str(producers),'complex' if complex_ else 'fixed'],
                binary_override=str(binary.resolve()),read_validation='full'))
    return cases


def calibration_profiles(binary, new_alerts=False):
    """Three retained warm phase alerts; identical raw A/B and guarded binary."""
    selected = []
    for p in profiles(binary, binary):
        if p['readers'] != 8:
            continue
        warm = p['command_tail'][0] == 'session-warm'
        if new_alerts:
            keep = ((not warm and p['group'] == 1 << 20 and p['producers'] == 1)
                    or (warm and p['total'] == 128 << 20 and p['group'] == 65536))
        else:
            keep = warm and (p['total'] == 1 << 30 or p['group'] == 1 << 20)
        if not keep:
            continue
        if p['variant'] not in ['before-a', 'before-b', 'after-guarded']:
            continue
        p['variant'] = {'before-a': 'raw-a', 'before-b': 'raw-b',
                        'after-guarded': 'guarded'}[p['variant']]
        p['name'] = f'{p["workload"]}--{p["variant"]}'
        selected.append(p)
    assert len(selected) == (6 if new_alerts else 9)
    return selected


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--before',type=Path,required=True);ap.add_argument('--after',type=Path,required=True)
    ap.add_argument('--output',type=Path,required=True);ap.add_argument('--smoke',action='store_true')
    ap.add_argument('--calibration',action='store_true',help='Fixed 30-round same-binary warm phase calibration')
    ap.add_argument('--calibration-new-alerts',action='store_true',help='Fixed 30-round confirmation of two additional final-runtime alerts')
    args=ap.parse_args();out=args.output.resolve();out.mkdir(exist_ok=False)
    assert not (args.calibration and args.calibration_new_alerts)
    args.calibration = args.calibration or args.calibration_new_alerts
    if args.calibration:
        assert not args.smoke and args.before.resolve() == args.after.resolve()
    cpus=sorted(os.sched_getaffinity(0))[:4];os.sched_setaffinity(0,cpus)
    memory=Path('/sys/fs/cgroup/memory.max').read_text().strip();quota,period=Path('/sys/fs/cgroup/cpu.max').read_text().split()
    assert memory!='max' and int(memory)<=16*2**30
    assert quota!='max' and int(quota)<=4*int(period)
    assert shutil.disk_usage(out).total<=32*2**30
    subprocess.run(['cc','-O2',str(bench.HERE/'measure.c'),'-o',str(out/'measure')],check=True)
    grid=calibration_profiles(args.after,args.calibration_new_alerts) if args.calibration else profiles(args.before,args.after,args.smoke)
    repeats=30 if args.calibration else 1 if args.smoke else 7
    binaries={p['binary_override']:hashlib.sha256(Path(p['binary_override']).read_bytes()).hexdigest() for p in grid}
    sources=[bench.HERE/n for n in ['Cargo.toml','Cargo.lock','build.rs','run.py','compare_append_only.py','measure.c']]
    sources+=list(bench.HERE.glob('src/**/*.rs'))
    for path in sources:
        target=out/'source'/path.relative_to(bench.HERE);target.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,target)
    (out/'worktree.patch').write_bytes(subprocess.check_output(['git','diff','--binary'],cwd=bench.REPO))
    bench.save(out/'manifest.json',dict(profiles=grid,repeats=repeats,smoke=args.smoke,calibration=args.calibration,
        new_alerts=args.calibration_new_alerts,affinity=cpus,memory_max=memory,
        cpu_max=f'{quota} {period}',disk_budget_gib=32,seed=9610,binaries=binaries,
        baseline='a3a1e262d3a935cf2f16c1637e3eb626b13f1354',started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),
        scope='One process, one writer, 1/8 reader threads; live windows, static two-pass snapshots, fixed/variable packed frames and four bounded MPSC producers. Same byte oracles, payload and publication count. Before A/B are the identical binary.',
        limitations='Fresh writes warm OS cache. No privileged cache eviction. Separate 50 ms process/storage probes can miss brief peaks. Do not pool cohorts or infer a cross-process token protocol.'))
    events=Path('/sys/fs/cgroup/memory.events');(out/'memory-before.txt').write_text(events.read_text())
    rng=random.Random(9610);phases=[]
    balanced=list(itertools.permutations(['raw-a','raw-b','guarded']))*5
    if args.calibration:
        rng.shuffle(balanced)
    for repeat in range(repeats):
        workloads=sorted({p['workload'] for p in grid});rng.shuffle(workloads)
        for workload in workloads:
            variants=[p for p in grid if p['workload']==workload]
            if args.calibration:
                variants.sort(key=lambda p: balanced[repeat].index(p['variant']))
            else:
                rng.shuffle(variants)
            phases += [(p,f'timing-{repeat}-{p["name"]}',False) for p in variants]
    order=grid.copy();rng.shuffle(order);phases += [(p,f'probe-{p["name"]}',True) for p in order]
    for index,(p,stem,probe) in enumerate(phases,1):
        assert shutil.disk_usage(out).free>=3*2**30
        row=bench.invoke(p,out,stem,probe=probe)
        with (out/'results.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
        print(f'{index}/{len(phases)} {stem} {row.get("error","OK")} {row["wall_seconds"]:.3f}s',flush=True)
        assert not row.get('error'),'Failed attempt retained; diagnose before a new cohort'
        result=row['result'];passes=2 if p['command_tail'][0]=='session-warm' else 1
        assert len(result['reader_stats'])==p['readers']
        assert all(r['bytes']==p['total']*passes and r['frames']==p['total']//p['group']*passes for r in result['reader_stats'])
        assert result['metadata_records']==(p['total']//(16<<20)*2 if p['complex'] else 0)
        if probe:assert row['sampled_peaks']['sampled_processes']>=2
    for path,expected in binaries.items():assert hashlib.sha256(Path(path).read_bytes()).hexdigest()==expected
    (out/'memory-after.txt').write_text(events.read_text())
    before=dict(line.split() for line in (out/'memory-before.txt').read_text().splitlines())
    after=dict(line.split() for line in (out/'memory-after.txt').read_text().splitlines())
    assert all(before[k]==after[k] for k in ['oom','oom_kill'])


if __name__=='__main__':main()
