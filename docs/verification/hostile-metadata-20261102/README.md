# Hostile metadata: executable contracts and limits

This follow-up starts at PR #18 head `b2ab2c9ea776ab991981c8a3ddd1104f587a4594` after reviewing its preceding changes, including the unlimited-spill fast path and the rejected progressive decompression allocation experiments. The baseline's CI and Sanitizers runs completed successfully (41389268520 and 41389268477).

## What is checked

The proof manifest is [proof-contracts.json](../proof-contracts.json). CI runs the complete declared set under both default and all features. Missing, duplicate, unexpected or unfinished harnesses fail the gate. A completed run with zero failed properties is required; an unwinding failure, unsupported operation, timeout or compilation error is not a successful proof or mutation detection. The default feature job also runs publication and resource negative controls.

| Contract | Formal scope | Native bridge |
| --- | --- | --- |
| Zstd framing | Actual shared block step over full-width offsets, lengths and headers; strict progress, complete-block extent and regenerated-byte bound. Reservation must fit both the caller limit and complete blocks. | 2,576 mutated/truncated/concatenated/skippable/empty-block frames; allocator refusal; known/unknown size and upstream legacy compatibility. |
| Chunk container | Actual header and entry parsers; full-width table extent bounds the number of iterations by actual input size. | 80 extreme length/count combinations; whole-container preallocation refusal; repeated and concurrent decoding. |
| Spill policy | Actual limited complete-write loop with full-u64 position/cap, up to four bytes, arbitrary short/zero writes, one Interrupted and a failure position; retry uses actual written progress. Positive successful writes are required. | Existing real partial-kernel-write, optional path/cap, scratch-index rollback and retry contracts. Unlimited File::write_all remains direct. |
| Index traversal | Actual shared visit budget decreases on every successful debit. Accepted child heights decrease. | 795 CRC-valid hostile images, including 30 that reach the child height check; existing repeated-subtree and tiny-cache tests. |
| Materialization | Actual full-u64 budget consumption preserves the ceiling and leaves rejected charges unchanged. | Existing nested-decode allocation/RSS test plus eight simultaneously live decoder outputs. |
| Native boundary prefix | Full-width initialization and range/observation transition invariant; truncation through four rows with full-width offsets/strides/row indices. Existing four-row u16 membership proof retained. | Amortized boundary discovery, invalid offsets, truncation and post-rewrite membership regressions. |
| Publication | Existing shared production protocol drivers and abstract frontier induction; positive metadata witnesses now cover StateOnly and nonzero DiskPlan digests, including nonempty primary witnesses. | Three CRC-valid hostile metadata fields, eight real following readers and one pinned reader; failed follow preserves EOF, resident state, old key reads and cursors; 24 retries adopt the restored valid publication. Existing crash/history contracts retained. |

All metadata is potentially hostile. A valid CRC is not authentication. Positive-witness preconditions describe valid metadata families; rejection proofs do not assume valid file fields. Prefix row indices are internal validated directory indices, strictly below `usize::MAX`; the transition proof assumes the documented reachable-state invariant. `usize` proofs run at the Linux x86-64 machine width.

## Termination and memory interpretation

For a fixed finite snapshot, successful modern zstd block steps advance by at least three input bytes; skippable frames consume at least their eight-byte header, and legacy frames now explicitly reject a zero consumed length. Chunk table iteration is bounded by the validated actual input extent, independently of a forged output count. Index traversal consumes fuel derived from checkpoint extent; node parsing also restricts height to 31 and backward child extents. These are compositional progress arguments supported by proofs and production binding guards, not a proof of every call in an arbitrary-length file parser.

Finite work can still be expensive on a legitimately large file. No default database-size cap, forced database-directory scratch path, shared reader lock, extra cache allowance, public signature or persisted format is introduced. Existing caller limits and optional per-spill limits remain the policy.

The allocation proof bounds requested output buffers and accounted materialization. It does not prove allocator overhead, zstd's C workspace, thread stacks, mmap/page-cache/tmpfs backing, or total process RSS. Live buffers can overlap, and independent readers add their own costs. The native aggregate test keeps eight outputs alive simultaneously and retains the existing 96 MiB RSS ceiling. Resource reports distinguish process RSS, temporary logical/allocated bytes and whole-cgroup memory.

Callbacks and OS operations must return; endlessly interrupted or stalled I/O and intentionally waiting follow APIs have separate liveness assumptions. The shared spill model assumes the standard Write contract (`n <= requested`). Existing mmap immutability requirements remain necessary. Ordinary infallible allocations, including the deferred BTreeMap item #19, preclude a global OOM-free claim.

## Proof attempts and remaining gaps

The first whole-parser model used arbitrary inputs of 0..96 bytes; its verification did not finish within the trial bounds. The general prefix `count * stride` representation induction also did not finish, including a Z3 trial. These attempts are retained as unsuccessful attempts, not counted as passes. The final proofs separate real header/entry parsing from full-width table arithmetic and distinguish the general observation transition from bounded truncation/membership. No normal-metadata assumption was added to make the rejection checks pass.

Initial creation, compaction/path replacement, every filesystem callback implementation, all OS schedules, complete C decoder behavior and arbitrary custom codecs are not formally refined. They continue to have native regression/failure tests, platform CI, fuzzing and/or Miri as applicable. The repository's coverage job reports line/region coverage separately; running all feature configurations does not turn unmodeled code into a proof.

## Qualification

Local checks passed: 1,325 workspace/doc tests (30 existing ignored), 33 required
release storage/resource contracts, 46 Python tooling tests, warnings-denied
Clippy, no-default-feature compilation, formatting, actionlint, the public API
and renamed-dependency fixtures, and both Loom models. Kani 0.68.0 verified all
28 harnesses under default and all features. All 11 publication and 10 resource
mutants were detected; strict exit/unwind/unsupported-result classification was
also reapplied to the saved logs.

AddressSanitizer-enabled fuzz binaries completed 3,623,475 codec, 303,310 native
and 20,868 sidecar-mutation executions with the existing 4 GiB fuzzer RSS limit.
The `__asan_init` symbol was checked in all three binaries. The aggregate decoder
release trials observed 8,455,080–8,586,152 bytes of peak additional Rust heap
while eight 1 MiB outputs were live; peak RSS was 76,224–76,416 KiB against
the unchanged 98,304 KiB ceiling.
The portable Miri selection is 199 of 251 tests, with 52 explicit exclusions;
this local follow-up checked selection only. Full Miri and platform results are
reported by CI on the published head, separately from these local runs.

Local proof reports identify their source snapshots as after-only; a final source
fingerprint is retained separately. CI captures and compares both sides of each
run on its clean checkout. [Lossless logs](logs.tar.gz), indexed by [original-byte hashes](log-index.json), include unsuccessful proof experiments and
fixture/tool invocation corrections; only the completed final runs above count.

[Performance qualification](../../performance/metadata-verification-20261102/README.md)
reports each workload and all initial/confirmation samples. Benchmarks use the same large/complex workloads and fixed writer/reader work as the baseline, unchanged four-CPU/16 GiB environment limits, alternating order, and no concurrent compilation or verification. All initial and confirmation observations are retained.
