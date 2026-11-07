# Maintenance refactor qualification

This is the before/after maintenance cohort in the [combined workload report](../workload-storage-20261104/README.md).
The baseline is `9343ea1`. The accepted implementation keeps native file methods in
focused source files under their original Rust module. The frozen API inventory
is unchanged; normalized signature/body comparison covers all 261 `VarlitFile`
methods, including trait methods. Formatting and final comment polish preserve
the machine-code `.text` bytes in all four benchmark executables: mixed, arrays,
writer and payload. All non-documentation Rust tokens remain identical across
the 49 files whose comments were polished.

## Work and controls

Both accepted builds use the same workspace path, Rust 1.95.0, release optimization,
feature selection and lockfile. CPU/memory/filesystem limits remain **4 CPU / 16 GiB /
32 GiB**. No resource ceiling or read-check policy is raised. Each configuration
has identical data, publication size, reader count, reverse item-query work and
validation. Groups contain 128-byte items packed into 64 KiB or 1 MiB blocks;
publications contain 16 MiB. Every reader follows the complete published workload
and independently queries the groups. Native arrays include scan, points, windows
and a repeated scan. Resource probes and publication gates are separate from timing.

Reference A/B are identical baseline binaries in randomized blocks. The primary
accepted cohort fixes five rounds per label. Its +5.12% completion alert at
128 MiB/eight readers and +7.88% CPU alert at 1 GiB/1 MiB blocks triggered a fixed
**30-round** randomized calibration for those two configurations. The candidate
binary did not change; the stopping count was fixed before looking at that run.
All primary alerts and calibration observations are retained, without pooling
cohorts or selecting a passing subset.

## Accepted observations

Detailed timing cells show the two baseline-label medians as a range; percentage
changes consistently use reference A. Two rows use the 30-round calibration; the remaining four use the primary five
rounds, as indicated by `n`. These are observed medians, not a confidence bound
for all workloads. The calibrated rows change completion by −0.48% / −1.46% and
CPU by +1.86% / −1.19%. Identical-reference controls change completion by −0.02% /
−1.07% and CPU by +1.89% / −0.53%, respectively.

| Workload | n per label | Completion s, before A/B → final | CPU s, before A/B → final | Writer s, before A/B → final | Reader active s, before A/B → final |
| --- | ---: | ---: | ---: | ---: | ---: |
| 128 MiB, 1 producer / 1 reader, 1 MiB blocks | 5 | 0.226–0.228 → 0.222 | 0.307–0.307 → 0.295 | 0.139–0.148 → 0.149 | 0.172–0.174 → 0.169 |
| 128 MiB, 1 producer / 8 readers, 64 KiB blocks | 30 | 0.351–0.351 → 0.350 | 1.281–1.305 → 1.305 | 0.233–0.237 → 0.230 | 0.269–0.287 → 0.286 |
| 1 GiB, 4 producers / 8 readers, 1 MiB blocks | 30 | 3.534–3.572 → 3.520 | 13.794–13.868 → 13.704 | 1.878–1.930 → 1.894 | 3.453–3.474 → 3.416 |
| 1 GiB, 4 producers / 8 readers, 64 KiB blocks | 5 | 2.705–2.808 → 2.602 | 10.734–11.073 → 10.322 | 2.573–2.577 → 2.533 | 2.583–2.614 → 2.416 |
| 128 MiB native views, 64 KiB blocks | 5 | 0.276–0.288 → 0.294 | 0.211–0.221 → 0.228 | 0.126–0.128 → 0.127 | — |
| 128 MiB immutable mmap, 64 KiB blocks | 5 | 0.201–0.204 → 0.209 | 0.135–0.138 → 0.143 | 0.125–0.126 → 0.127 | — |

At 128 MiB, the single-reader writer phase rises 7.47% against reference A,
while identical reference B rises 6.09%. The eight-reader active phase rises
6.22%, while identical reference B rises 6.64%. These phase variations remain
visible; they are not uniform performance improvements.

For native arrays, writer time is the harness's existing `write_seconds` phase.
The individual read phases below are retained independently; do not add overlapping
open/scan counters. Each point/range result is checked.

| Native route | Open s | First scan s | Repeated scan s | Point queries s | Windows s |
| --- | ---: | ---: | ---: | ---: | ---: |
| 128 MiB native views, 64 KiB blocks | 0.0048 → 0.0053 | 0.0288 → 0.0293 | 0.0298 → 0.0292 | 0.0467 → 0.0475 | 0.0141 → 0.0141 |
| 128 MiB immutable mmap, 64 KiB blocks | 0.0052 → 0.0047 | 0.0175 → 0.0176 | 0.0093 → 0.0096 | 0.0026 → 0.0028 | 0.0052 → 0.0050 |

Reader-active is the maximum reader's consume plus refresh time, including query
verification and final drain, excluding idle polling sleep. It overlaps writer
work and must not be added to completion. Raw telemetry retains writer-active
intervals and per-reader consume intervals, including work completed during writes.
All 12 publication-gate runs for the accepted candidate pass.

| Workload | Sampled PSS MiB, before A/B range → final | wait4 max RSS MiB, before A/B range → final | Final allocated MiB, before → final | Peak disposable temp MiB, before → final |
| --- | ---: | ---: | ---: | ---: |
| 128 MiB, 1 producer / 1 reader, 1 MiB blocks | 85.3–86.2 → 85.3 | 80.7–80.7 → 80.8 | 128.24 → 128.24 | 0.00 → 0.00 |
| 128 MiB, 1 producer / 8 readers, 64 KiB blocks | 93.0–93.2 → 93.3 | 76.2–76.2 → 76.2 | 130.00 → 130.00 | 0.00 → 0.00 |
| 1 GiB, 4 producers / 8 readers, 1 MiB blocks | 195.4–202.6 → 191.8 | 146.6–146.7 → 146.6 | 1026.19 → 1026.19 | 0.00 → 0.00 |
| 1 GiB, 4 producers / 8 readers, 64 KiB blocks | 167.1–169.0 → 166.3 | 141.2–141.4 → 141.2 | 1102.92 → 1101.45 | 0.00 → 0.00 |
| 128 MiB native views, 64 KiB blocks | 22.9–23.0 → 23.0 | 23.3–23.4 → 23.4 | 128.23 → 128.23 | 0.00 → 0.00 |
| 128 MiB immutable mmap, 64 KiB blocks | 141.2–141.3 → 129.3 | 141.6–141.7 → 141.7 | 128.23 → 128.23 | 0.00 → 0.00 |

Process-tree PSS is periodically sampled and can miss a short peak; wait4 RSS is
a maximum child value, not aggregate physical memory. Filesystem/cache state and
producer arrival order can vary allocation/temporary peaks and index packing.
The raw reference resource observations remain alongside candidate probes. These
finite runs do not establish long-duration logging deadlines or OS OOM freedom.

## Rejected module split

An initial implementation placed native methods in eight distinct Rust modules.
The same-path controlled campaign reproduced approximately **16–22%** completion
regressions on concurrent grouped workloads, with higher CPU. Method bodies were
unchanged, but compiler partitioning and generated code changed. That variant is
rejected. Keeping the original logical module restores the original private
boundary and removes that observed regression without runtime caches, locks,
relaxed validation or larger budgets. The first separate-build-path campaign is
retained as preliminary evidence; a second campaign adds a separate-path baseline
control and confirms the regression at the identical path.

## Evidence

Across preliminary, same-path module, accepted and calibration cohorts there are
**480 timing observations, 66 resource probes and 28 publication gates**, all with
successful data/operation oracles. Performance regressions above remain explicit
regardless of correctness success. See [comparison data](comparison.json),
[raw observations and build receipts](evidence.tar.xz) and [checksums](SHA256SUMS).
The archive includes the rejected variant's source snapshot, fixed run scripts,
binary hashes and every stdout/stderr/usage/configuration observation. The accepted
timed source is identified by per-file hashes in the comparison data and the
[`cf44f67` Git tree](https://github.com/shim9610/varlit/tree/cf44f6747ded17d348379e4608ab07904848b334).
The subsequent comment-only rebuild preserves these `.text` SHA-256 values:

```text
mixed    8b6ee7e167a01d571cf85d5d7988b2cc8e300d2a782f97d87f15d3d63f927437
arrays   bc311ce56ef606ce7648d7cf399ccde06742c9ebc1a8d4aeda816c9bf767f497
writer   8de91b3c35f872269ecbcaeb20a7fd9727cfb8abf0a0585ff11401506e68bb45
payload  f85d27062fc00ccf15af10bd7494eef13887e960ff2da7b21992cff37c17ebdf
```
