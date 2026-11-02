# Ownership allocation contracts

This follow-up to PR #18 starts from `213e2349406bf5ad4de1f052672b40c892bd634a`.
It strengthens `TryClone`, fallible Box allocation and private shared ownership.
The contract ends when success or an error reaches the caller; the caller chooses
whether to retry, stream, release memory or abandon the operation.

## Implementation

`try_box_uninit<T>()` reserves `Box<MaybeUninit<T>>` without requiring a constructed
`T` argument. Dropping it frees storage without dropping an uninitialized `T`.
Callers can use `Box::write`, or initialize through a pointer and uphold
`assume_init`'s safety requirements. A native/Miri regression fills a 16 MiB
array directly in heap storage from a thread with a 64 KiB native stack budget.

`try_box_with` reserves storage before calling a fallible initializer. Refusal
skips the initializer, its error is forwarded unchanged, and partial storage is
released. `Box<T>::try_clone` now uses this ordering. `try_box(value)` keeps its
existing signature and drops its already constructed argument on refusal.

Private shared owners reserve a `triomphe::UniqueArc<MaybeUninit<T>>`, initialize
it exactly once, then transfer the same allocation to a shareable Arc. Uniqueness
is represented by the owner type during initialization. Copy-on-write reserves
the replacement owner before cloning its payload and replaces the original only
after both succeed. Failure leaves the retained source intact. Public standard
Arc types, existing signatures and on-disk formats remain compatible; no new
runtime dependency was added.

The `with` initializers still return `T` by value. They do not by themselves remove
large stack temporaries from arbitrary callback code. The uninitialized Box API
supports caller-controlled in-place initialization.

## Preconditions and scope

The API boundary starts when the function is entered; evaluating its arguments
belongs to the caller. Allocator/deallocator calls must return normally and obey
Rust's allocation contract; allocation refusal returns null. User initializers,
`TryClone` implementations, hash/equality
callbacks and destructors must return normally and uphold their own contracts.
A user implementation that panics, aborts, allocates infallibly or never returns
cannot be made fallible by the trait signature alone.

Shared clones retain the underlying Arc reference-count overflow guard. This
work does not prove arbitrary concurrent reference-count schedules or turn that
guard into an error. It also does not model OS termination, allocator aborts,
external signals, stack exhaustion, invalid caller unsafe code or all dependency
internals. Public deep `Clone`, existing infallible convenience APIs and
`BTreeMap` remain outside this work. **This is not unconditional whole-library
OOM freedom, nor a universal proof over every Rust type and container length.**

## Executed production code and verifier boundary

The implementation also has a small structural argument under the preconditions
above. Box reservation performs at most one allocation and has no retry loop:
null returns an inline error, while a valid pointer is immediately adopted as
`Box<MaybeUninit<T>>`. Initialization then either returns its error while that
owner releases storage, or writes a `T` and transfers ownership without another
allocation. The shared-owner path follows the same order through UniqueArc.
Copy-on-write publishes only a fully initialized replacement, which is still
uniquely owned when mutable access is obtained. Slice cloning reserves for its
finite input length once, advances one element per returning callback, and uses
the output vector's initialized length to clean up a partial result.

This argument depends on Rust/triomphe layout, ownership and allocator contracts.
It is code-level reasoning, not a mechanized induction over arbitrary types or
slice lengths. The concrete model checks below provide stronger exhaustive
evidence within their stated input domains.

These proof runs target x86-64 Linux. The ownership harnesses call the
production helpers in `fallible.rs`, including
the pinned triomphe initialization and destruction paths. They replace allocator
entry points with a model that refuses a symbolic allocation position and every
later request. Successful allocations/deallocations delegate to `System`.
Native refusal sweeps separately intercept the actual global allocator.

The model tracks outstanding requested bytes. Payload destructors and partial
owner destruction actually execute. Returned errors are forgotten at the caller
boundary to avoid unfolding the unrelated recursive `Error` destructor graph.
The harnesses only construct the heap-free allocation/layout errors and
`io::Error::from(ErrorKind::OutOfMemory)`; this is not a proof that arbitrary
caller-provided errors have no owned resources. Native and Miri checks drop the
real returned errors normally.

Kani 0.68's allocation-stub build produced a false empty-vector state: the CBMC
trace returned capacity 1 and an invalid pointer from `RawVec::new_in`, then
attempted to reallocate that pointer. An isolated one-harness reproducer and
counterexample traces are retained. The same COW code passes without allocator
stubs and passes native/Miri refusal checks. The ownership proofs therefore use
an explicit standard empty-Vec constructor model via `Vec::from_raw_parts` with
a non-null aligned dangling pointer, zero length and zero raw capacity. A separate
harness checks its empty, aligned, zero-sized and allocation-free behavior.
This is an additional trusted standard-library boundary, not a production fix
or a proof of Kani's representation of `Vec::new`.

Every vector clone in these harnesses reserves once into an empty vector. A
model trap rejects any attempt to grow an existing allocation; later pushes and
copies must fit the initial reservation. No owner/clone implementation is
replaced by a successful-result stub. Unwinding assertions remain enabled.

## Proof obligations

| Family | Quantified inputs and checked behavior |
| --- | --- |
| Box value and storage | Symbolic byte value and refusal; correct value, exactly one input destruction, no retained allocation; 64-byte alignment and 128-byte-aligned ZST |
| Box initializer | Independent refusal/initializer-error booleans; initializer skipped on refusal, error preserved, storage released, initialized value destroyed once |
| Box `TryClone` | Owner reservation precedes the payload callback; callback error preserved; source and allocation balance retained |
| Byte vector clone | Arbitrary four-byte input and length 0–4; every refusal position; equivalent copy or error, unchanged source, released output |
| Nested owned clone | Separate sizes 0, 1, 2 and 3, symbolic allocation refusal and failing item; production slice cloning of boxed payloads, partial-owner cleanup and unchanged source |
| Capacity arithmetic | Full-width `usize` requests for `Vec<u16>` under allocator refusal; overflow rejected without allocation, valid nonzero requests fail cleanly, zero succeeds |
| Shared value/initializer | Symbolic value/refusal/initializer error; correct payload, skipped initializer, preserved error and exactly-once destruction; last of two shared owners releases storage |
| Shared vector initialization | Preserves a three-byte owned vector through uninitialized-to-initialized shared ownership |
| Copy-on-write | Three-byte vector, unique/shared input and symbolic refusal position; replacement completes before publication, failure retains pointer/content, retained reader stays unchanged |
| Model calibration | Empty-vector model has the documented length/capacity/alignment and requests no storage, including a ZST |

The manifest registers 15 ownership harnesses alongside the existing 35 proofs.
The nested sizes are separate verifier executions of the same production loop;
this avoids unrelated input construction/growth and keeps the original 0–3
bound. It is not an induction proof for arbitrary container lengths. The
full-width capacity proof covers arithmetic/refusal, not all loop executions.

Five negative controls restore incorrect allocation/initialization ordering,
reject valid Box allocations, or leak failed initialization storage. Each must
fail at its named assertion; compiler failures, verifier crashes, timeouts and
empty selections are not accepted as detections. The production full-crate
proofs use the actual Error type. Only the isolated mutation controls reduce
Error to the three variants exercised by those controls.

## Qualification and evidence

Final qualification results and per-unit performance are recorded in
[the performance report](../../performance/ownership-contracts-20261102/README.md).
Source fingerprints must match before and after full default/all-feature proof
runs. The CI manifest rejects missing, duplicate or incomplete proof selections.
The allocation and initialization regressions are also required storage contracts.

Failed development attempts remain in the evidence archive. Two CBMC workers
were killed at the existing memory limit; other oversized attempts were stopped,
and one counterexample-playback attempt timed out. These are verifier failures,
not application OOM reproductions, and are not counted as passing proofs. CPU,
memory and disk budgets were not increased. The final model correction and
bounded input construction were checked under the same budgets.

Additional initialization/valid-value instrumentation could not be qualified
with Kani 0.68. The combined `-Z uninit-checks -Z valid-value-checks` run stopped
in the compiler's `check_values.rs` handling of a non-null raw slice pointer
pattern ABI. A separate `-Z uninit-checks` run stopped in
`initial_target_visitor.rs`: resolving a standard `std::io::ErrorKind` function
pointer is unsupported by that instrumentation
([Kani issue #3300](https://github.com/model-checking/kani/issues/3300)). Both logs are
retained. These compiler failures are neither application counterexamples nor
passing initialization proofs. The registered proof results use Kani's standard
checks; the 12 production-code Miri tests separately check the exercised ownership
and initialization paths. Miri is dynamic evidence, not a replacement universal
formal proof of uninitialized-memory or value validity.
