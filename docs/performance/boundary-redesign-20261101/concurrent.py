import pathlib, subprocess, shutil, sys, json, os
root=pathlib.Path('/workspace/scratch/boundary-redesign-20261101')
repo=pathlib.Path('/workspace/varlit')
sys.path.insert(0,str(repo/'scripts'))
from compare_load import compare
bins={'before':pathlib.Path('/workspace/scratch/security-fixes-20261101/bin/isolated-before-scalable'),'after':root/'after-target/release/examples/scalable_load'}
os.environ['TMPDIR']=str(root/'disk-data')
assert bins['before'].read_bytes() != bins['after'].read_bytes(), 'identical comparison binaries'
for total,epoch in [('1','16'),('4','64')]:
 reports={'before':[],'after':[]}
 for pair in range(5):
  for variant in (['before','after'] if pair%2==0 else ['after','before']):
   name=f'concurrent-final-{total}-{pair}-{variant}'
   out=root/name; data=root/(name+'-data')
   cmd=['python3',str(repo/'scripts/load_scalable.py'),'--suite','large','--only','indexed-r8-20g','--total-gib',total,'--epoch-mib',epoch,'--reader-checks-per-epoch','16384','--read-seconds','1','--data-root',str(data),'--output',str(out),'--binary',str(bins[variant])]
   print('RUN',name,flush=True)
   with (root/(name+'.log')).open('w') as log:
    subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=600)
   report=json.loads((out/'report.json').read_text()); reports[variant].append(report)
   if data.is_dir() and (data/'.varlit-load-owner').read_text()==str(out): shutil.rmtree(data)
  print('PAIR DONE',total,pair,flush=True)
 result=compare(reports['before'],reports['after'])
 (root/f'concurrent-final-{total}-comparison.json').write_text(json.dumps(result,indent=2))
 print('SUMMARY',total,json.dumps(result['metrics']),flush=True)
