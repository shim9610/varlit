import os,pathlib,json,subprocess,statistics,hashlib
root=pathlib.Path(__file__).parent
old=pathlib.Path('/workspace/scratch/security-fixes-20261101')
bins={'regressed':root/'before-target/release/examples/record_mutation_load','original':old/'control-target/release/examples/record_mutation_load','after':root/'after-target/release/examples/record_mutation_load'}
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
provenance={'regressed':'ed92ba1e0eb3816d5b2c1590946bab07cd8b9931','original':'317fc981e5cb78eec26f6820c9e010baf2873bcb','candidate_production_diff_sha256':hashlib.sha256(subprocess.check_output(['git','diff','--','crates/varlit-core/src'],cwd='/workspace/varlit')).hexdigest(),'binaries':{k:sha(v) for k,v in bins.items()},'separate_targets':True,'harness_sha256':sha(pathlib.Path('/workspace/varlit/crates/varlit/examples/record_mutation_load.rs')),'cargo_lock_sha256':sha(pathlib.Path('/workspace/varlit/Cargo.lock'))}
assert len(set(provenance['binaries'].values()))==3
source=pathlib.Path('/workspace/varlit/crates/varlit/examples/record_mutation_load.rs').read_bytes()
for directory in [root/'before',old/'baseline']:
 assert (directory/'crates/varlit/examples/record_mutation_load.rs').read_bytes()==source
 assert (directory/'Cargo.lock').read_bytes()==pathlib.Path('/workspace/varlit/Cargo.lock').read_bytes()
 assert not subprocess.check_output(['git','diff','--','crates/varlit-core/src'],cwd=directory)
(root/'provenance.json').write_text(json.dumps(provenance,indent=2))
(root/'candidate-production.patch').write_bytes(subprocess.check_output(['git','diff','--','crates/varlit-core/src'],cwd='/workspace/varlit'))
os.sched_setaffinity(0,{min(os.sched_getaffinity(0))})
cases=[]
for n in [1024,4096]:
 for mode,layout,order in [('lazy','fixed','reverse'),('lazy','variable','random'),('nonresident','variable','reverse'),('recovery','fixed','forward')]:
  cases.append(('regressed','tmpfs',[mode,layout,order,str(n),str(n)]))
for mode,layout,order,n,m in [
 ('writer','fixed','forward',200000,20000),('writer','fixed','random',200000,20000),
 ('reopen','fixed','random',200000,20000),('writer','variable','random',200000,20000),('reopen','variable','random',200000,20000),('epochs','fixed','random',200000,20000),('segment','fixed','random',200000,20000),
 ('rewrite','fixed','random',200000,20000),('rewrite','variable','random',200000,20000),
 ('lazy','fixed','single',200000,1),('lazy','fixed','reverse',200000,20000),
 ('lazy','variable','random',200000,20000),('lazy','fixed','repeat',200000,20000),
 ('nonresident','variable','reverse',200000,20000),('recovery','fixed','forward',200000,20000),
 ('writer','fixed','forward',2000000,200000),('reopen','fixed','forward',2000000,200000)]:
 cases.append(('original','tmpfs',[mode,layout,order,str(n),str(m)]))
for mode in ['writer','lazy']:
 cases.append(('original','disk',[mode,'large','reverse','65536','4096']))
cases.append(('original','disk',['writer','mixed','random','131072','8192']))
(root/'disk-data').mkdir(exist_ok=True)
rows=[];summaries=[]
for baseline,storage,args in cases:
 print('CASE',baseline,storage,args,flush=True)
 env=dict(os.environ,TMPDIR='/tmp' if storage=='tmpfs' else str(root/'disk-data'))
 for pair in range(5):
  for side in (['before','after'] if pair%2==0 else ['after','before']):
   exe=bins[baseline if side=='before' else side]
   proc=subprocess.run([str(old/'measure-child'),str(root/'usage.json'),str(exe),*args],text=True,capture_output=True,env=env,timeout=300)
   if proc.returncode:raise RuntimeError((args,side,proc.stderr))
   result=json.loads(proc.stdout);usage=json.loads((root/'usage.json').read_text())
   row=dict(baseline=baseline,storage=storage,args=args,pair=pair,side=side,result=result,**usage,binary_sha256=sha(exe))
   rows.append(row);(root/'samples.json').write_text(json.dumps(rows,indent=2))
   print(pair,side,round(result['seconds'],6),'frames',result['frames'],'rss',usage['rss'],flush=True)
 selected=[r for r in rows if r['baseline']==baseline and r['storage']==storage and r['args']==args]
 work=[{k:v for k,v in r['result'].items() if k not in ['seconds','frames','temporary_bytes']} for r in selected]
 assert all(w==work[0] for w in work)
 summary=dict(baseline=baseline,storage=storage,args=args,pairs=5,work=work[0])
 for field,fn,agg in [('seconds',lambda r:r['result']['seconds'],statistics.median),('cpu',lambda r:r['cpu'],statistics.median),('rss_median_kib',lambda r:r['rss'],statistics.median),('rss_max_kib',lambda r:r['rss'],max),('write_bytes',lambda r:r['write_blocks']*512,statistics.median),('frames',lambda r:r['result']['frames'],statistics.median),('temporary_bytes',lambda r:r['result']['temporary_bytes'],statistics.median)]:
  values={side:agg(fn(r) for r in selected if r['side']==side) for side in ['before','after']}
  summary[field]={**values,'delta_percent':(values['after']/values['before']-1)*100 if values['before'] else None}
 summaries.append(summary);(root/'summary.json').write_text(json.dumps(summaries,indent=2));print('SUMMARY',json.dumps(summary),flush=True)
