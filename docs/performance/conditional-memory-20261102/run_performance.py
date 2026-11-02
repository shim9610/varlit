import hashlib,json,os,subprocess,time,shutil
from pathlib import Path
repo=Path('/workspace/varlit');root=Path('/workspace/scratch/conditional-memory');out=root/'performance';out.mkdir(exist_ok=True)
before=root/'before-bin';after=root/'after-bin';before.mkdir(exist_ok=True);after.mkdir(exist_ok=True)
for n in ['record_mutation_load','oom_maintenance_load','scalable_load']:
 shutil.copy2(Path('/workspace/scratch/metadata-verification/final-bin')/n,before/n)
 shutil.copy2(repo/'target/release/examples'/n,after/n)
shutil.copy2('/workspace/target-memory-before/release/examples/decoder_memory_load',before/'decoder_memory_load')
shutil.copy2(repo/'target/release/examples/decoder_memory_load',after/'decoder_memory_load')
host={k:Path('/sys/fs/cgroup'/Path(k)).read_text() for k in ['cpu.max','memory.max','memory.events']}
host['binaries']={s:{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in d.iterdir()} for s,d in [('before',before),('after',after)]}
host['baseline']='50866e5df7bd5b49b3c3e1871400ee6fe90e9674';(out/'host.json').write_text(json.dumps(host,indent=2)+'\n')
jobs=[]
def run(name,command):
 print('START',name,flush=True);start=time.monotonic()
 with (out/(name+'.log')).open('w') as log:code=subprocess.run(command,cwd=repo,stdout=log,stderr=subprocess.STDOUT).returncode
 jobs.append(dict(name=name,command=command,exit=code,seconds=time.monotonic()-start));(out/'jobs.json').write_text(json.dumps(jobs,indent=2)+'\n')
 if code:raise SystemExit(code)
common=['--before',str(before),'--after',str(after),'--repo',str(repo)]
native=['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','native']
decoder=['python3',str(repo/'docs/performance/conditional-memory-20261102/decoder_bench.py'),'--before',str(before/'decoder_memory_load'),'--after',str(after/'decoder_memory_load')]
for group,command,file,flag in [('native',native,'native-summary.json','--native-cases'),('decoder',decoder,'summary.json','--cases')]:
 run(group,[*command,'--output',str(out/group),'--pairs','5'])
 rows=json.loads((out/group/file).read_text());cases=[r['case'] for r in rows if any(v['change_percent'] is not None and v['change_percent']>5 for v in r['metrics'].values())]
 if cases:run(group+'-confirmation',[*command,'--output',str(out/(group+'-confirmation')),'--pairs','15',flag,*map(str,cases)])
run('swmr',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','concurrent','--output',str(out/'swmr'),'--pairs','3'])
for side,bins in [('before',before),('after',after)]:
 for name,binary,args in [('forward','record_mutation_load',['writer','fixed','forward','2000000','200000']),('rewrite','record_mutation_load',['rewrite','variable','random','200000','20000']),('large','record_mutation_load',['writer','large','reverse','65536','4096']),('complex','oom_maintenance_load',['complex','65536','512']),('hashmap','decoder_memory_load',['fixed','1048576','8'])]:
  label='resources-'+side+'-'+name
  run(label,['python3',str(repo/'docs/performance/audit-followup-20261102/resource_probe.py'),'--output',str(out/(label+'.json')),str(bins/binary),*args])
host['memory.events_after']=Path('/sys/fs/cgroup/memory.events').read_text();(out/'host.json').write_text(json.dumps(host,indent=2)+'\n')
print('DONE',flush=True)
