# ChunkedBytes range streaming: performance and resources

Baseline: `b837577cca7a2436a1da0074e94149c003503680`, compared with the
streaming implementation in this commit. Existing methods and file format are
preserved. This is an opt-in route after whole-output refusal, with a caller
buffer and an explicit zstd history-window policy.

The four-CPU/16 GiB cgroup limits were unchanged. Measurements ran without
concurrent builds, tests or proofs. Single-reader/native cases use the same
pinned CPU; concurrent readers use the same full affinity under that CPU quota.
Before/after order alternates. Every initial direct/native result above +5% gets
15 additional pairs; those samples are pooled with the initial five pairs.
Time and process CPU use medians; RSS uses the maximum per side. These are
measurements on this host, not general speedups or a zero-overhead claim.

## Equal requested-byte comparisons

Both sides receive identical encoded fixtures and validate the same requested
bytes, offsets and CRC. Before uses complete materialization, after streams only
selected overlapping chunks (except the unchanged-full control, which uses the
old API on both sides). The full-buffer baseline does extra decoding outside a
selected range because it has no range API; this avoided work is the intended
benefit, not a claim that every input byte is decoded on both sides. All selected
chunks are fully decoded/CRC checked, including their unrequested prefixes and
suffixes. Normal streaming scratch is 32 KiB, explicit history cap 8 MiB.

Operation time excludes input loading and reference generation; process CPU and
peak RSS include setup. Fixtures are prepared before measurement. All encoded
input stays resident; the incompressible case deliberately exposes this cost.

| Workload | Pairs | Operation | Process CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| Existing full decode: 64 MiB × 8 | 5 | +1.01% | +2.43% | 67.77 → 67.83 |
| Full streaming: 64 MiB × 8 | 5 | -21.84% | -14.83% | 67.83 → 3.58 |
| Full streaming: 1 GiB × 2 | 5 | -13.01% | -1.31% | 1041.67 → 17.47 |
| 16 MiB range in 1 GiB × 4 | 5 | -98.45% | -96.67% | 1041.73 → 17.53 |
| Same range × 2 × 8 readers | 5 | -98.62% | -98.35% | 7109.62 → 23.02 |
| 1 MiB range in one 64 MiB chunk × 8 | 5 | -16.86% | -16.30% | 66.53 → 3.36 |
| Full 64 MiB × 4, 64-byte buffer | 20 | +80.68% | +41.42% | 67.75 → 3.54 |
| 32 MiB range in incompressible 256 MiB × 4 | 5 | -91.44% | -53.58% | 514.75 → 258.52 |

**The 64-byte buffer has a measured cost:** +80.68% operation time and +41.42%
whole-process CPU across all 20 pairs, despite much lower RSS. This opt-in
memory/CPU tradeoff is not a performance-gate pass. The API accepts a one-byte
buffer (covered by regressions), but callers should choose a larger buffer when
possible to reduce native/callback/CRC call overhead. No resource ceiling was
increased and no samples were discarded to hide this result.

## Existing paths

Five pairs each retain identical native work counters, logical file sizes and
temporary lengths. They exercise the existing full codec and database methods,
not the new callback route. None requires confirmation under the +5% rule.

| Workload | Operation | Process CPU | Max RSS |
| --- | ---: | ---: | ---: |
| 2M records / 200k forward marks | +2.13% | -7.10% | -1.37% |
| 4 GiB writer / 4096 reverse marks | -10.79% | -0.91% | +0.46% |
| Complex maintenance | +0.03% | -0.63% | -0.23% |
| 64 MiB chunk encoding | +1.51% | +1.88% | -0.22% |
| 64 MiB full chunk decoding | +3.78% | +4.07% | +0.11% |

## One writer and multiple readers

Indexed SWMR uses eight following readers plus one pinned reader and 16,384
checks per reader per mutation epoch. Each size has three alternating pairs.
Comparison requires matching work, snapshots and trace hashes. These existing
database paths are separate from the eight concurrent streaming readers above.

1 GiB: **passed**; wall +2.02%, CPU +0.52%, maximum RSS +0.09%. [Exact aggregates](swmr-1-comparison.json).

4 GiB: **passed**; wall +1.47%, CPU +1.46%, maximum RSS -0.14%. [Exact aggregates](swmr-4-comparison.json).

## Temporary storage and process versus system memory

Eight separate before/after probes sample RSS, whole-cgroup memory and open or
deleted temporary-file lengths/allocated blocks every 5 ms. These are observed
peaks, not formal maxima. RSS excludes some filesystem cache and tmpfs backing.
The callback in these benchmarks retains no output; applications that stage
output to a file or retain it in memory add their own storage cost.

| Probe | Temporary allocated before → after (bytes) | All open regular-file allocation before → after (bytes) |
| --- | ---: | ---: |
| large-full-stream | 0 → 0 | 15,552,512 → 15,552,512 |
| concurrent-range | 0 → 0 | 15,552,512 → 15,552,512 |
| large-single-chunk | 0 → 0 | 0 → 860,160 |
| complex | 4,194,304 → 4,194,304 | 91,955,200 → 91,955,200 |

The single-chunk probe caught the short-lived input file descriptor only on the
after side (860,160 allocated bytes); this is the existing encoded fixture, not
a new spill file. Zero sampled open-file bytes on the before side does not mean
no input file was read. All direct streaming probes observed zero temporary
allocation; complex maintenance retained the same 4 MiB spill allocation.

Raw per-probe JSON includes cgroup/cache memory separately. The cgroup's OOM,
OOM-kill and group-kill counters stayed zero; its existing memory ceiling and
cache-reclaim events are retained in `host.json`. This is not proof that future
arbitrary workloads cannot exhaust memory.

## Reproduction and retained evidence

- `performance.py` records exact commands, equal-work assertions, repetitions,
  affinity and confirmation rules; it calls the existing audit-followup
  `bench.py` and `resource_probe.py` from this repository.
- `build_benches.sh` builds both release/all-feature sets with stable Rust 1.95.0.
  Its absolute paths record this session; map `/workspace/varlit` to the candidate
  checkout and `/workspace/varlit-stream-baseline` to the baseline worktree.
  Copy `baseline_chunk_stream_load.rs` into the baseline's core examples and
  register it with required features `compression-zstd, integrity`, as on the
  candidate. The baseline harness differs only in excluding the unavailable
  streaming arm; requested-work validation is shared.
- `host.json` records binary SHA-256 values and unchanged cgroup budgets.
  `source-sha256.json` identifies the qualified changed sources.
  `fixtures.json` retains encoded input sizes and SHA-256 values; fixture
  generators and commands reproduce them. Large fixtures/binaries are excluded
  from the archive.
- `evidence.tar.gz` retains all direct/native/SWMR samples, commands, logs and
  resource probes, losslessly, with `evidence-members.sha256` and `archive.sha256`.
  There are **110 direct + 50 native + 12 SWMR primary observations**, plus
  eight resource probes. Preparation, compilation and functional-smoke timings
  are excluded from the qualified samples.

Earlier PR hardening costs remain relevant; this comparison does not supersede
them. [Validation scope and proof assumptions](../../verification/chunk-streaming-20261102/README.md)
distinguish wrapper arithmetic proof from native-backend and global OOM claims.
