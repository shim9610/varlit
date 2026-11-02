import json,os,subprocess,statistics,sys
from pathlib import Path
root=Path('/workspace/scratch/ownership-contracts');stage,previous=sys.argv[1:3];out=root/(stage+'-owner-perf');out.mkdir(exist_ok=True)
bins={side:root/(name+'-bin/ownership_boundary_load') for side,name in [('before',previous),('after',stage)]};measure=root/'box-perf/measure-child'
os.sched_setaffinity(0,{min(os.sched_getaffinity(0))})
samples=[];summaries=[]
for mode,iterations in [('box',10000000),('nested-box',8000),('shared',10000000)]:
 selected=[]
 def pairs(start,end):
  for pair in range(start,end):
   for side in (['before','after'] if pair%2==0 else ['after','before']):
    name=f'{mode}-{pair}-{side}';usage=out/(name+'.json');cmd=[str(measure),str(usage),str(bins[side]),mode,str(iterations)]
    r=subprocess.run(cmd,text=True,capture_output=True,check=True,timeout=120)
    row=dict(case=mode,pair=pair,side=side,usage=json.loads(usage.read_text()),result=json.loads(r.stdout),command=cmd);samples.append(row);selected.append(row)
    (out/'samples.json').write_text(json.dumps(samples,indent=2)+'\n');print(name,row['result']['seconds'],flush=True)
 def summary():
  assert len({(r['result']['iterations'],r['result']['sum']) for r in selected})==1
  m={}
  for k,fn,agg in [('operation',lambda r:r['result']['seconds'],statistics.median),('cpu',lambda r:r['usage']['cpu'],statistics.median),('rss',lambda r:r['usage']['rss'],max)]:
   a,b=[agg(fn(r) for r in selected if r['side']==s) for s in ['before','after']];m[k]=dict(before=a,after=b,change_percent=100*(b/a-1))
  return dict(case=mode,pairs=len(selected)//2,metrics=m)
 pairs(0,5)
 if any(v['change_percent']>5 for v in summary()['metrics'].values()):pairs(5,20)
 summaries.append(summary());(out/'summary.json').write_text(json.dumps(summaries,indent=2)+'\n')
print('DONE',flush=True)
