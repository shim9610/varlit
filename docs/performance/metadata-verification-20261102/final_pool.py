import json,statistics
from pathlib import Path
root=Path('/workspace/scratch/metadata-verification/performance')
pooled=json.loads((root/'pooled-summary.json').read_text())
for group,cases in [('native',range(7)),('policy',[0,2])]:
 rows=[]
 for stage in ['final-'+group,'final-'+group+'-confirmation']:
  p=root/stage/'native-samples.json'
  if p.exists():rows += [dict(row,stage=stage) for row in json.loads(p.read_text())]
 for case in cases:
  selected=[r for r in rows if r['case']==case]
  work=[{k:v for k,v in r['result'].items() if k!='seconds'} for r in selected];assert all(w==work[0] for w in work)
  metrics={}
  for name,get,aggregate in [('operation_seconds',lambda r:r['result']['seconds'],statistics.median),('cpu_seconds',lambda r:r['usage']['cpu'],statistics.median),('peak_rss_kib',lambda r:r['usage']['rss'],max)]:
   values={s:aggregate(get(r) for r in selected if r['side']==s) for s in ['before','after']}
   metrics[name]={**values,'change_percent':(values['after']/values['before']-1)*100}
  result=dict(case=case,pairs=len(selected)//2,work=work[0],metrics=metrics,source='final frozen record_mutation_load; earlier binary samples excluded')
  pooled[group]=[result if old['case']==case else old for old in pooled[group]]
 (root/('final-'+group+'-pooled-samples.json')).write_text(json.dumps(rows,indent=2)+'\n')
(root/'qualified-summary.json').write_text(json.dumps(pooled,indent=2)+'\n')
for group,rows in pooled.items():
 for r in rows:print(group,r['case'],r['pairs'],{k:round(v['change_percent'],2) for k,v in r['metrics'].items()})
print('qualified_observations',sum(r['pairs']*2 for rows in pooled.values() for r in rows)+12)
