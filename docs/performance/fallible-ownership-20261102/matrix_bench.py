import os,sys,json,subprocess,statistics,hashlib
from pathlib import Path
root=Path('/workspace/scratch/fallible-ownership'); stage,previous=sys.argv[1:3];out=root/(stage+'-matrix-perf');out.mkdir(exist_ok=True)
data=out/'disk-data';data.mkdir(exist_ok=True)
env={**os.environ,'TMPDIR':str(data)}
(out/'host.json').write_text(json.dumps({**{n:Path('/sys/fs/cgroup',n).read_text() for n in ['cpu.max','memory.max','memory.events']},'TMPDIR':str(data),'filesystem':subprocess.check_output(['df','-T',str(data)],text=True)},indent=2)+'\n')
measure=root/'stage1-perf/measure-child';bins={s:root/(v+'-bin/matrix_ownership_load') for s,v in [('before',previous),('after',stage)]}
samples=[]; summaries=[]
for mode in ['ordinary','durable']:
 selected=[]
 def pairs(start,end):
  for pair in range(start,end):
   for side in (['before','after'] if pair%2==0 else ['after','before']):
    name=f'{mode}-{pair}-{side}';usage=out/(name+'.json');cmd=[str(measure),str(usage),str(bins[side]),'8192','64','8','512',mode]
    with (out/(name+'.log')).open('w') as f:r=subprocess.run(cmd,stdout=f,stderr=subprocess.STDOUT,env=env,timeout=180)
    assert r.returncode==0,name
    row=dict(case=mode,pair=pair,side=side,usage=json.loads(usage.read_text()),result=json.loads((out/(name+'.log')).read_text()));selected.append(row);samples.append(row)
    (out/'samples.json').write_text(json.dumps(samples,indent=2)+'\n')
    print(name,row['result']['seconds'],flush=True)
 def summary():
  assert len({(r['result']['rows'],r['result']['epochs'],r['result']['readers'],r['result']['checks'],r['result']['sum']) for r in selected})==1
  metrics={}
  for key,fn,agg in [('operation',lambda r:r['result']['seconds'],statistics.median),('cpu',lambda r:r['usage']['cpu'],statistics.median),('rss',lambda r:r['usage']['rss'],max),('writes',lambda r:r['usage']['write_blocks'],statistics.median)]:
   a,b=[agg(fn(r) for r in selected if r['side']==s) for s in ['before','after']];metrics[key]=dict(before=a,after=b,change_percent=100*(b/a-1) if a else None)
  return dict(case=mode,pairs=len(selected)//2,metrics=metrics)
 pairs(0,5)
 if any((m['change_percent'] or 0)>5 for m in summary()['metrics'].values()):pairs(5,20)
 summaries.append(summary());(out/'summary.json').write_text(json.dumps(summaries,indent=2)+'\n')
for side in bins:
 cmd=['python3','/workspace/varlit/docs/performance/audit-followup-20261102/resource_probe.py','--output',str(out/(side+'-resources.json')),str(bins[side]),'8192','64','8','512','durable']
 with (out/(side+'-resources.log')).open('w') as f:subprocess.run(cmd,stdout=f,stderr=subprocess.STDOUT,env=env,timeout=180,check=True)
print('DONE',flush=True)
