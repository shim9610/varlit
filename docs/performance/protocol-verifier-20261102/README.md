# Publication protocol and writer-resume performance

Baseline: `795c1ab22ad664f568da9aa350aa6019250d9934`. This comparison isolates the
shared publication drivers and conditional writer-resume synchronization. Earlier
hardening costs in PR #18 remain applicable; these measurements do not replace
that earlier baseline.

Both builds use Rust 1.95.0, release mode, all features, and separate target
folders. The identical `scalable_load` harness (including the new `writer-open`
phase) is compiled in both trees; the baseline's only source change is that
benchmark harness. Configured cache, batch and record budgets are unchanged.
Measurements run without concurrent builds, tests or proof solving.

## Repeated writer open

`writer_open.py` creates actual 1 and 4 GiB committed workloads, then alternates
five before/after pairs with 1,000 opens per sample. These are warm repeated opens
in one process. Each size also exercises a divergent head by invalidating one
confirmed copy while retaining the complete other copy. Fixture writes and sync
are outside timing. The divergent measurement captures the extra synchronization
call on an already clean cache, not dirty-device or power-loss recovery latency.

| File workload | Heads | Before µs/open | After µs/open | Change |
| --- | --- | ---: | ---: | ---: |
| 1 GiB | Matching | 66.932 | 63.625 | −4.94% |
| 4 GiB | Matching | 57.299 | 56.329 | −1.69% |
| 1 GiB | Divergent | 62.507 | 161.269 | +158.00% |
| 4 GiB | Divergent | 52.071 | 155.626 | +198.87% |

Normal-open slowdown was not observed in these samples; the decreases do not
establish a general speedup. The divergent path costs about 99–104 µs more per
open. It supplies durability that the old path omitted. The fix does not rewrite
the heads, so repeated opens remain on this conditional sync path until a later
publication makes both copies match.

Separate `strace` runs confirm exactly two additional positional reads per open.
Matching-head opens issue zero `fsync`/`fdatasync` calls in either build.
Divergent-head candidate opens issue one `fdatasync` each; the baseline issues
none. Primary and sidecar lengths remain unchanged across repeated opens:

| Workload | Primary bytes | Sidecar bytes |
| --- | ---: | ---: |
| 1 GiB | 1,075,986,001 | 61,223,200 |
| 4 GiB | 4,303,917,397 | 89,399,584 |

All 40 timing observations and eight syscall summaries are retained in
`writer-open.json`. These tests measure operation elapsed time and actual syscalls;
filesystem and hardware durability still depend on the platform's sync contract.

## Equal-work concurrent readers

Eight following readers and one pinned reader perform exactly 16,384 checks per
mutation epoch. Counts, snapshots and trace hashes must match. Each initial size
has three alternating pairs. The unchanged gate allows at most 5% increase in
write-phase wall time, CPU time, maximum observed resident memory and write bytes.

| Comparison | Wall | CPU | Maximum RSS | Gate |
| --- | ---: | ---: | ---: | --- |
| 1 GiB, initial 3 pairs | −3.28% | −3.48% | −0.08% | passed |
| 4 GiB, initial 3 pairs | +4.48% | +4.49% | +0.02% | passed |
| 4 GiB, additional 5 pairs | −2.29% | −1.89% | +0.15% | passed |
| 4 GiB, all 8 pairs pooled | +0.46% | +0.24% | +0.10% | passed |

The initial 4 GiB result was close to the gate, so it triggered five additional
pairs. Its increase did not repeat in that confirmation. All 22 complete reports
are retained under `runs/`; initial, confirmation and pooled comparisons remain
separate. Maximum RSS is a maximum across observations, not a median. These results
qualify the tested workloads and do not prove exactly zero overhead.

## Native mutation, rewrite, recovery and codecs

Eleven workloads include two million records, 20,000 random mutations among
200,000 records, roughly 4 GiB large-record files, lazy boundary discovery, rewrite, recovery,
checkpointing and chunk codecs. Each initially has three alternating pairs.
Every returned work counter and resource geometry must match.

Two initial signals triggered 15 further pairs each: variable random marks had
+7.61% operation time, while post-rewrite random marks had +7.18% whole-process CPU.
The confirmation and all-observation results are:

| Case | Confirmation operation | Confirmation CPU | Pooled operation | Pooled CPU |
| --- | ---: | ---: | ---: | ---: |
| Variable random marks | +4.86% | +2.30% | +3.97% | +2.99% |
| Post-rewrite random marks | +2.34% | −0.26% | +2.98% | −1.02% |

Each pooled case has 18 pairs. All 126 native observations, including the initial
signals, are retained in `native-samples.json`. The other nine cases retain their
original three pairs. Final pooled increases stay below the existing 5% comparison
threshold for operation time, process CPU, peak resident memory and write bytes.
There are small measured costs, especially random marking; these results are not
a claim that every operation became faster or has literally no added cost.

## Reproduction

Use the same compiler/features for both `release/examples` directories, applying
only `baseline-harness.patch` to the baseline. Then run:

```sh
python3 docs/performance/protocol-verifier-20261102/bench.py --before BEFORE --after AFTER --repo . --output INITIAL --phase concurrent --pairs 3
python3 docs/performance/protocol-verifier-20261102/bench.py --before BEFORE --after AFTER --repo . --output CONFIRM --phase concurrent --sizes 4 --pairs 5
python3 docs/performance/protocol-verifier-20261102/bench.py --before BEFORE --after AFTER --repo . --output NATIVE --phase native --pairs 3
python3 docs/performance/protocol-verifier-20261102/bench.py --before BEFORE --after AFTER --repo . --output NATIVE_CONFIRM --phase native --native-cases 1 2 --pairs 15
python3 docs/performance/protocol-verifier-20261102/writer_open.py --before BEFORE/scalable_load --after AFTER/scalable_load --output OPEN
```
