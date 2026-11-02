#!/usr/bin/env python3
"""Sequential equal-work qualification; start only after builds/checks stop."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--before', type=Path, required=True)
p.add_argument('--after', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
a=p.parse_args()
repo=Path(__file__).resolve().parents[3]
a.output.mkdir(parents=True,exist_ok=True)
prior=repo/'docs/performance/audit-followup-20261102'
policy=a.output/'spill-policy';policy.mkdir(exist_ok=True)
host=dict(platform=platform.platform(),cpu_max=Path('/sys/fs/cgroup/cpu.max').read_text().strip(),
          memory_max=Path('/sys/fs/cgroup/memory.max').read_text().strip(),affinity=sorted(os.sched_getaffinity(0)),
          memory_events_before=Path('/sys/fs/cgroup/memory.events').read_text(),
          baseline='b2ab2c9ea776ab991981c8a3ddd1104f587a4594',
          binaries={side:{name:hashlib.sha256((path/name).read_bytes()).hexdigest() for name in ['record_mutation_load','oom_maintenance_load','scalable_load','decompression_load']} for side,path in [('before',a.before),('after',a.after)]})
(a.output/'host.json').write_text(json.dumps(host,indent=2)+'\n')
jobs=[]
def run(name,command,overrides=None):
    print('START',name,flush=True);start=time.monotonic()
    with (a.output/(name+'.log')).open('w') as log:
        code=subprocess.run(command,cwd=repo,env={**os.environ,**(overrides or {})},stdout=log,stderr=subprocess.STDOUT).returncode
    jobs.append(dict(name=name,command=list(map(str,command)),environment_overrides=overrides or {},exit=code,seconds=time.monotonic()-start))
    (a.output/'jobs.json').write_text(json.dumps(jobs,indent=2)+'\n')
    print('DONE',name,code,flush=True)
    if code:raise SystemExit(code)
common=['--before',str(a.before),'--after',str(a.after),'--repo',str(repo)]
run('native',['python3',str(prior/'bench.py'),*common,'--output',str(a.output/'native'),'--pairs','5','--phase','native'])
run('standalone',['python3',str(prior/'extra.py'),*common,'--output',str(a.output/'standalone'),'--pairs','5'])
run('swmr',['python3',str(prior/'bench.py'),*common,'--output',str(a.output/'swmr'),'--pairs','3','--phase','concurrent'])
run('policy',['python3',str(prior/'bench.py'),*common,'--output',str(a.output/'policy'),'--pairs','3','--phase','native','--native-cases','0','2','7'],
    {'VARLIT_SPILL_DIR':str(policy),'VARLIT_SPILL_MAX_BYTES':'33554432'})
for side,bins in [('before',a.before),('after',a.after)]:
    for name,example,args in [
        ('forward','record_mutation_load',['writer','fixed','forward','2000000','200000']),
        ('rewrite','record_mutation_load',['rewrite','variable','random','200000','20000']),
        ('large','record_mutation_load',['writer','large','reverse','65536','4096']),
        ('complex','oom_maintenance_load',['complex','65536','512'])]:
        label=f'resources-{side}-{name}'
        run(label,['python3',str(prior/'resource_probe.py'),'--output',str(a.output/(label+'.json')),str(bins/example),*args])
host['memory_events_after']=Path('/sys/fs/cgroup/memory.events').read_text()
(a.output/'host.json').write_text(json.dumps(host,indent=2)+'\n')
print('Measurements complete; inspect initial signals before deciding confirmations.',flush=True)
