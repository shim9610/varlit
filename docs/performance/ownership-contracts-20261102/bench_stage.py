import os,subprocess,json,statistics,hashlib,sys
from pathlib import Path
root=Path('/workspace/scratch/ownership-contracts');repo=Path('/workspace/varlit');stage=sys.argv[1];previous=sys.argv[2];out=root/(stage+'-perf');out.mkdir(exist_ok=True)
bins={'before':root/(previous+'-bin'),'after':root/(stage+'-bin')}
def save(p,v):p.write_text(json.dumps(v,indent=2)+'\n')
host={n:Path('/sys/fs/cgroup',n).read_text() for n in ['cpu.max','memory.max','memory.events']};host['binaries']={s:{f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in d.iterdir()} for s,d in bins.items()};save(out/'host.json',host)
jobs=[]
def run(name,cmd):
 print('START',name,flush=True)
 with (out/(name+'.log')).open('w') as f:r=subprocess.run(cmd,cwd=repo,stdout=f,stderr=subprocess.STDOUT)
 jobs.append(dict(name=name,command=list(map(str,cmd)),exit=r.returncode));save(out/'jobs.json',jobs);print('END',name,r.returncode,flush=True);return r.returncode
measure=out/'measure-child';subprocess.run(['cc','-O2',str(repo/'docs/performance/boundary-redesign-20261101/measure-child.c'),'-o',str(measure)],check=True)
affinity=os.sched_getaffinity(0);os.sched_setaffinity(0,{min(affinity)})
samples=[];summaries=[]
for mode,count,it in [('bytes',67108864,16),('bytes',1073741824,2),('nested',16384,16)]:
 name=f'{mode}-{count}';selected=[]
 def pairs(start,end):
  for pair in range(start,end):
   for side in (['before','after'] if pair%2==0 else ['after','before']):
    label=f'{name}-{pair}-{side}';usage=out/(label+'.json')
    assert run(label,[str(measure),str(usage),str(bins[side]/'ownership_load'),mode,str(count),str(it)])==0
    row=dict(case=name,pair=pair,side=side,result=json.loads((out/(label+'.log')).read_text()),usage=json.loads(usage.read_text()));selected.append(row);samples.append(row);save(out/'samples.json',samples)
 def summary():
  assert len({(r['result']['count'],r['result']['iterations'],r['result']['sum']) for r in selected})==1
  metrics={}
  for key,fn,agg in [('operation',lambda r:r['result']['seconds'],statistics.median),('cpu',lambda r:r['usage']['cpu'],statistics.median),('rss',lambda r:r['usage']['rss'],max),('writes',lambda r:r['usage']['write_blocks'],statistics.median)]:
   a,b=[agg(fn(r) for r in selected if r['side']==s) for s in ['before','after']];metrics[key]=dict(before=a,after=b,change_percent=100*(b/a-1) if a else None)
  return dict(case=name,pairs=len(selected)//2,metrics=metrics)
 pairs(0,5)
 if any((v['change_percent'] or 0)>5 for v in summary()['metrics'].values()):pairs(5,20)
 summaries.append(summary());save(out/'summary.json',summaries)
os.sched_setaffinity(0,affinity)
common=['--before',str(bins['before']),'--after',str(bins['after']),'--repo',str(repo)]
assert run('native',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','native','--native-cases','0','4','7','9','10','--pairs','5','--output',str(out/'native')])==0
flagged=[r['case'] for r in json.loads((out/'native/native-summary.json').read_text()) if any((m['change_percent'] or 0)>5 for m in r['metrics'].values())]
if flagged:assert run('native-confirmation',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','native','--native-cases',*map(str,flagged),'--pairs','15','--output',str(out/'native-confirmation')])==0
run('swmr',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','concurrent','--pairs','3','--output',str(out/'swmr')])
for side in ['before','after']:
 for name,command in [('complex',[str(bins[side]/'oom_maintenance_load'),'complex','65536','512']),('clone-large',[str(bins[side]/'ownership_load'),'bytes','1073741824','2'])]:
  assert run('resources-'+name+'-'+side,['python3',str(repo/'docs/performance/audit-followup-20261102/resource_probe.py'),'--output',str(out/('resources-'+name+'-'+side+'.json')),*command])==0
host['memory.events_after']=Path('/sys/fs/cgroup/memory.events').read_text();save(out/'host.json',host);print('DONE',flush=True)
