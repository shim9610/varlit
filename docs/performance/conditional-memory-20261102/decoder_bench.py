#!/usr/bin/env python3
"""Alternating equal-work benchmarks for the changed HashMap decoder path."""
import argparse, hashlib, json, os, statistics, subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--before',type=Path,required=True);p.add_argument('--after',type=Path,required=True)
p.add_argument('--output',type=Path,required=True);p.add_argument('--pairs',type=int,default=5)
p.add_argument('--cases',nargs='+',type=int,default=[0,1,2]);a=p.parse_args()
root=Path(__file__).resolve().parents[3];a.output.mkdir(parents=True,exist_ok=True)
os.sched_setaffinity(0,{min(os.sched_getaffinity(0))})
measure=a.output/'measure-child'
subprocess.run(['cc','-O2',str(root/'docs/performance/boundary-redesign-20261101/measure-child.c'),'-o',str(measure)],check=True)
bins={k:v.resolve() for k,v in [('before',a.before),('after',a.after)]}
(a.output/'binary-hashes.json').write_text(json.dumps({k:hashlib.sha256(v.read_bytes()).hexdigest() for k,v in bins.items()},indent=2)+'\n')
cases=[['fixed','1048576','8'],['strings','262144','6'],['nested','8192','6']]
rows=[];summary=[]
for case in a.cases:
 selected=[]
 for pair in range(a.pairs):
  for side in (['before','after'] if pair%2==0 else ['after','before']):
   usage=a.output/'usage.json';cmd=[str(measure),str(usage),str(bins[side]),*cases[case]]
   result=subprocess.run(cmd,capture_output=True,text=True,check=True,timeout=600)
   row=dict(case=case,pair=pair,side=side,command=cmd,result=json.loads(result.stdout),usage=json.loads(usage.read_text()))
   selected.append(row);rows.append(row)
   (a.output/'samples.json').write_text(json.dumps(rows,indent=2)+'\n')
 work=[{k:v for k,v in r['result'].items() if k!='seconds'} for r in selected];assert all(w==work[0] for w in work)
 metrics={}
 for name,get,aggregate in [('operation_seconds',lambda r:r['result']['seconds'],statistics.median),('cpu_seconds',lambda r:r['usage']['cpu'],statistics.median),('peak_rss_kib',lambda r:r['usage']['rss'],max),('write_bytes',lambda r:r['usage']['write_blocks']*512,statistics.median)]:
  values={s:aggregate(get(r) for r in selected if r['side']==s) for s in bins}
  metrics[name]={**values,'change_percent':((values['after']/values['before']-1)*100 if values['before'] else (0 if not values['after'] else None))}
 summary.append(dict(case=case,pairs=a.pairs,work=work[0],metrics=metrics))
 (a.output/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
 print(case,metrics,flush=True)
