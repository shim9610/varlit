#!/usr/bin/env python3
"""Run frozen, equal-work profiles. Standard library only; Linux, fresh outputs."""
import argparse
import hashlib
import gzip
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import threading
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
BASE = dict(scenario="mixed", records=256, payload=65536, variable=True,
            per_tx=16, producers=1, readers=1, lookups=False, scalar=False,
            cache=8 << 20, page=4096, mmap=0, fletcher=True, compact_bytes=512 << 20, hdf_chunk=0)


def profiles(suite):
    """Return complete configurations; never splice unindexed writes into indexed reads."""
    if suite == "mapped-arrays":
        return [dict(name=f"array-mapped-n{total}-g{group}-{mode}",kind="array",mode=mode,total=total,
                     group=group,page=4096,mmap=0,points=4096,ranges=128,readers=0,warm_scan=True)
                for total in [16<<20,256<<20,1<<30] for group in [65536,1<<20]
                for mode in ["native-mmap","sql-chunks"]]
    if suite == "hdf-mixed":
        out=[]
        for payload in [65536,1<<20]:
            for readers in [1,8]:
                for chunk in [0,payload//4]:
                    c=dict(BASE,records=(1<<30)//payload,payload=payload,
                           per_tx=(16<<20)//payload,readers=readers,hdf_chunk=chunk)
                    out.append(dict(name=f"mixed-p{payload}-r{readers}-chunk{chunk}-hdf5",kind="mixed",engine="hdf5",config=c))
        return out
    if suite == "packed":
        out=[]
        for records in [100000,1000000]:
            for producers in [1,4]:
                c=dict(BASE,scenario="table",records=records,payload=64,variable=False,
                       per_tx=100,producers=producers,readers=8,lookups=True)
                for engine in ["varlit-matrix-packed","sqlite"]:
                    out.append(dict(name=f"table-w{producers}-n{records}-{engine}",kind="mixed",engine=engine,config=c))
        return out
    if suite == "tuning":
        candidates = [p for p in profiles("pilot") if p.get("engine") == "sqlite" or p.get("mode", "").startswith("sql-")]
        extra = [dict(p, group=65536, name=p["name"].replace("g1048576", "g65536")) for p in candidates if p.get("mode") == "sql-chunks"]
        out = []
        for p in candidates + extra:
            for page in [4096, 16384, 65536]:
                for mmap in [0, 1 << 30]:
                    q = dict(p, name=p["name"] + f"-p{page}-m{mmap}")
                    if p["kind"] == "mixed":
                        q["config"] = dict(p["config"], page=page, mmap=mmap)
                    else:
                        q.update(page=page, mmap=mmap)
                    q["tuning_family"] = (f"array-{p['mode']}-g{p['group']}" if p["kind"] == "array" else p["name"])
                    out.append(q)
        return out
    small = suite == "smoke"
    full = suite == "full"
    mixed_engines = ["varlit-stream", "varlit-indexed", "sqlite", "hdf5"]
    table_engines = ["varlit-indexed", "varlit-matrix", "sqlite", "hdf5"]
    out = []
    for readers in ([1, 8] if not small else [2]):
        for payload in ([65536, 1 << 20] if full else [65536]):
            c = dict(BASE, records=(1024 if payload == 1 << 20 else 16384) if full
                     else (32 if small else 4096), payload=payload,
                     per_tx=(16 if payload == 1 << 20 else 256) if not small else 8,
                     readers=readers)
            for e in mixed_engines:
                out.append(dict(name=f"mixed-p{payload}-r{readers}-{e}", kind="mixed", engine=e, config=c))
    # Small independent business objects, not individual samples from a numeric array.
    c = dict(BASE, records=1000000 if full else (128 if small else 100000),
             payload=128, per_tx=16 if small else 1000, variable=False,
             readers=2 if small else 8)
    for e in mixed_engines:
        out.append(dict(name=f"objects-follow-{e}", kind="mixed", engine=e, config=c))
    keyed = dict(c, lookups=True)
    for e in ["varlit-indexed", "sqlite"]:
        out.append(dict(name=f"objects-keyed-{e}", kind="mixed", engine=e, config=keyed))
    for producers in ([1, 4] if not small else [4]):
        c = dict(BASE, scenario="table", records=1000000 if full else (1024 if small else 100000),
                 payload=64, variable=False, per_tx=32 if small else 100,
                 producers=producers, readers=2 if small else 8, lookups=True)
        for e in table_engines:
            out.append(dict(name=f"table-w{producers}-{e}", kind="mixed", engine=e, config=c))
    # 16 finite f64 channels per logical sample. Stored in large groups, never one
    # Varlit frame per sample. Only SQLite's typed-table candidate uses sample rows.
    modes = ["native-view", "stream", "indexed", "matrix-mmap", "matrix-owned", "sql-columns", "sql-chunks",
             "hdf-contiguous-bulk", "hdf-chunked-bulk", "hdf-chunked-fletcher-bulk"]
    for total in ([16 << 20, 256 << 20, 1 << 30] if full else [16 << 20 if small else 256 << 20]):
        for group in ([65536, 1 << 20] if full else [1 << 20]):
            for mode in modes:
                out.append(dict(name=f"array-n{total}-g{group}-{mode}", kind="array", mode=mode,
                                total=total, group=group, page=4096, mmap=0,
                                points=64 if small else 4096, ranges=4 if small else 128, readers=0))
    return out


def save(p, v):
    p.write_text(json.dumps(v, indent=2) + "\n")


def descendants(pid):
    # This kernel omits /proc/PID/task/PID/children. Build the PPid graph
    # without reading unrelated command lines or process memory.
    children={}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():continue
        try:
            text=(entry/"stat").read_text();fields=text[text.rfind(")")+2:].split()
            parent=int(fields[1]);children.setdefault(parent,[]).append(int(entry.name))
        except (OSError,ValueError,IndexError):continue
    result,stack=set(),[pid]
    while stack:
        n=stack.pop()
        if n in result:continue
        result.add(n);stack.extend(children.get(n,[]))
    return result


def sample(root, pid, process_ids=None, kind="mixed"):
    rss = pss = deleted = 0
    deleted_inodes = set()
    for n in (descendants(pid) if process_ids is None else process_ids):
        try:
            fields = dict((x.split(":", 1)[0], x.split(":", 1)[1].strip())
                          for x in Path(f"/proc/{n}/smaps_rollup").read_text().splitlines() if ":" in x)
            rss += int(fields["Rss"].split()[0]) * 1024
            pss += int(fields["Pss"].split()[0]) * 1024
        except (OSError, KeyError):
            pass
        try:
            for fd in Path(f"/proc/{n}/fd").iterdir():
                try:
                    st = fd.stat()
                    if st.st_nlink == 0 and st.st_size > 0 and (st.st_dev, st.st_ino) not in deleted_inodes:
                        deleted_inodes.add((st.st_dev, st.st_ino))
                        deleted += st.st_blocks * 512
                except OSError:
                    pass
        except OSError:
            pass
    logical = allocated = temporary = 0
    for folder in ([root] if kind=="array" else [root / "data"]):
        if not folder.exists():
            continue
        for p in folder.rglob("*"):
            try:
                st = p.stat()
                if p.is_file():
                    logical += st.st_size
                    allocated += st.st_blocks * 512
                    if folder.name == "tmp" or p.name.endswith(("-wal", "-shm")) or ".spill" in p.name or p.name.startswith((".tmp", ".varlit-")):
                        temporary += st.st_blocks * 512
            except OSError:
                pass
    cg = {}
    try:
        cg["cgroup_memory_current"] = int(Path("/sys/fs/cgroup/memory.current").read_text())
        stat = dict(x.split() for x in Path("/sys/fs/cgroup/memory.stat").read_text().splitlines())
        cg["cgroup_memory_file"] = int(stat["file"])
        cg["cgroup_memory_anon"] = int(stat["anon"])
    except OSError:
        pass
    return dict(**cg, sampled_processes=len(process_ids) if process_ids is not None else 0, rss_sum=rss, pss_sum=pss, visible_logical=logical,
                visible_allocated=allocated, temporary_allocated=temporary, deleted_open_allocated=deleted)


def compress_failed_data(work):
    """Preserve failed engine files losslessly, between timed child executions."""
    artifacts=[]
    data=work/"data" if (work/"data").exists() else work
    for path in data.rglob("*"):
        if not path.is_file() or path.stat().st_blocks * 512 < 1<<20 or path.name.endswith((".gz", ".part")):
            continue
        size=path.stat().st_size
        target=path.with_name(path.name+".evidence.gz")
        partial=target.with_name(target.name+".part")
        original=hashlib.sha256()
        try:
            with path.open("rb") as src, gzip.open(partial,"wb",compresslevel=1) as dst:
                while block:=src.read(1<<20):
                    original.update(block)
                    dst.write(block)
            check=hashlib.sha256()
            with gzip.open(partial,"rb") as src:
                while block:=src.read(1<<20):
                    check.update(block)
            assert check.digest()==original.digest(), "failure evidence compression mismatch"
            partial.rename(target)
            path.unlink()
            artifacts.append(dict(original_path=str(path), gzip_path=str(target), original_bytes=size,
                                  gzip_bytes=target.stat().st_size, original_sha256=original.hexdigest(),
                                  roundtrip_verified=True))
        except OSError as error:
            partial.unlink(missing_ok=True)
            artifacts.append(dict(original_path=str(path), compression_error=str(error), original_retained=True))
    return artifacts


def invoke(profile, output, stem, probe=False, gate=False, keep=False, timeout=1800):
    work = output / (stem + ".work")
    config = output / (stem + ".config.json")
    env = dict(os.environ)
    # The mixed binary creates its own fresh output root; its TMPDIR must exist separately.
    tmp = output / (stem + ".tmp")
    tmp.mkdir()
    env["TMPDIR"] = str(tmp)
    if profile["kind"] == "mixed":
        save(config, profile["config"])
        command = [profile.get("binary_override", str(HERE / "target/release/mixed")), "run", profile["engine"], str(work), str(config)]
        if gate:
            env.update(VARLIT_BENCH_VISIBILITY_GATE=str(work),
                       VARLIT_BENCH_GATE_READERS=str(profile["config"]["readers"]))
    else:
        work.mkdir()
        command = [profile.get("binary_override",str(HERE / "target/release/arrays")), profile["mode"], str(work),
                   *map(str, [profile[x] for x in ["total", "group", "page", "mmap", "points", "ranges", "readers"]])]
    if profile.get("warm_scan"):
        command.append("warm-scan")
    usage = output / (stem + ".usage.json")
    pidfile = output / (stem + ".pid")
    cmd = [str(output / "measure"), str(usage), str(pidfile), *command]
    row = dict(profile=profile, probe=probe, visibility_gate=gate, command=command)
    if probe:
        row["probe_baseline"] = {k:v for k,v in sample(work, os.getpid()).items() if k.startswith("cgroup_")}
    stop, peaks = threading.Event(), {}
    budget_error=[]

    def sampler(pid):
        known={};expected=(profile["config"]["readers"]+2) if profile["kind"]=="mixed" else 2
        def birth(n):
            text=Path(f"/proc/{n}/stat").read_text();return text[text.rfind(")")+2:].split()[19]
        while not stop.is_set():
            if len(known)<expected:
                for n in descendants(pid):
                    try:known.setdefault(n,birth(n))
                    except (OSError,IndexError):pass
            live=set()
            for n,started in known.items():
                try:
                    if birth(n)==started:live.add(n)
                except (OSError,IndexError):pass
            v = sample(work, pid, live, profile["kind"])
            # TMPDIR is intentionally outside the workload root, so include it explicitly.
            temp = 0
            for item in tmp.rglob("*"):
                try:
                    if item.is_file():
                        temp += item.stat().st_blocks * 512
                except OSError:
                    pass
            v["temporary_allocated"] += temp
            v["visible_allocated"] += temp
            for key, value in v.items():
                peaks[key] = max(peaks.get(key, 0), value)
            interval=0.005 if profile["kind"]=="array" and profile.get("total",0)<=16<<20 else 0.05
            stop.wait(min(interval,0.005) if len(known)<expected else interval)

    def budget_guard(proc):
        while not stop.wait(0.1):
            if shutil.disk_usage(output).free < 1 * 2**30:
                budget_error.append("fixed filesystem budget: stopped before the last 1 GiB of free space was consumed")
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                return

    start = time.monotonic()
    with (output / (stem + ".stdout")).open("w") as out, (output / (stem + ".stderr")).open("w") as err:
        p = subprocess.Popen(cmd, stdout=out, stderr=err, env=env, start_new_session=True)
        guard_thread=threading.Thread(target=budget_guard,args=(p,))
        guard_thread.start()
        sampler_thread = None
        if probe:
            sampler_thread = threading.Thread(target=sampler, args=(p.pid,))
            sampler_thread.start()
        try:
            p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            row["error"] = "timeout"
        finally:
            stop.set()
            guard_thread.join()
            if sampler_thread:
                sampler_thread.join()
        # Cleanup orphaned child readers even when an adapter/coordinator failed.
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if budget_error:
        row["error"]=budget_error[0]
    row.update(exit=p.returncode, wall_seconds=time.monotonic() - start)
    if usage.exists():
        row["usage"] = json.loads(usage.read_text())
    if p.returncode == 0:
        result = json.loads((output / (stem + ".stdout")).read_text())
        assert result["verified"]
        if profile["kind"] == "mixed":
            assert len(result["readers"]) == profile["config"]["readers"]
            assert all(r["records"] == profile["config"]["records"] and r["verified"] for r in result["readers"])
        else:
            assert result["payload_bytes"] == profile["total"] and result["logical_samples"] == profile["total"] // 128
            if profile.get("warm_scan"):
                assert result["scan_passes"]==2 and result["warm_scan_seconds"] is not None
        row["result"] = result
        if probe:
            row["sampled_peaks"] = peaks
        if not keep:
            shutil.rmtree(work)
            shutil.rmtree(tmp)
    else:
        row.setdefault("error", f"exit {p.returncode}; data retained")
        row["failure_artifacts"] = compress_failed_data(work)
    save(output / (stem + ".json"), row)
    return row


def summarize(rows, output):
    groups = {}
    for r in rows:
        if r.get("error") or r["probe"] or r["visibility_gate"]:
            continue
        groups.setdefault(r["profile"]["name"], []).append(r)
    lines = ["# Storage comparison observations", "", "Medians of successful timing observations; failures remain in results.jsonl.",
             "No statistical qualification or universal twofold speedup is inferred.", "",
             "| Profile | n | Write s | Completion s | CPU s | wait4 max RSS MiB | Final allocated MiB |",
             "|---|---:|---:|---:|---:|---:|---:|"]
    for name, rs in groups.items():
        median = lambda f: statistics.median(f(r) for r in rs)
        write = median(lambda r: (r["result"]["initialization_seconds"]+r["result"]["writer_seconds"]+r["result"]["finalize_seconds"]) if r["profile"]["kind"]=="mixed" else r["result"]["write_seconds"])
        allocated = median(lambda r: r["result"]["allocated_bytes"] / 2**20)
        lines.append(f"| {name} | {len(rs)} | {write:.6f} | {median(lambda r:r['usage']['elapsed_seconds']):.6f} | "
                     f"{median(lambda r:r['usage']['cpu']):.6f} | {median(lambda r:r['usage']['rss_kib']/1024):.2f} | {allocated:.2f} |")
    lines += ["", "wait4 max RSS is not aggregate physical memory. Use separate sampled process-tree PSS/RSS probes.",
              "Array completion includes different supported query sets for stream (no point/range API); compare its common scan/write metrics only."]
    (output / "summary.md").write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--suite", choices=["smoke", "pilot", "full", "tuning", "packed", "hdf-mixed", "mapped-arrays"], default="smoke")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--resume",action="store_true",help="Continue an interrupted frozen run with identical benchmark binaries")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--probe", action="store_true", help="Separate resource runs after timing; do not mix probe timings")
    ap.add_argument("--tuning-results", type=Path, help="Frozen complete SQLite winners from a prior tuning run")
    ap.add_argument("--filter", default="", help="Substring of frozen profile name")
    ap.add_argument("--keep-data", action="store_true")
    ap.add_argument("--timeout", type=int, default=1800)
    args = ap.parse_args()
    assert args.repeats > 0
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=args.resume)
    cpus = sorted(os.sched_getaffinity(0))[:4]
    os.sched_setaffinity(0, cpus)
    assert len(cpus) <= 4
    memory = Path("/sys/fs/cgroup/memory.max").read_text().strip()
    if memory == "max" or int(memory) > 16 * 2**30:
        raise SystemExit("Use an existing cgroup capped at 16 GiB; this tool does not raise resource budgets.")
    if not args.resume:
        subprocess.run(["cc", "-O2", str(HERE / "measure.c"), "-o", str(output / "measure")], check=True)
    selected = profiles(args.suite)
    if args.tuning_results:
        observations = [json.loads(x) for x in (args.tuning_results / "results.jsonl").read_text().splitlines()]
        assert all(not x.get("error") for x in observations), "do not select from a failed tuning grid"
        families = {}
        for x in observations:
            if x["probe"] or x["visibility_gate"]:
                continue
            families.setdefault(x["profile"]["tuning_family"], {}).setdefault(x["profile"]["name"], []).append(x)
        winners = {}
        for family, candidates in families.items():
            # Freeze one COMPLETE layout by median end-to-end completion, not a
            # synthetic mixture of its fastest write and another layout's reads.
            winner = min(candidates.values(), key=lambda rs: statistics.median(r["usage"]["elapsed_seconds"] for r in rs))
            assert len(winner) >= 3, "tuning requires at least three observations per selected candidate"
            winners[family] = winner[0]["profile"]
        for p in selected:
            if p.get("engine") == "sqlite":
                family = p["name"]
                if p["config"]["scenario"] == "mixed" and p["name"].startswith("mixed-"):
                    family = p["name"].replace("p1048576", "p65536")
                if args.suite=="packed":
                    family=f"table-w{p['config']['producers']}-sqlite"
                winner = winners[family]
                p["config"] = dict(p["config"], **{k:winner["config"][k] for k in ["page", "mmap"]})
            elif p.get("mode", "").startswith("sql-"):
                family = f"array-{p['mode']}-g{p['group']}"
                if family not in winners:
                    family = f"array-{p['mode']}-g1048576"
                winner = winners[family]
                p.update(page=winner["page"], mmap=winner["mmap"])
        save(output / "frozen-sqlite-winners.json", winners)
    resume_id=str(time.time_ns())
    source = output / ("source-resume-"+resume_id if args.resume else "source")
    source.mkdir()
    for src in [HERE / "Cargo.toml", HERE / "Cargo.lock", HERE / "build.rs", HERE / "run.py", HERE / "measure.c", HERE / "README.md", *HERE.glob("src/**/*.rs")]:
        dst = source / src.relative_to(HERE)
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src,dst)
    chosen = [p for p in selected if args.filter in p["name"]]
    assert chosen, "no matching profile"
    manifest = dict(suite=args.suite, profiles=chosen, repetitions=args.repeats, affinity=cpus,
                    memory_max=memory, cpu_max=Path("/sys/fs/cgroup/cpu.max").read_text().strip(),
                    disk_budget_gib=32, tuning_source=str(args.tuning_results) if args.tuning_results else None, started_utc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    runtime_head=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
                    worktree_diff=subprocess.check_output(["git", "diff", "--stat"], cwd=REPO, text=True),
                    sha256={str(p.relative_to(HERE)): hashlib.sha256(p.read_bytes()).hexdigest()
                            for p in [HERE / "target/release/mixed", HERE / "target/release/arrays", HERE / "Cargo.lock", HERE / "Cargo.toml", HERE / "build.rs", *HERE.glob("src/**/*.rs"), HERE / "run.py", HERE / "measure.c"]},
                    criterion="For each equivalent successful operation separately: SQLite elapsed / Varlit elapsed >= 2. Never average operations.",
                    cache_policy="Fresh files; warm OS page cache after writes; no privileged cache eviction.",
                    durability="SQLite WAL FULL; Varlit sync/checkpoint; HDF5 flush+fsync and application publication prefix. These do not imply equal crash recovery.")
    if args.resume:
        previous=json.loads((output/"manifest.json").read_text())
        assert previous["profiles"]==chosen and previous["repetitions"]==args.repeats
        for name in ["target/release/mixed","target/release/arrays","Cargo.lock"]:
            assert previous["sha256"][name]==manifest["sha256"][name], "benchmark binaries/lockfile changed; cannot resume"
        save(output/("resume-manifest-"+resume_id+".json"),manifest)
    else:
        save(output / "manifest.json", manifest)
    before = Path("/sys/fs/cgroup/memory.events").read_text()
    if not args.resume:
        (output / "memory-before.txt").write_text(before)
    rows = [json.loads(x) for x in (output/"results.jsonl").read_text().splitlines()] if args.resume else []
    if args.resume:
        artifacts=[]
        for row in rows:
            if row.get("error"):
                # Preserve original observations; record only housekeeping in a supplement.
                work=Path(row["command"][3] if row["profile"]["kind"]=="mixed" else row["command"][2])
                artifacts.extend(compress_failed_data(work))
        save(output/("resume-failure-artifacts-"+resume_id+".json"),artifacts)
    phases = [("timing", r) for r in range(args.repeats)]
    if args.probe:
        phases.append(("probe", 0))
    # Smoke gates are conformance tests with forced synchronization, not timing results.
    if args.suite == "smoke":
        phases.insert(0, ("visibility", 0))
    for phase, rep in phases:
        order = chosen[rep % len(chosen):] + chosen[:rep % len(chosen)]
        if rep % 2:
            order = list(reversed(order))
        for profile in order:
            if phase == "visibility" and (profile["kind"] != "mixed" or profile["config"]["producers"] != 1):
                continue
            if shutil.disk_usage(output).free < 3 * 2**30:
                raise SystemExit("Less than 3 GiB free; preserve evidence and stop without increasing storage budget")
            stem = f"{phase}-{rep}-{profile['name']}"
            if args.resume and (output/(stem+".json")).exists():
                continue
            if args.resume and any(output.glob(stem+".*")):
                raise SystemExit("Incomplete attempt retained without overwriting its logs; continue that profile in a fresh output directory")
            row = invoke(profile, output, stem, phase == "probe", phase == "visibility", args.keep_data, args.timeout)
            rows.append(row)
            with (output / "results.jsonl").open("a") as f:
                f.write(json.dumps(row) + "\n")
            print(stem, row.get("error", "OK"), f"{row['wall_seconds']:.3f}s", flush=True)
            summarize(rows, output)
    (output / "memory-after.txt").write_text(Path("/sys/fs/cgroup/memory.events").read_text())
    if any("error" in r for r in rows):
        raise SystemExit("One or more profiles failed; retained attempts are not removed from comparison")


if __name__ == "__main__":
    main()
