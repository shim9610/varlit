import os,pathlib,json,subprocess,statistics,hashlib
root=pathlib.Path(__file__).parent
old=pathlib.Path('/workspace/scratch/security-fixes-20261101')
bins={'original':old/'bin/isolated-before-mark','regressed':old/'bin/v7-after-mark','after':root/'after-target/release/examples/record_mark_load'}
os.sched_setaffinity(0,{min(os.sched_getaffinity(0))})
rows=[]
for pair in range(5):
 order=list(bins);order=order[pair%3:]+order[:pair%3]
 for side in order:
  exe=bins[side]
  p=subprocess.run([str(old/'measure-child'),str(root/'exact-usage.json'),str(exe),'lazy','200000','1'],capture_output=True,text=True,check=True,timeout=300,env=dict(os.environ,TMPDIR='/tmp'))
  result=json.loads(p.stdout)
  row=dict(pair=pair,side=side,result=result,**json.loads((root/'exact-usage.json').read_text()),binary_sha256=hashlib.sha256(exe.read_bytes()).hexdigest())
  rows.append(row)
  (root/'exact-samples.json').write_text(json.dumps(rows,indent=2))
  print(json.dumps(row),flush=True)
summary={side:{key:statistics.median(r['result'][key] for r in rows if r['side']==side) for key in ['seconds']} for side in bins}
(root/'exact-summary.json').write_text(json.dumps(summary,indent=2))
print('SUMMARY',json.dumps(summary),flush=True)
