import json,statistics
from pathlib import Path
root=Path('/workspace/scratch/metadata-verification/performance')
pooled={}
for group,file in [('native','native-samples.json'),('standalone','samples.json'),('policy','native-samples.json')]:
 rows=[]
 for stage in ['initial','confirmation']:
  p=root/(group if stage=='initial' else group+'-confirmation')/file
  if p.exists():rows += [dict(row,stage=stage) for row in json.loads(p.read_text())]
 result=[]
 for case in sorted({r['case'] for r in rows}):
  selected=[r for r in rows if r['case']==case]
  work=[{k:v for k,v in r['result'].items() if k!='seconds'} for r in selected]
  assert all(w==work[0] for w in work)
  metrics={}
  for name,get,aggregate in [('operation_seconds',lambda r:r['result']['seconds'],statistics.median),('cpu_seconds',lambda r:r['usage']['cpu'],statistics.median),('peak_rss_kib',lambda r:r['usage']['rss'],max)]:
   values={s:aggregate(get(r) for r in selected if r['side']==s) for s in ['before','after']}
   metrics[name]={**values,'change_percent':(values['after']/values['before']-1)*100}
  result.append(dict(case=case,pairs=len(selected)//2,work=work[0],metrics=metrics))
 pooled[group]=result
 (root/(group+'-pooled-samples.json')).write_text(json.dumps(rows,indent=2)+'\n')
(root/'pooled-summary.json').write_text(json.dumps(pooled,indent=2)+'\n')
for group,rows in pooled.items():
 for r in rows:print(group,r['case'],r['pairs'],{k:round(v['change_percent'],2) for k,v in r['metrics'].items()})
