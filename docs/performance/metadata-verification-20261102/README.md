# Verification follow-up performance

Baseline: `b2ab2c9ea776ab991981c8a3ddd1104f587a4594`. Candidate changes are the shared framing/spill/traversal helpers and chunk table validation order described in [the verification report](../../verification/hostile-metadata-20261102/README.md).

Four-CPU quota and 16 GiB cgroup memory limit were unchanged. Builds and verification finished before measurement. Single-process pairs use the same pinned CPU; concurrent tests retain the existing CPU allowance. Before/after order alternates. Native work counters and concurrent reader work, snapshots and trace hashes must match.

A final rebuild changed the `record_mutation_load` hash; the other three measured binaries matched exactly. The affected native cases 0–6, optional-policy cases 0/2, and their storage probes were therefore measured again with a frozen final binary. Earlier affected measurements remain in the archive as superseded evidence and are not pooled into the table below. The cause of the binary difference was not established. [The identity check](measurements/final-binary-identity.json) and [final hashes](measurements/final-binary-hashes.json) make the distinction explicit.

Every initial case with any metric above +5% received 15 additional alternating pairs. On the final binary, variable/random process CPU initially increased +8.13%, and mixed-large operation time increased +7.06%; the confirmation subsets measured +0.02% and -3.18%, respectively. Unchanged standalone binaries retain the original initial signals (+5.21%, +5.45%, +9.33% operation time) and their confirmations. All samples remain available, including superseded initial/confirmation runs. The table pools only measurements of the qualified binary for each workload.

## Pooled measurements

Operation and whole-process CPU use medians; resident set size (RSS) uses the maximum observed per side. These are measured differences, not a claim of zero cost or general speedup.

| Native workload | Pairs | Operation | Process CPU | Max RSS |
| --- | ---: | ---: | ---: | ---: |
| 2M records / 200k forward marks | 5 | -5.83% | -2.71% | +1.11% |
| Variable records / random marks | 20 | +0.77% | +1.25% | +2.94% |
| Post-rewrite variable random marks | 5 | -0.74% | -2.11% | +0.92% |
| MarkTail recovery | 5 | -6.32% | -3.30% | +1.66% |
| 4 GiB large records / current writer | 5 | -17.21% | -1.65% | +1.27% |
| 4 GiB large records / lazy reader | 5 | +0.17% | +2.52% | +1.86% |
| 4 GiB mixed records / random marks | 20 | +1.19% | -0.12% | +1.22% |
| Complex maintenance | 5 | +3.01% | -0.82% | -1.38% |
| Checkpoint maintenance | 5 | -2.17% | +3.25% | +0.69% |
| 64 MiB chunk encoding | 5 | +3.43% | +2.66% | +0.11% |
| 64 MiB chunk decoding | 5 | +1.22% | +2.33% | -0.02% |

| Standalone zstd workload | Pairs | Operation | Process CPU | Max RSS |
| --- | ---: | ---: | ---: | ---: |
| Known 1 MiB × 1000 | 20 | +2.88% | +3.45% | -0.10% |
| Known 64 MiB × 20 | 20 | +0.68% | +0.45% | +0.00% |
| Unknown-size 64 MiB × 20 | 5 | -1.67% | -0.96% | +0.00% |
| Concatenated 64 MiB × 20 | 20 | +2.54% | +2.09% | ≈0.00% |

The optional spill policy uses the same explicit directory and 32 MiB per-file cap on both sides. Pooled operation/CPU changes are 2M records / 200k forward marks -11.56% / -1.29% (3 pairs); Post-rewrite variable random marks -4.55% / -4.15% (3 pairs); Complex maintenance -4.91% / -1.89% (3 pairs).

## Single writer and concurrent readers

Eight following readers and one pinned reader execute 16,384 checks per reader per mutation epoch. Three alternating pairs per size retain identical work/snapshots/traces. Both sizes pass the existing 5% wall/CPU/RSS/write gate.

| Size | Wall | CPU | Max observed RSS | Write bytes |
| --- | ---: | ---: | ---: | ---: |
| 1 GiB | -7.65% | -6.42% | +0.47% | ≈0.00% |
| 4 GiB | -2.82% | +0.52% | +0.01% | ≈0.00% |

## Memory and temporary storage

Sampled temporary logical/allocated peaks and total open-file allocated peaks match exactly between before and after. The observer samples every 5 ms; these are observed peaks, not formal maxima. RSS does not include all page cache or temporary-file backing.

| Workload | Temporary logical bytes | Temporary allocated bytes | All open regular-file allocated bytes |
| --- | ---: | ---: | ---: |
| forward | 31,457,280 | 31,457,280 | 175,464,448 |
| rewrite | 4,144,723 | 4,149,248 | 93,356,032 |
| large | 0 | 0 | 4,300,742,656 |
| complex | 4,194,190 | 4,194,304 | 91,955,200 |

Whole-cgroup memory, including cache and other session allocations, is recorded separately in the resource probes. No OOM or OOM-kill event occurred during these measurements. No resource budget was raised. Normal large-file acceptance and the default unlimited/platform-temporary spill policy remain intact.

## Evidence and reproduction

All 502 timing/resource observations are retained; 330 belong to the final qualified binary set. The remaining 172 observations used the superseded record binary. Fourteen separate storage probes are retained, including the six repeated for the final record binary. `measurements/` exposes compact initial, confirmation and pooled summaries, binary hashes, commands and resource measurements. [The lossless measurement archive](measurements.tar.gz) retains every raw sample/report/log, including pooled sample records; [the member index](measurement-index.json) records byte hashes. [Qualified summaries](measurements/qualified-summary.json) and [confirmation selection](measurements/confirmation-jobs.json) expose the aggregation and why each case was repeated.

Build both revisions with the same Rust toolchain, locked dependencies and the same release examples (`record_mutation_load`, `oom_maintenance_load`, `scalable_load` under varlit/all-features; `decompression_load` under varlit-core/compression-zstd). Run `run.py --before <baseline release/examples> --after <candidate release/examples> --output <fresh directory>` without concurrent builds/checks. It reuses the existing audit-follow-up measurement drivers. `confirm.py`, `pool.py`, `final_native.py`, `final_resources.py` and `final_pool.py` retain the exact local confirmation/aggregation commands and paths used for this qualification.

These comparisons are against the audit follow-up head. They do not replace or erase the PR's previously disclosed pre-hardening costs.
