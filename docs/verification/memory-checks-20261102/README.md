# Memory instrumentation follow-up

Base: PR #18 at `e84d0d93af5b689057f9b1c7e7b1f332002a9c62`.

The two Kani compiler crashes are fixed by using an immutable upstream source
revision. **Complete initialization/value-validity verification of Box,
TryClone and the internal Arc is still blocked.** The additional passing
checks below have deliberately narrower scopes. The ordinary ownership
contracts and their allocator model are unchanged.

## Cause and upstream evidence

| Failure | Diagnosis and disposition |
| --- | --- |
| `expected pattern type to have a scalar ABI` | Kani assumed pattern types could not wrap fat pointers. `NonNull<[u8]>` disproves that assumption. [Issue #4829](https://github.com/model-checking/kani/issues/4829), fixed by [PR #4856](https://github.com/model-checking/kani/pull/4856). The fix checks the data pointer's non-null range; it does not disable validity checking. |
| `initial_target_visitor` unwrap on a function pointer | Delayed-UB target discovery tried to resolve every call as a direct function. [PR #5006](https://github.com/model-checking/kani/pull/5006) fixes the crash. Reachable indirect calls still fail as unsupported; they are not silently accepted. |
| Errors after those crashes were removed | The current experimental passes still reject opaque mutable pointer casts, niche-encoded enums and raw-pointer/union operations. Even the actual `try_box::<u32>` success path reaches unsupported `Box::write(MaybeUninit<_>)` instrumentation. These are separate from the fixed crashes. |

Kani's latest official release at investigation time was still 0.68.0. Both
merged fixes postdate it. CI now builds upstream commit
`947fcec757d9b6a2d07283a0952aaa088e1d8df1`, using its
`nightly-2026-10-21` compiler and the release bundle's CBMC 6.11.0. Upstream still
reports version 0.68.0 at this commit, so the report records the source commit
and compiler/driver binary hashes rather than relying on the version string.
No local Kani source patch is applied. CI retains four build jobs, the existing
30-minute job timeout and 300-second per-harness timeout.

Further investigation found a separate initialization false positive:
[issue #5056](https://github.com/model-checking/kani/issues/5056) describes
checking old destination bytes before stores/raw field-address formation.
[PR #5057](https://github.com/model-checking/kani/pull/5057) is an **open** proposed
fix, not part of this tool pin. Its own limitations explicitly retain mutable
cast, copy and uninitialized-memory restrictions. The field-insensitive Vec
initialization analysis also has known limitations described in
[PR #4898](https://github.com/model-checking/kani/pull/4898) and the
[initialization tracking issue #3300](https://github.com/model-checking/kani/issues/3300).
Consequently, changing the version alone cannot establish the requested
general memory-safety theorem.

## Passing checks and exact boundaries

`fallible/memory_proofs.rs` calls the actual production helpers:

* `initialized_scalar_box_and_clone`: arbitrary `u32` value, successful Box
  construction and boxed `TryClone`, equal values, distinct owners and real
  drops. This uses normal Kani allocation models, with successful allocation.
  It passes **value-validity** instrumentation, not initialization instrumentation.
* `refused_scalar_box_does_not_initialize`: the allocator returns null;
  `try_box_with` returns `AllocationFailed` and never invokes the callback.
  This passes **both** extra instrumentation flags. Its heap-free error is
  forgotten at the caller boundary; native/Miri tests execute real error drops.

Neither harness proves arbitrary `T`, partially successful multiple allocations,
Arc concurrency, allocator implementation correctness or absence of OS OOM kills.
The original bounded ownership proofs continue to check partial clones,
drop counts, live storage, refusal positions and transactional COW separately.

Six controls calibrate the new tool:

| Control | Required outcome |
| --- | --- |
| Allocate/read/drop a `u32` Box | Value-validity success; previously compilation crashed |
| Null data pointer in `NonNull<[u8]>` | Specific invalid-value failure |
| Initialize one byte, create/read `&u8` | Initialization success |
| Create `&u8` from uninitialized storage | Specific uninitialized-pointer failure |
| Slice indexing with unreachable indirect panic formatting | Initialization success; previously compilation crashed |
| Reachable function-pointer call | Specific unsupported failure, **not** a UB detection or proof success |

The runner requires the exact harness, a completed verification summary, a
nonzero exit and the specific failed property for each negative control.
Compiler crashes, timeouts and unrelated failures cannot satisfy these controls.
The ordinary manifest requires 52 harnesses: the previous 50 plus the two
scoped additions. Standard checking of this manifest does not by itself run
either experimental memory flag.

The new native/Miri test uses a padded, 64-byte-aligned payload containing
`bool`, `NonZeroU32`, a reference and `Vec<Box<bool>>`. It covers both boolean
values, every allocation refusal in a four-allocation COW clone, preserved
source sharing, successful detachment, and real partial-owner/error drops.
Miri retains its default validity and aliasing checks. This is dynamic testing,
not a replacement formal proof.

## Reproduction and blocked results

Install the official Kani 0.68.0 bundle first (`cargo install kani-verifier
--locked --version 0.68.0`, then `cargo kani setup`), then:

```sh
bash scripts/build_memory_kani.sh /tmp/kani-ownership
export PATH="/tmp/kani-ownership/scripts:$HOME/.kani/kani-0.68.0/bin:$PATH"
python3 scripts/check_memory_proofs.py --kani-checkout /tmp/kani-ownership \
  --output target/verification/memory-default
python3 scripts/check_memory_proofs.py --kani-checkout /tmp/kani-ownership \
  --features all --output target/verification/memory-all
```

To request the original four full production ownership harnesses under **both**
extra flags, add `--diagnose-ownership`. Their assertions and allocator model
are preserved. The command records every failed property and **returns nonzero
while those checks remain blocked**. CI's scoped scalar step does not run or
claim this full qualification. Its job summary explicitly says that complete
ownership initialization is not qualified.

An experiment replacing only the proof allocator's opaque C pointer boundary
removed the first mutable-cast blocker, but exposed further unsupported
copy/union/enum operations and initialization false positives. That change was
discarded; its patch and failed logs are retained as diagnostic evidence. No
owner implementation, assertion or input bound was weakened to obtain a green
result. A draft run was also interrupted for a test-only Clippy annotation;
interrupted/draft logs are not qualified results.

The historical [ownership report](../ownership-20261102/README.md) remains an
accurate account of the earlier tool and checks. This follow-up does not
retroactively relabel its blocked instrumentation as successful.

## Runtime compatibility and performance

This follow-up changes verification tooling and `cfg(test)`/`cfg(kani)` code.
Production helper bodies, public APIs, dependencies and file formats are
unchanged. Five rebuilt benchmark executables are byte-identical to the prior
qualified binaries; the sixth (`record_mutation_load`) has identical `.text`
and `.rodata` sections. Equal-work runtime and resource measurements are still
performed on the unchanged four-CPU/16-GiB machine; see the accompanying results.
Prior performance costs, including the earlier direct Box measurement of
**+8.77% elapsed / +9.11% CPU**, remain disclosed in the
[ownership performance report](../../performance/ownership-contracts-20261102/README.md).

## Qualified results

* Standard Kani: **52/52 default and 52/52 all features**, identical before/after source fingerprints.
* Extra flags: **2 value-validity harnesses and 1 refusal-initialization harness**, plus **6 calibration controls**, in each feature configuration.
* Full ownership diagnostic: **0/4 passed, exit 1**. The compiler completes; remaining unsupported/failed properties are retained.
* Native workspace library/integration tests: **1,352 passed, 27 ignored**; nested UI snapshots were explicitly skipped. Miri: **7 owner unit tests + 6 ownership integration tests passed**.
* Deliberate defects detected: **5 ownership + 11 publication + 17 resource**. Python checks: **47 passed**. Clippy (1.95.0, workspace/all targets/all features), formatting and actionlint passed.

There are **122 equal-work timing observations and four separate resource probes**. Single-process cases alternate before/after on the same CPU; concurrent cases retain the same affinity set under the four-CPU quota. Five pairs are followed by 15 additional pairs when a measured regression exceeds 5%; all 20 are pooled. Only the 1-GiB clone case needed that confirmation. SWMR uses three pairs at each size, eight following readers and a pinned reader.

| Workload | Pairs | Elapsed change | CPU change |
| --- | ---: | ---: | ---: |
| bytes-67108864 | 5 | +0.24% | +0.35% |
| bytes-1073741824 | 20 | +1.82% | +1.43% |
| nested-16384 | 5 | +1.67% | +2.77% |
| Single writer, 2M fixed records | 5 | +1.35% | +1.72% |
| Single writer, large reverse updates | 5 | -5.59% | -0.36% |
| Complex maintenance, 65,536 × 512 B | 5 | +3.32% | +4.89% |
| Chunk compression, 64 MiB | 5 | -1.56% | -0.15% |
| Chunk decoding, 64 MiB | 5 | +3.07% | +4.20% |
| SWMR 1 GiB, 8 followers + pinned reader | 3 | +1.28% | -0.15% |
| SWMR 4 GiB, 8 followers + pinned reader | 3 | +2.44% | +1.90% |

All configured 5% regression gates passed. These are observations on unchanged executable code, not evidence of a runtime speedup. The maximum native peak-RSS increase was 0.93%; the 1-GiB clone remained about 2,049 MiB. The complex probe retained exactly **4,194,304 bytes** of allocated temporary storage and **91,955,200 bytes** of open regular-file allocation on both sides. The large clone used no temporary files. CPU and memory limits were not raised; no additional OOM/kill events occurred during the benchmarks.

Resource probes are sampled separately from the timing suite. Their raw per-run times are retained: the large-clone probe reported 0.810 s before and 0.955 s after. That single sampled pair is not pooled into, or substituted for, the 20-pair timing comparison above.

`results.json` contains the compact results. `evidence.tar.xz` contains source snapshots, completed logs, failed/draft experiments, binary hashes and all benchmark measurements. Generated build trees and executable binaries are excluded.
