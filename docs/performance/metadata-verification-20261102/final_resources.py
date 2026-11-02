import json,subprocess,hashlib
from pathlib import Path
root=Path('/workspace/scratch/metadata-verification');out=root/'performance';repo=Path('/workspace/varlit')
rows=[]
for side,bins in [('before',Path('/workspace/target-verification-before/release/examples')),('after',root/'final-bin')]:
 for name,args in [('forward',['writer','fixed','forward','2000000','200000']),('rewrite',['rewrite','variable','random','200000','20000']),('large',['writer','large','reverse','65536','4096'])]:
  label=f'final-resources-{side}-{name}'
  command=['python3',str(repo/'docs/performance/audit-followup-20261102/resource_probe.py'),'--output',str(out/(label+'.json')),str(bins/'record_mutation_load'),*args]
  with (out/(label+'.log')).open('w') as log:code=subprocess.run(command,stdout=log,stderr=subprocess.STDOUT).returncode
  assert code==0
  rows.append(dict(name=label,command=command,exit=code))
(out/'final-resource-jobs.json').write_text(json.dumps(rows,indent=2)+'\n')
(out/'final-binary-hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (root/'final-bin').iterdir()},indent=2)+'\n')
(out/'final-resource-events.json').write_text(json.dumps({k:Path('/sys/fs/cgroup/'+k).read_text() for k in ['cpu.max','memory.max','memory.events']},indent=2)+'\n')
