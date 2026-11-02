# Conditional decoder memory: performance qualification

Baseline: `50866e5df7bd5b49b3c3e1871400ee6fe90e9674`. The runtime change accounts for overlapping HashMap tables; budgets, APIs, file format and spill defaults are unchanged. Small maps retain the original charge. Larger maps can reject earlier at an existing materialization limit because the conservative charge includes two tables.

Runs use the unchanged four-CPU/16 GiB cgroup, the same workloads and alternating before/after order, without concurrent builds/tests/proofs. Single-process cases use the same pinned CPU. Every initial case with any metric above +5% receives 15 additional pairs; initial and confirmation data are both retained and pooled. Operation/process CPU use medians and RSS uses the maximum per side. These are observations, not a zero-cost or general-speedup claim.

| Workload | Pairs | Operation | CPU | Max RSS |
| --- | ---: | ---: | ---: | ---: |
| 2M records / 200k forward marks | 5 | -2.65% | -0.30% | +1.25% |
| Variable random marks | 5 | -3.52% | -4.85% | -1.36% |
| Post-rewrite random marks | 5 | +4.51% | -2.07% | +0.12% |
| MarkTail recovery | 5 | -1.95% | -2.69% | -3.04% |
| 4 GiB current writer | 5 | +2.50% | +2.69% | +0.00% |
| 4 GiB lazy reader | 5 | -4.38% | -1.99% | +0.00% |
| 4 GiB mixed records | 20 | +10.24% | -0.38% | +0.00% |
| Complex maintenance | 5 | -0.50% | -2.18% | +0.00% |
| Checkpoint | 5 | -3.43% | -0.73% | +0.31% |
| 64 MiB chunk encoding | 5 | -0.71% | -0.99% | -0.08% |
| 64 MiB chunk decoding | 5 | -0.32% | -0.87% | -0.03% |
| 1M-entry HashMap × 8 | 5 | +1.52% | +0.41% | ≈0.00% |
| 262,144 strings × 6 | 5 | +1.03% | +0.73% | -0.05% |
| 8,192 nested HashMaps × 6 | 5 | -2.83% | -1.50% | -0.02% |

## Single writer / concurrent readers

Eight following readers and one pinned reader execute 16,384 checks per reader per mutation epoch. Three pairs per size require matching work, snapshots and trace hashes. These exercise the indexed database path; the HashMap decoder workloads above measure the changed path separately.

| Size | Wall | CPU | Max RSS | Gate |
| --- | ---: | ---: | ---: | --- |
| 1 GiB | -1.45% | -0.15% | +0.09% | passed |
| 4 GiB | -0.42% | -0.01% | -0.03% | passed |

## Temporary storage

The 5 ms sampler records observed peaks, not formal maxima. Whole-cgroup memory includes cache and is recorded separately; process RSS does not include all mmap/tmpfs backing.

| Workload | Before temporary allocated | After temporary allocated | Before all open regular files | After all open regular files |
| --- | ---: | ---: | ---: | ---: |
| forward | 31,457,280 | 31,457,280 | 175,464,448 | 175,464,448 |
| rewrite | 4,149,248 | 4,149,248 | 93,356,032 | 93,356,032 |
| large | 0 | 0 | 4,300,742,656 | 4,300,742,656 |
| complex | 4,194,304 | 4,194,304 | 91,955,200 | 91,955,200 |
| hashmap | 0 | 0 | 0 | 0 |

All 182 primary A/B timing/resource observations plus 60 diagnostic observations and ten storage probes are retained in [the lossless archive](measurements.tar.gz), with [member hashes](measurement-index.json). [Commands and exits](jobs.json), [binary hashes and environment](host.json), and [pooled data](pooled-summary.json) describe this run. Earlier PR hardening costs are not superseded by this comparison. The decoder and orchestration scripts retain the exact commands used.

## Initial signals retained

- native case 6, operation_seconds: initial +17.54%; 15-pair confirmation +6.67%. All samples remain in the pooled table.

## Short-operation measurement limitation

The original 8,192-mark mixed-file workload has a **+10.24% pooled operation-time signal** (initial +17.54%, confirmation +6.67%). Ten pairs using the **same candidate binary on both sides** also show +10.03%. Consequently this short workload is not qualified to distinguish a 5% code-change effect on this host; the A/A control does not erase the A/B observations.

An additional diagnostic retains the same 131,072-record / approximately 4 GiB file and increases marks to 65,536 on both sides, with unchanged resource budgets. Ten A/B and ten identical-binary pairs are reported separately and are never pooled into the original workload.

| Diagnostic | Pairs | Operation | Process CPU | Max RSS |
| --- | ---: | ---: | ---: | ---: |
| 8,192 marks A/A | 10 | +10.03% | -0.90% | -0.09% |
| 65,536 marks A/B | 10 | -3.65% | -2.74% | +1.85% |
| 65,536 marks A/A | 10 | -0.44% | -1.09% | +1.19% |

The longer diagnostic gives a less noisy control and no observed regression, but does not establish a general speedup or retroactively pass the short-workload threshold. No runtime code was changed in response to timing noise.
