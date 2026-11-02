import argparse, json, os, stat, subprocess, time
from pathlib import Path
p=argparse.ArgumentParser()
p.add_argument('--output',type=Path,required=True)
p.add_argument('command',nargs=argparse.REMAINDER)
a=p.parse_args()
proc=subprocess.Popen(a.command,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
peak={k:0 for k in ['rss_bytes','anonymous_scratch_logical_bytes','anonymous_scratch_allocated_bytes','deleted_regular_logical_bytes','open_regular_allocated_bytes','cgroup_current_bytes']}
samples=0
while proc.poll() is None:
    vals=dict.fromkeys(peak,0)
    seen=set()
    try:
        for fd in Path(f'/proc/{proc.pid}/fd').iterdir():
            try:
                link=os.readlink(fd); meta=fd.stat(); key=(meta.st_dev,meta.st_ino)
                if not stat.S_ISREG(meta.st_mode) or key in seen: continue
                seen.add(key)
                vals['open_regular_allocated_bytes']+=meta.st_blocks*512
                if link.endswith(' (deleted)'):
                    vals['deleted_regular_logical_bytes']+=meta.st_size
                    if '/#' in link or '/.tmp' in link:
                        vals['anonymous_scratch_logical_bytes']+=meta.st_size
                        vals['anonymous_scratch_allocated_bytes']+=meta.st_blocks*512
            except OSError: pass
        for line in Path(f'/proc/{proc.pid}/status').read_text().splitlines():
            if line.startswith('VmHWM:'): vals['rss_bytes']=int(line.split()[1])*1024
        vals['cgroup_current_bytes']=int(Path('/sys/fs/cgroup/memory.current').read_text())
    except OSError: pass
    peak={k:max(peak[k],vals[k]) for k in peak};samples+=1
    time.sleep(.005)
out,err=proc.communicate()
a.output.write_text(json.dumps({'command':a.command,'exit':proc.returncode,'sample_interval_seconds':.005,'samples':samples,'peaks':peak,'stdout':out,'stderr':err},indent=2)+'\n')
raise SystemExit(proc.returncode)
