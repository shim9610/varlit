#!/usr/bin/env python3
"""Run every scenario x engine in fresh directories at one data size; report
medians of the key metrics, one table per scenario.

usage: run.py [--data-gb GB] [--repeats N] [--out DIR] [--only SCENARIO,...]
              [--formats a,b] [--gate]

DIR must be on a real disk (fsync costs matter); default target/runs. Every
scenario is sized to --data-gb (default 10 GB of logical data). Scenarios in
MEMCAP run under a memory cap: on Linux the process and its page cache
(systemd-run when there is a user systemd, else a cgroup v1 memory group as
root); on Windows the process's committed memory (a Job Object); on macOS none,
since nothing there enforces one for an unprivileged process. Every result
records the cap it ran under (`memcap`), and the scenario evicts its files
from the page cache before it reads on every platform.
"""
import argparse, json, os, shutil, statistics, subprocess, sys, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = HERE / "target/release" / ("varlit-format-bench" + (".exe" if os.name == "nt" else ""))
LINUX = sys.platform.startswith("linux")

# (label, scenario, extra environment, engines). Each engine runs with its
# layout for that data: the first of its candidates (see README).
PLAN = [
    ("grid", "grid", {}, ["varlit", "hdf5", "sqlite", "parquet"]),
    ("oltp", "oltp", {}, ["varlit", "sqlite", "hdf5", "parquet"]),
    ("oltp-sparse", "oltp-sparse", {}, ["varlit", "sqlite", "hdf5"]),
    ("analytics", "analytics", {}, ["varlit", "parquet", "sqlite", "hdf5"]),
    # Throughput: producers flat out, four followers of every type.
    ("mixed", "mixed", {"FB_ALL_READERS": "4"}, ["varlit", "sqlite", "hdf5", "parquet"]),
    # Freshness: real-time emission, 10 ms freshness, everything durable.
    ("mixed-live", "mixed", {"FB_ALL_READERS": "4", "FB_PACE": "1", "FB_FRESH_MS": "10",
                             "FB_LOSS_MS": "0"}, ["varlit", "sqlite", "hdf5", "parquet"]),
    ("cells", "cells", {}, ["varlit", "sqlite", "hdf5"]),
]

# Scenarios that must not fit in memory (page cache counts toward the cap).
MEMCAP = {"grid": "4G"}
CGROUP = Path("/sys/fs/cgroup/memory/fb-bench")


UNITS = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}


def capped(cmd, limit):
    """`cmd` run under a memory cap of `limit` (e.g. "4G"), and the cap it got.

    Linux caps the process and its page cache (systemd scope or cgroup).
    Windows caps the process's committed memory with a Job Object (see
    `run`); the page cache is the system's there, and the scenario evicts its
    files before it reads. macOS gives an unprivileged process no enforceable
    memory limit (no cgroups, and `RLIMIT_RSS`/`RLIMIT_AS` are not enforced),
    so it runs uncapped and relies on the same eviction."""
    if sys.platform == "win32":
        return cmd, f"{limit} job"
    if not LINUX:
        return cmd, "none"
    if shutil.which("systemd-run") and subprocess.run(
            ["systemd-run", "--user", "--scope", "-q", "true"], capture_output=True).returncode == 0:
        return ["systemd-run", "--user", "--scope", "-q", "-p", f"MemoryMax={limit}",
                "-p", "MemorySwapMax=0"] + cmd, limit
    CGROUP.mkdir(exist_ok=True)
    (CGROUP / "memory.limit_in_bytes").write_text(str(int(limit[:-1]) * UNITS[limit[-1]]))
    return ["sh", "-c", f'echo $$ > {CGROUP}/cgroup.procs && exec "$@"', "sh"] + cmd, limit


def run(cmd, env, cap):
    """Runs `cmd` like `subprocess.run(capture_output=True, text=True)`; with a
    Windows job cap (`cap` ending in " job") the child is put in a Job Object
    whose per-process committed-memory limit is the cap."""
    if not (cap and cap.endswith(" job")):
        return subprocess.run(cmd, capture_output=True, text=True, env=env)
    import ctypes
    from ctypes import wintypes
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)

    class BASIC(ctypes.Structure):
        _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                    ("LimitFlags", wintypes.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", wintypes.DWORD),
                    ("Affinity", ctypes.c_size_t), ("PriorityClass", wintypes.DWORD),
                    ("SchedulingClass", wintypes.DWORD)]

    class IO(ctypes.Structure):
        _fields_ = [(n, ctypes.c_uint64) for n in ("ReadOperationCount", "WriteOperationCount",
                    "OtherOperationCount", "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

    class EXTENDED(ctypes.Structure):
        _fields_ = [("BasicLimitInformation", BASIC), ("IoInfo", IO),
                    ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                    ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]

    JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x100
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000
    ExtendedLimitInformation = 9
    limit = cap.split()[0]
    info = EXTENDED()
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    info.ProcessMemoryLimit = int(limit[:-1]) * UNITS[limit[-1]]
    k32.CreateJobObjectW.restype = wintypes.HANDLE
    job = k32.CreateJobObjectW(None, None)
    if not job or not k32.SetInformationJobObject(wintypes.HANDLE(job), ExtendedLimitInformation,
                                                  ctypes.byref(info), ctypes.sizeof(info)):
        raise OSError(ctypes.get_last_error(), "job object for the memory cap")
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
    try:
        if not k32.AssignProcessToJobObject(wintypes.HANDLE(job), wintypes.HANDLE(p._handle)):
            p.kill()
            raise OSError(ctypes.get_last_error(), "assign the benchmark to its job object")
        out, err = p.communicate()
    finally:
        k32.CloseHandle(wintypes.HANDLE(job))
    return subprocess.CompletedProcess(cmd, p.returncode, out, err)


def control(out):
    """Disk-health probe: 1000 x (4 KiB write + fdatasync); healthy ~0.48 s."""
    d = out / "control"
    p = subprocess.run([str(BIN), "probe", str(d)], capture_output=True, text=True, check=True)
    shutil.rmtree(d, ignore_errors=True)
    return float(p.stdout.strip())


def wait_healthy(out, limit=0.7):
    while True:
        t = control(out)
        print(f"control txn_s={t:.2f}", flush=True)
        if t < limit:
            return
        time.sleep(60)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data-gb", type=float, default=10.0)
    ap.add_argument("--repeats", type=int, default=1)
    ap.add_argument("--out", default=str(HERE / "target/runs"))
    ap.add_argument("--only", help="comma list of plan labels")
    ap.add_argument("--formats", help="comma list; run only these engines")
    ap.add_argument("--device-flush", choices=["full", "os"], default="full",
                    help="durable syncs flush the drive cache (full) or stop at the OS sync (os); "
                         "differs only on macOS")
    ap.add_argument("--gate", action="store_true",
                    help="wait for a healthy disk before each repeat; redo a repeat if the disk degraded during it")
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    results_path = out / "results.jsonl"
    rep = 0
    warned = {}
    while rep < a.repeats:
        if a.gate:
            wait_healthy(out)
        batch = []
        for label, scen, extra, engines in PLAN:
            if a.only and label not in a.only.split(","):
                continue
            for fmt in engines:
                if a.formats and fmt not in a.formats.split(","):
                    continue
                d = out / f"{label}-{fmt}-{rep}"
                shutil.rmtree(d, ignore_errors=True)
                env = dict(os.environ, FB_DATA_GB=str(a.data_gb), FB_DEVICE_FLUSH=a.device_flush, **extra)
                cmd = [str(BIN), "bench", scen, fmt, str(d)]
                cap = None
                if scen in MEMCAP:
                    cmd, cap = capped(cmd, MEMCAP[scen])
                    if cap == "none" and not warned.get(scen):
                        warned[scen] = True
                        print(f"note: {scen} runs without a memory cap on {sys.platform} (none is enforceable "
                              "there); its files are evicted from the page cache before it reads",
                              file=sys.stderr, flush=True)
                t = time.time()
                p = run(cmd, env, cap)
                shutil.rmtree(d, ignore_errors=True)
                if p.returncode != 0:
                    print(f"FAIL {label} {fmt} rep{rep}\n{p.stderr[-2000:]}", file=sys.stderr, flush=True)
                    r = {"label": label, "scenario": scen, "format": fmt, "rep": rep, "failed": True,
                         "error": p.stderr.strip().splitlines()[-1:] or ["?"]}
                else:
                    r = json.loads(p.stdout.strip().splitlines()[-1])
                    r.update(label=label, rep=rep, data_gb=a.data_gb)
                if cap is not None:
                    r["memcap"] = cap
                batch.append(r)
                with results_path.open("a") as f:
                    f.write(json.dumps(r) + "\n")
                print(f"rep{rep} {label:12} {fmt:8} {time.time() - t:7.1f}s", flush=True)
        if a.gate and control(out) >= 0.7:
            print(f"disk degraded during rep{rep}; redoing", flush=True)
            continue
        rep += 1
    rows = [json.loads(l) for l in results_path.read_text().splitlines()]
    med = {}
    for r in rows:
        if r.get("failed"):
            med.setdefault((r["label"], r["format"]), {})["failed"] = r.get("error")
            continue
        m = med.setdefault((r["label"], r["format"]), {})
        for k, v in r.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool) and k != "rep":
                m.setdefault(k, []).append(v)
    summary = {f"{l}/{f}": {k: (statistics.median(v) if isinstance(v, list) else v) for k, v in ms.items()}
               for (l, f), ms in med.items()}
    (out / "summary.json").write_text(json.dumps(summary, indent=1))
    print(f"wrote {out / 'summary.json'}; tables: python3 report.py {out}")


if __name__ == "__main__":
    main()
