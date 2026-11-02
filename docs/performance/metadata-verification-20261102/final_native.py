import json,os,subprocess,time
from pathlib import Path
repo=Path('/workspace/varlit');root=Path('/workspace/scratch/metadata-verification/performance')
common=['--before','/workspace/target-verification-before/release/examples','--after','/workspace/scratch/metadata-verification/final-bin','--repo',str(repo)]
script=repo/'docs/performance/audit-followup-20261102/bench.py'
rows=[]
for group,cases,pairs,env in [('final-native',list(range(7)),5,{}),('final-policy',[0,2],3,{'VARLIT_SPILL_DIR':str(root/'spill-policy'),'VARLIT_SPILL_MAX_BYTES':'33554432'})]:
 command=['python3',str(script),*common,'--output',str(root/group),'--phase','native','--pairs',str(pairs),'--native-cases',*map(str,cases)]
 print('START',group,flush=True);start=time.monotonic()
 with (root/(group+'.log')).open('w') as log:code=subprocess.run(command,cwd=repo,env={**os.environ,**env},stdout=log,stderr=subprocess.STDOUT).returncode
 rows.append(dict(group=group,command=command,environment_overrides=env,exit=code,seconds=time.monotonic()-start,reason='Final rebuild changed record_mutation_load binary; other three binaries match exactly'))
 (root/'final-native-jobs.json').write_text(json.dumps(rows,indent=2)+'\n')
 if code:raise SystemExit(code)
 selected=json.loads((root/group/'native-summary.json').read_text())
 confirm=[r['case'] for r in selected if any(v['change_percent'] is not None and v['change_percent']>5 for v in r['metrics'].values())]
 if confirm:
  command=['python3',str(script),*common,'--output',str(root/(group+'-confirmation')),'--phase','native','--pairs','15','--native-cases',*map(str,confirm)]
  print('CONFIRM',group,confirm,flush=True);start=time.monotonic()
  with (root/(group+'-confirmation.log')).open('w') as log:code=subprocess.run(command,cwd=repo,env={**os.environ,**env},stdout=log,stderr=subprocess.STDOUT).returncode
  rows.append(dict(group=group+'-confirmation',command=command,environment_overrides=env,exit=code,seconds=time.monotonic()-start,reason='Final binary initial sample increase >5%; retain all observations'))
  (root/'final-native-jobs.json').write_text(json.dumps(rows,indent=2)+'\n')
  if code:raise SystemExit(code)
print('Final binary measurements complete',flush=True)
