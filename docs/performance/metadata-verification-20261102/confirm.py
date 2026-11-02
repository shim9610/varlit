import json,os,subprocess,time
from pathlib import Path
repo=Path('/workspace/varlit');root=Path('/workspace/scratch/metadata-verification/performance')
assert len(json.loads((root/'jobs.json').read_text())) == 12, 'initial suite must finish first'
common=['--before','/workspace/target-verification-before/release/examples','--after','/workspace/varlit/target/release/examples','--repo',str(repo)]
report=[]
for group,summary,script in [('native','native-summary.json','bench.py'),('standalone','summary.json','extra.py'),('policy','native-summary.json','bench.py')]:
 rows=json.loads((root/group/summary).read_text())
 cases=[r['case'] for r in rows if any(m['change_percent'] is not None and m['change_percent']>5 for m in r['metrics'].values())]
 if not cases:continue
 output=root/(group+'-confirmation')
 command=['python3',str(repo/'docs/performance/audit-followup-20261102'/script),*common,'--output',str(output),'--pairs','15']
 command+=['--cases',*map(str,cases)] if group=='standalone' else ['--phase','native','--native-cases',*map(str,cases)]
 overrides={'VARLIT_SPILL_DIR':str(root/'spill-policy'),'VARLIT_SPILL_MAX_BYTES':'33554432'} if group=='policy' else {}
 print('CONFIRM',group,cases,flush=True);start=time.monotonic()
 with (root/(group+'-confirmation.log')).open('w') as log:
  code=subprocess.run(command,cwd=repo,env={**os.environ,**overrides},stdout=log,stderr=subprocess.STDOUT).returncode
 report.append(dict(group=group,cases=cases,reason='Initial increase above 5% in any operation/resource metric',pairs=15,command=command,environment_overrides=overrides,exit=code,seconds=time.monotonic()-start))
 (root/'confirmation-jobs.json').write_text(json.dumps(report,indent=2)+'\n')
 if code:raise SystemExit(code)
print('Confirmation measurements complete',flush=True)
