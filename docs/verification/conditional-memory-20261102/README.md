# Conditional memory safety, excluding BTreeMap

Baseline: PR #18 head `50866e5df7bd5b49b3c3e1871400ee6fe90e9674`. This stage adds executable decoder contracts and allocation-failure sweeps. `BTreeMap` allocation remains deferred; neither its implementation nor its tests are removed. The acceptance target below is conditional, and the whole-library target is **not yet established**.

## Contract and assumptions

For a finite immutable input snapshot and external callbacks that return, attacker-controlled lengths/counts must either pass checked input and resource bounds or return an error before the corresponding reservation. Successful charges decrease the remaining budget; rejected charges leave it unchanged. A child decode, including an error after a successful charge, must return its remaining budget to the parent. Retained sibling results must use the same cumulative budget. Zero-wire-width collections still consume a finite count/materialization allowance.

Allocation refusal must return an existing typed error, release partially built values, preserve the immutable input, and permit a subsequent retry. This is tested at every observed Rust allocation in the selected built-in decoder fixtures, with all later allocations also refused. It is not an enumeration of every allocation on every input.

A conditional process bound would require the following inequality over **simultaneously live** objects:

```text
resident_before_operation
+ retained_outputs_and_operation_buffers
+ decoder/native_workspaces
+ allocator_overhead_and_thread_stacks
+ charged_mappings_and_temporary_backing
<= available_committed_memory
```

Independent readers add their live costs. A per-record budget cannot be reused as a cumulative bound while the caller retains prior results. Existing materialization limits are accounting policies, not a shared global allocator or a reservation of physical memory. The right side must remain available; kernel overcommit, unrelated processes and external mmap mutation/truncation are not controlled by these proofs. Input and callback types are fixed and custom codecs must honor accounting and terminate.

## Verified defect: overlapping HashMap tables

The decoder screened one final table against the materialization budget, but growth temporarily retains both the previous and replacement tables. On the baseline, 1,793 `(u64, u64)` entries charged **69,696 bytes** while observed live Rust allocations peaked at **104,480 bytes**. The new peak-vs-charge regression failed on that baseline.

Small maps, up to the existing 1,024-entry upfront reservation, keep their original charge. Larger declared maps charge two conservative final-table bounds before decoding entries. Each old/new table fits one final bound; nested payloads are charged separately. This is a conservative bound: some counts above 1,024 can fit the actual first table, but still reserve the two-table allowance in the policy. The multiplication is checked and overflow rejects. The existing hash-table layout assumption (supported std implementation, control group no wider than 32 bytes) remains explicit.

No budget, default file-size cap, public signature or persisted format changes. A large map near an existing finite materialization ceiling can now reject earlier because the old charge omitted live memory. Streaming over a large file is not charged cumulatively when each decoded record is released. No general performance improvement is claimed.

## Executable evidence

Six additional Kani harnesses exercise actual decoder methods: full-width charge arithmetic; collection preflight with full-width count/element sizes and a 64-byte input; bounded real-slice cursor progress; nested error accounting; retained sibling accounting; and the HashMap overlap charge instantiated with zero-sized keys/values over the full count range. Sized/growing/nested maps are also checked by native tests. Nested/accounted models use a returning callback that charges through the actual public interface. They do not prove arbitrary custom codec bodies or allocator implementations.

The proof manifest requires the complete set under default and all features, with matching before/after source fingerprints. Five new negative controls omit a charge, remove the zero-sized-element floor, refund nested/accounted charges, or omit the overlapping table. Compilation errors, timeout, unwinding failures and unrelated failed assertions do not count as detected mutations. The isolated mutation fixture extracts the actual decoder method bodies; its reduced error/type scaffolding is not a whole-library refinement.

Native cases cover byte/string/sequence decoding, nested strings, stack and heap arrays, option/tuple, growing and nested HashMaps, high field-ID bookkeeping, and chunk output. Both endian modes are included. Table growth boundaries have positive exact-budget witnesses and reject one byte below the required charge before touching the allocator. Existing public index-read refusal/retry, C zstd context/workspace refusal, aggregate concurrent decode, storage recovery, hostile metadata and termination contracts remain required.

## Remaining obligations before a whole-library claim

| Area | Current evidence / obligation |
| --- | --- |
| Built-in decode budgets | Arithmetic/transfer proofs plus native refusal and live-byte tests; not whole arbitrary-input parser refinement. |
| Hash table implementation | Conservative layout model plus measured reservation/growth guards; std/hashbrown internals are not formally verified here. |
| C zstd contexts/workspace | Existing native malloc refusal and ASan/fuzz checks; no formal C workspace/termination bound. |
| File open and snapshot ownership | `SnapshotFile::new` creates an `Arc`; setup allocation must fit caller-provided headroom. Not covered by decoder refusal sweeps. |
| Public clone and returned collections | `BlockVec::clone` copies owned entries; public `Clone` cannot return `Result`. Caller-retained copies need a cumulative bound; no unconditional fallible-allocation claim. |
| Error construction and other operations | Some diagnostic/wrapper paths allocate (`std::io::Error::other`, boxed disk-index errors). A complete failure-return theorem requires auditing these paths too. |
| OS, mmap, spill backing, allocator overhead and stacks | Require explicit environment/caller bounds; process RSS alone does not account for all of them. |
| BTreeMap | Explicitly deferred by the user; no attempt to certify or replace its allocation behavior in this stage. |

These remaining rows are **obligations, not assumed-away passes**. Passing this stage establishes the listed contracts and fixes the reproduced undercharge; it does not establish that BTreeMap is the only remaining obstacle to global OOM freedom. The previous whole-parser and general prefix proof gaps remain documented in the [hostile-metadata report](../hostile-metadata-20261102/README.md).

## Qualification results

Final local checks: 1,328 workspace/doc tests (30 existing ignored), 36 required release storage/resource contracts, 46 tooling tests, warnings-denied Clippy, no-default-feature compilation, API/rename fixtures, formatting and actionlint passed. All 34 Kani harnesses passed under both default and all features with matching source fingerprints. All 15 resource and 11 publication mutants were detected. ASan codec fuzzing completed 3,518,857 executions under the unchanged 4 GiB RSS limit.

The release decoder sweeps cover 20 fixtures / 122 refusal points plus two chunk-output refusal points. The rehash regression spans seven growth-boundary counts and exact/one-byte-insufficient budgets. The lossless [evidence archive](evidence.tar.gz) and [index](evidence-index.json) retain failed baseline, superseded proof trials and fixture-marker corrections alongside final passes; only completed final checks count. [Validation summary](validation.json) and [performance qualification](../../performance/conditional-memory-20261102/README.md) keep scope and resource evidence separate. Remote checks must be evaluated on the newly published head; baseline success is not carried forward.
