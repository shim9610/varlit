import subprocess,os,time,json,statistics,hashlib,shutil
from pathlib import Path
root=Path('/workspace/scratch/chunk-stream'); repo=Path('/workspace/varlit');out=root/'performance';out.mkdir(exist_ok=True)
bins={s:root/(s+'-bin') for s in ['before','after']}
def save(p,d):p.write_text(json.dumps(d,indent=2)+'\n')
host={n:Path('/sys/fs/cgroup',n).read_text() for n in ['cpu.max','memory.max','memory.events']};host['baseline']='b837577cca7a2436a1da0074e94149c003503680';host['binaries']={s:{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in d.iterdir()} for s,d in bins.items()};save(out/'host.json',host)
jobs=[]
def run(name,cmd):
 print('START',name,flush=True);start=time.monotonic()
 with (out/(name+'.log')).open('w') as f:r=subprocess.run(cmd,cwd=repo,stdout=f,stderr=subprocess.STDOUT)
 jobs.append(dict(name=name,command=list(map(str,cmd)),exit=r.returncode,seconds=time.monotonic()-start));save(out/'jobs.json',jobs)
 print('END',name,r.returncode,flush=True)
 return r.returncode
measure=out/'measure-child';subprocess.run(['cc','-O2',str(repo/'docs/performance/boundary-redesign-20261101/measure-child.c'),'-o',str(measure)],check=True)
for label,length,chunk in [('small',67108864,1048576),('large',1073741824,1048576),('single',67108864,67108864),('random',268435456,1048576)]:
 assert run('prepare-'+label,[str(bins['before']/'chunk_stream_load'),'prepare',str(out/(label+'.bin')),str(length),str(chunk),*(["random"] if label=="random" else [])])==0
cases=[('unchanged-full','small',67108864,0,67108864,8,1,'full'),('full-stream','small',67108864,0,67108864,8,1,'stream'),('large-full-stream','large',1073741824,0,1073741824,2,1,'stream'),('large-range','large',1073741824,123456789,16777253,4,1,'stream'),('concurrent-range','large',1073741824,123456789,16777253,2,8,'stream'),('large-single-chunk','single',67108864,12345,1048579,8,1,'stream')]
cases.append(('tiny-buffer','small',67108864,0,67108864,4,1,'stream-small'))
cases.append(('incompressible-range','random',268435456,33554431,33554439,4,1,'stream'))
samples=[]
def pairs(case,first,last):
 name,fixture,length,start,count,it,readers,mode=case
 for pair in range(first,last):
  for side in (['before','after'] if pair%2==0 else ['after','before']):
   label=f'{name}-{pair}-{side}';resource=out/(label+'.json');cmd=[str(measure),str(resource),str(bins[side]/'chunk_stream_load'),'full' if side=='before' else mode,str(out/(fixture+'.bin')),str(length),str(start),str(count),str(it),str(readers),*(["random"] if fixture=="random" else [])]
   assert run(label,cmd)==0
   row=json.loads((out/(label+'.log')).read_text().splitlines()[-1]); row.update(json.loads(resource.read_text()));row.update(name=name,pair=pair,side=side);samples.append(row);save(out/'samples.json',samples)
def summary(case):
 rows=[r for r in samples if r['name']==case[0]];d={}
 for k in ['seconds','cpu','rss','write_blocks']:
  fn=max if k=='rss' else statistics.median
  a,b=[fn([r[k] for r in rows if r['side']==s]) for s in ['before','after']]
  d[k]={'before':a,'after':b,'change_percent':100*(b/a-1) if a else None}
 assert len({(r['length'],r['start'],r['count'],r['iterations'],r['readers'],r['crc']) for r in rows})==1
 return dict(name=case[0],pairs=len(rows)//2,metrics=d)
affinity=os.sched_getaffinity(0)
for case in cases:
 os.sched_setaffinity(0,affinity if case[6]>1 else {min(affinity)})
 pairs(case,0,5)
 if any((x['change_percent'] or 0)>5 for x in summary(case)['metrics'].values()):pairs(case,5,20)
 save(out/'summary.json',[summary(c) for c in cases if any(r['name']==c[0] for r in samples)])
os.sched_setaffinity(0,affinity)
common=['--before',str(bins['before']),'--after',str(bins['after']),'--repo',str(repo)]
run('native',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','native','--native-cases','0','4','7','9','10','--pairs','5','--output',str(out/'native')])
native_rows=json.loads((out/'native/native-summary.json').read_text())
flagged=[r['case'] for r in native_rows if any((v['change_percent'] or 0)>5 for v in r['metrics'].values())]
if flagged:
 run('native-confirmation',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','native','--native-cases',*map(str,flagged),'--pairs','15','--output',str(out/'native-confirmation')])
run('swmr',['python3',str(repo/'docs/performance/audit-followup-20261102/bench.py'),*common,'--phase','concurrent','--pairs','3','--output',str(out/'swmr')])
for side in ['before','after']:
 for name,fixture,length,start,count,it,readers,mode in [cases[2],cases[4],cases[5]]:
  run('resources-'+name+'-'+side,['python3',str(repo/'docs/performance/audit-followup-20261102/resource_probe.py'),'--output',str(out/('resources-'+name+'-'+side+'.json')),str(bins[side]/'chunk_stream_load'),'full' if side=='before' else mode,str(out/(fixture+'.bin')),str(length),str(start),str(count),str(it),str(readers),*(["random"] if fixture=="random" else [])])
for side in ['before','after']:
 run('resources-complex-'+side,['python3',str(repo/'docs/performance/audit-followup-20261102/resource_probe.py'),'--output',str(out/('resources-complex-'+side+'.json')),str(bins[side]/'oom_maintenance_load'),'complex','65536','512'])
host['memory.events_after']=Path('/sys/fs/cgroup/memory.events').read_text();save(out/'host.json',host)
print('DONE',flush=True)
