# Known Limitations

This describes the unreleased 0.12.0 implementation. Historical measurements
below identify their original platform and are not new performance claims.
Private spill files bound retained directories; they do not remove initial
scans, temporary disk requirements, per-record workspace or filesystem limits.

## 1. Matrix: opening a matrix is not O(1), because opening it verifies it

Residency is no longer part of this section's problem. As of 0.5.0 matrix commit
metadata is demand-filled and bounded by a declared cache — §1.2 and §1.3, which
used to record the opposite, now record what is left. What remains is I/O: the
default open **authenticates the whole commit map**, and that read is
`O(candidate pages)`. It is a declared policy with an off switch, and the numbers
below are what it costs.

### 1.1 Open cost is proportional to candidate pages, not to the working set

**What it is.** A matrix's commit metadata is stored as 4096-byte bitmap pages,
each page covering 32,768 cells of one category. Under the default
`MatrixMetadataVerification::AtOpen` policy, `open` **reads and authenticates**
every page in the *candidate set*: the union of the pages named by the persisted
page index (`L`, the pages actually holding state) and the pages the platform's
allocation map reports as written (`A`). Cost is `O(L + A)` bytes read.

**Reading is not retaining, and since 0.5.0 nothing is retained.** The
verification pass reads each candidate page into a single reusable 4096-byte
buffer, authenticates it against its stored page digest, folds the finding, and
moves on. So the I/O is `O(L + A)` and the residency contribution is **zero** — do
not size resident metadata from the bytes read, and do not expect the reads to
bound memory in either direction.

`O(L + A)` is not `O(1)`, and neither `L` nor `A` is the working set. A reader
that intends to touch one cell and leaves verification on pays for the whole
candidate set in I/O.

**Measured on this host, 2026-07-26** (`crates/stratava/tests/matrix_lazy_residency.rs`
and `matrix_integrity_scaling.rs`, all tests passing):

| Fixture | Cells | Live pages | Bytes read, `AtOpen` | Pages visited | Bytes read, `OnDemand` | Resident bitmap bytes |
| --- | --- | --- | --- | --- | --- | --- |
| small | 2,097,152 | 1 | 69,800 | 17 | **32** | 0 |
| large | 8,388,608 | 1 | 69,800 | 17 | **32** | 0 |
| large | 8,388,608 | 64 | 267,800 | 65 | **1,040** | 0 |

Read those rows in this order:

1. **Open is independent of file size.** Two matrices differing 4x in cell count
   and 4x in file size, with the same one-page live set, read the same 69,800
   bytes. This part of the design goal is met — **on a sparse NTFS volume.** See
   the platform precondition below, which is not a footnote.
2. **Open is not O(1) while it verifies.** A matrix with **one live page** still
   reads **69,800 bytes** and visits **17 pages**. On NTFS the allocation map
   reports the bitmap region in ~128 KiB runs, so `A` is tens of pages even when
   `L` is 1. That is a floor, not an average.
3. **The whole of it is verification.** The same opens under
   `MatrixMetadataVerification::OnDemand` read **32 bytes** and **1,040 bytes**
   and visit **0** pages: an 8-byte page-index entry per live page, and no
   allocation-map query at all. The gap between those two columns is the price of
   detecting commit-map damage at open, and it is a one-line declaration.
4. **Residency is zero in every row**, at any live-page count, under either
   policy. That column used to read `8,192 * live_pages`.

#### The numbers above are Windows/NTFS numbers, and `A` is filesystem-defined

This matters enough to state before the mitigations, because on two of the three
cases below the headline result is *different*, not merely less precise.

- **Windows and Linux, sparse volume (measured case).** `A` comes from
  `FSCTL_QUERY_ALLOCATED_RANGES` / `SEEK_HOLE`. On NTFS the ~128 KiB run
  granularity gives tens of candidate pages around a single live page, which is
  where the ~70 KB floor comes from. Linux extent granularity differs, so the
  constant differs; the shape does not.
- **Every other target.** `query_allocated_extents` returns `None`
  (`matrix.rs`, `#[cfg(not(any(windows, target_os = "linux")))]`), so the
  candidate set has **no `A` term at all** and verification visits `L` pages only.
  Open is *cheaper* there than the table above shows. It is also blinder: the `A`
  term is the only thing that detects stray bytes in a page nothing ever
  published. This is untested — see §6.1.
- **A volume that is not sparse.** Sparseness is requested best-effort. On
  Windows the file must carry the sparse attribute *before* it is extended, and
  `mark_file_sparse` **ignores failure** by design ("the matrix is then simply
  dense, which costs performance and nothing else"). On exFAT/FAT32, some
  network and virtual volumes, or after a copy by a tool that expands holes, the
  declared extent is genuinely allocated — the allocation map then reports the
  **whole** bitmap region as data, every page becomes a candidate, and a verifying
  open degrades to `Theta(declared cells / 8)` of reads. **On a non-sparse volume,
  open cost is proportional to the declared cell count, not to live state, and the
  "independent of file size" result in point 1 does not hold.** There is no error
  and no warning; the only signal is that open is slow and reads a lot. Memory is
  unaffected — the pass still retains one page.

The same precondition governs the create-time claim: metadata I/O at create is
bounded by live state because the unwritten extent is a hole. Where it is not a
hole, `set_len` allocates what it reserves.

**Who it affects.** Anyone who opens matrix files frequently — short-lived
processes, request handlers, CLI invocations, per-file workers — or who opens
many matrices in one process. A long-running process that opens one matrix and
keeps it open pays this once and does not care.

**Workaround.** Three, and the first is complete:

- Declare `MatrixMetadataVerification::OnDemand`, and call
  `verify_matrix_metadata()` on whatever schedule suits you — once per file, on a
  scrub, never. Read §1.4 first: what you give up is the matrix *announcing*
  damage at open, the whole-category quarantine, and the strict-recovery writer
  gate.
- Keep one reader open per reading thread. Matrix reads take `&self` (§4),
  and each reader owns its cache.
- Nothing needs doing about memory: an open retains no commit-map payload.

**VMAT v5.** OnDemand opens now read validated directory headers without
traversing the live-page list. Direct page-to-slot metadata replaces the old
reconstructed mirror. AtOpen still performs full verification by choice.
Earlier timing/byte tables in this section describe the older representation.

<a id="12-what-residency-still-costs-the-page-index-mirror-and-a-per-bitmap-cache-bound"></a>

### 1.2 Bitmap directories read original metadata

Each directory stores a count, forward slots and a sparse reverse array in the
matrix generation. The reverse entry is addressed by bitmap page number; its
forward link is cross-checked before a published page is used. Readers cache
bounded metadata windows, configured through `with_matrix_directory_cache_bytes`
(default 1 MiB per bitmap). Zero disables retention. A miss reads the same
original metadata regardless of cache capacity; no mirror or scratch index is
constructed. Normal follow streams changed metadata ranges and keeps the old
root on any preparation failure. Aggregate queries still stream relevant pages.

The reverse array doubles the logical entry-region reservation from 8 to 16
bytes per possible bitmap page, plus the existing header. Zero extents and the
COW generation layer avoid eagerly writing that entire reservation. Actual
physical size follows touched metadata windows and publication history.

<a id="13-max_matrix_bitmap_bytes-bounds-a-cache-and-the-residual-admission-limit-is-the-index-mirror"></a>

### 1.3 Bitmap limits control the working set

`max_matrix_bitmap_bytes` still limits admitted writer bitmap pages and clamps
reader caches. A reader can open and follow when the complete persisted index
exceeds that limit. A single writer operation still needs its working page(s);
zero or undersized budgets may reject that operation. Cache bounds are per
bitmap, so schema bitmap count also affects total memory. Allocator overhead and
fixed tree/framing scratch are additional; these are not whole-process RSS caps.

### 1.4 `Lazy` is the default; verification is what still happens at open

**As of 0.5.0 an undeclared open is
`MatrixMetadataResidency::Lazy { DEFAULT_CACHE_BYTES }` plus
`MatrixMetadataVerification::AtOpen`.** The project's standing policy — open reads
a header, pages fault in on demand, memory is bounded by the working set and never
by total file content — is met for residency. What is *not* bounded by the working
set is the verification read (§1.1), and that is now a separate declaration
rather than a consequence of how much is kept in memory.

Numbers below are measured on Windows/NTFS by `matrix_lazy_residency.rs`. The left
column is what a user who has never heard of either option gets.

| | undeclared (`Lazy` + `AtOpen`) | `Lazy` + `OnDemand` |
| --- | --- | --- |
| Open bytes read, 1 live page | 69,800 | **32** |
| Commit-map pages visited at open, 1 live page | 17 | **0** |
| Open bytes read, 64 live pages | 267,800 | **1,040** |
| Commit-map pages visited at open, 64 live pages | 65 | **0** |
| Resident bitmap bytes after open | **0** | **0** |
| Resident after touching 4 distinct live pages | 16,384 (exactly 4 x 4,096) | same |
| Resident after touching all 64 live pages | 262,144 (exactly 64 x 4,096) under the 2 MiB derived cache — 12.5% of it, so nothing was evicted; 16,384 under a declared 16,384-byte cache, an exact LRU cap | same |
| Eviction | LRU, to the declared bound | same |
| Allocation-map query | yes (where the platform has one) | never |
| Commit-map damage reported by | `open` | `verify_matrix_metadata()`, when called |
| Damaged category failed closed as a whole | yes (`Error::MatrixCommitQuarantined`) | **no** — only the pages a read touches refuse |
| `RecoveryPolicy::Strict` writer gated on a damaged category | yes | **no** |
| A live set over `max_matrix_bitmap_bytes` | opens | opens |

**What you give up with `OnDemand`, stated as the loss it is.** The commit-map half
of `MatrixRecoveryReport` is produced by the verification pass and by nothing else:

- the `Recoverable` `MatrixCorruptionKind::CommitMap` finding is not produced;
- `Error::MatrixCommitQuarantined` does not fire, so a damaged category is not
  failed closed **as a whole** — measured: with one damaged commit-map page a
  verifying open refuses a *clean* page in that category with
  `MatrixCommitQuarantined("analysis")`, while a non-verifying open answers that
  clean page `Ok(Committed)` and refuses only the damaged one;
- the `RebuildCommitMap` / `ClearCategory` recommendations are absent, and with
  them the documented rebuild-recovery path;
- a `RecoveryPolicy::Strict` **writer** is not stopped from mutating a category
  whose commit map holds a damaged page;
- and in one case it is worse than deferral: the candidate set is the persisted
  page index **unioned with** the platform's allocation map, and it is that second
  term which catches stray bytes in a page nothing ever published. Such damage is
  never examined at all, not merely examined later.

`verify_matrix_metadata()` recovers the first three on demand — it is the same
pass, and it reports the same findings and the same recommendations — but not the
quarantine and not the writer gate, which are derived once, at open, from the
findings the layout is assembled with. **No read answers from unverified bytes
under any policy**, so none of this is a soundness hole: every page faulted in is
authenticated against its stored digest before a bit of it is reported, and so is
every page an aggregate count reads.

**Verification remains work.** AtOpen streams entries and verifies candidate
pages. OnDemand defers entry reads and validation to access, so it does not give
an all-directory corruption verdict at open. Explicit verification restores that
scan; destructive CRC rebuild validates its directory before trusting absences.

**Three consequences of demand residency that are part of the declaration, not
defects:**

1. **A page is authenticated at fault-in.** A page whose bytes disagree with its
   stored digest is reported as `Error::MatrixFatalCorruption` by the read that
   touches it. That is a per-page refusal, not a verdict on the map; the verdict is
   `MatrixMetadataVerification`.
2. **A page absent from the persisted index still reads as clear.** That is a
   fact read from the original directory, not a cache miss. "Not cached" and
   "not published" stay distinct across eviction. OnDemand cannot diagnose
   unrelated directory corruption until that metadata is read or scrubbed.
3. **Cache residency does not decide visibility.** Every fault reads the captured
   generation, including after eviction; `follow()` explicitly adopts a new one.

<a id="15-the-default-limits-cap-a-matrix-at-16000000-cells--create-fails-above-it"></a>

### 1.5 Default matrix admission is separate from memory budgets

`ReadLimits::STANDARD` no longer imposes the former 16,000,000 dimension/cell,
128 MiB checksum-extent or 8 GiB slot-region caps. Their default is `u64::MAX`;
checked shape products, offsets, format representability and filesystem limits
still apply. Explicit `with_max_matrix_cells`, dimension, checksum or region
limits remain admission policies. Descriptor-memory, bitmap-cache, generation
cache and pending-buffer limits remain finite.

A large declared extent is economical only where the filesystem supports sparse
allocation. Sparse-file tests are not evidence of physical multi-gigabyte I/O
throughput. Growing dimensions still require an explicit chunk declaration.

### 1.6 The matrix residency and verification policies have no `stratava_format!` DSL key

> **The bug this section used to describe was fixed in 0.5.0.** Through 0.4.0,
> `*_with_resource_limits` **silently discarded** a spec-declared
> `matrix_metadata_residency`, because `MatrixMetadataResidency` had no unset
> state and `ReadLimits::STANDARD` therefore carried a real eager declaration that
> `overlay` took outright. A caller raising an unrelated bitmap ceiling lost the
> only bound on matrix metadata memory (§1.4) and could then be refused at open by
> the very admission limit they were raising (§1.3) — measured, pre-fix:
> `LimitExceeded { resource: "matrix bitmap bytes", actual: 33536, limit: 32768 }`.
> **If you wrote a workaround for that, it is now unnecessary** (still harmless).
> See [API Changes §A.2](api-changes.md#a2-_with_resource_limits-composes-a-declared-residency-policy-instead-of-replacing-it).

**What it is.** `matrix_metadata_residency` — and, since 0.5.0,
`matrix_metadata_verification` — compose like every ceiling beside them: a silence
never overwrites a declaration.

| Entry point | Format declared, caller silent | Caller declared, format silent | Both declared |
| --- | --- | --- | --- |
| `*_with_resource_limits` (`overlay`) | format's policy **kept** | caller's policy taken | caller wins |
| `*_with_limits` (`tighten`, legacy fieldwise) | format's policy kept | caller's policy **taken** | format wins |
| plain `open`/`create` | kept | — | — |

`tighten` still lets a format's declaration outrank a runtime one, because a
policy has no "tighter" direction to meet. What changed in 0.5.0 is that a
*silence* no longer outranks anything, in either function, for either policy.

**What remains a limitation.** There is **no `stratava_format!` DSL key** for either
policy. `limits { }` accepts eighteen integer keys, `file_len` through
`header_extension`, but not these two, and no key was added: the grammar is
`key: <integer literal>;` over a fixed key list, which cannot express a variant,
with or without a payload. Both are reachable two ways — from a spec, through
`FormatSpec::with_read_limits` / `with_resource_defaults`, or at a call site by
passing them in the same `ReadLimits` value you hand to the entry point:

```rust
let limits = ReadLimits::STANDARD
    .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes: 1 << 20 })
    .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand)
    .with_max_matrix_bitmap_bytes(n);
```

As of 0.5.0 the settings no longer have to travel together — raising the ceiling
alone will not destroy a spec-declared policy — but a `ReadLimits` value is still
the only place a *call site* can name one. Both are reader-side policies, so the
call site is a legitimate home for them: nothing about the file depends on either.

**Who it affects.** Anyone who wants these declared in the `stratava_format!` macro
rather than in Rust. Macro-declared formats emit `ReadLimits::missing()`, so they
carry neither policy and inherit `MatrixMetadataResidency::DEFAULT` and
`MatrixMetadataVerification::DEFAULT` (§1.4).

### 1.7 Clearing a category costs `Theta(cells / 8)` unless the platform can punch holes

**What it is.** `clear_category` and a whole-map page-index rebuild remove the
byte range rather than writing it, which is `O(1)` in the range length — but only
where the platform supports it. `punch_zero_range_native` is implemented for
Windows (`FSCTL_SET_ZERO_DATA`) and Linux (`FALLOC_FL_PUNCH_HOLE`) and returns
`false` on **every other target**. Every failed attempt, and every unsupported
target, streams `len` zero bytes instead — for a whole-category clear that is
`Theta(declared cells / 8)` bytes written.

On macOS, and on any Windows/Linux filesystem without sparse support, clearing a
16,000,000-cell category writes 2 MB where the supported path writes nothing.
Scale that to the cell counts §1.5 lets you raise the ceiling to and it is the
difference between an instant operation and a multi-gigabyte write.

**Who it affects.** Anyone calling `clear_category`, or triggering a whole-map
page-index rebuild, on a non-Windows/Linux target or a non-sparse volume.

**Workaround.** There is no fallback that avoids the write. There *is* detection:
the outcome is recorded unconditionally and neither counter is feature-gated. Two
scope rules decide whether a reading means anything, and both will otherwise be
read as a clean bill of health:

- **A clear issues several range requests** (validity bitmap, page indexes, page
  digests, commit map), and
  `MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes` reports only the
  last of them, so `0` from it does not qualify the whole clear. Take
  `matrix_total_zero_range_streamed_bytes` before and after instead; a nonzero
  delta is exact proof that some range streamed.
- **Both counters are thread-local.** Sample them on the thread that ran the
  clear. A clear performed on a worker thread reads as `0` from anywhere else.

`matrix_sparse_zeroing_supported()` reports the compile-time platform capability
only and is not sufficient on its own. Check the counters rather than inferring
from the target triple.

**Planned.** No additional platform is scheduled. The cost model is stated above
in full — `O(1)` where `punch_zero_range_native` succeeds, `Theta(declared cells
/ 8)` bytes written everywhere else — because this file is the one organised by
what a user hits.

### 1.8 Matrix dimensions are fixed at create, unless one is declared growing

> **This section used to say the opposite.** It said there was no grow path and
> that none was planned. One dimension may be declared growing as of 0.6.0, and
> rows past its declared extent land in chunk records in the append log — see
> [API Changes §C.2](api-changes.md#c2-a-written-matrix-chunk-can-now-be-edited-and-errormatrixchunknotreopenable-is-new)
> for what a written chunk costs. Everything below about over-provisioning a
> *fixed* dimension still holds, and it is still the cheaper answer wherever you
> can name a ceiling.

**What it is.** A matrix's dimensions are supplied to
`create_with_dims` / `create_new_with_dims` and are part of the created layout.
There is no `grow`, `resize`, or `extend` operation anywhere in the public API.
A fixed dimension that runs out of rows cannot be enlarged; the file must be
recreated and its contents copied.

**The one exception is declared at create.**
`FormatSpec::with_growing_matrix_dimension(name, rows_per_chunk)` makes
rows past the declared extent land in *chunks*: ordinary internal records in the
append log, each covering `rows_per_chunk` rows of every matrix block. `name`
must be dimension 0 of every matrix block, and its declared value at create must
equal `rows_per_chunk` — the matrix region *is* chunk 0 — refused with
`Error::MatrixSizeMismatch` otherwise. One chunk is buffered at a time, which is
what bounds memory; a write addressing an already-written chunk writes the open
one out and reads that one back, and the rewrite lands where the record already
sits, so the file does not grow. The declaration is folded into the schema hash
only when it is made, so a format that declares none is byte-identical to one
written before the option existed.

**Who it affects.** Anyone modelling an open-ended stream — a growing acquisition,
an append-only log of scans, anything whose extent is not known when the file is
created — who wants it in a matrix with no growing dimension declared. **A fixed
dimension cannot model an unbounded stream; a declared growing one can.**

**Workaround.** Use the append-log APIs for unbounded growth:

- Resident: `StratavaFile` / generated typed writers, for data that fits in RAM.
- Stream/indexed (default API): `StratavaStreamWriter` / `StratavaIndexedWriter`
  and their readers, for bounded-memory ingest and point lookup over data far
  larger than RAM. See [Scalable I/O](scalable-io.md).

**Over-provisioning is the answer for a dimension whose extent you can bound,
and it is close to free.** Declare the ceiling, not the expectation. The region
is a hole: `create_layout` marks the file sparse, writes only the fixed-size
descriptor tables, and establishes the commit map, slot region, bitmaps and
checksum table with one `set_len`. Nothing scales with the declared cell count.

**Measured on this host, 2026-08-04**, ext4, `rustc 1.95` debug build. Each row
creates the matrix, writes and commits one cell at the **far end** of the scan
dimension, then reopens:

| declared | cells | apparent size | **allocated on disk** | create | open |
| --- | --- | --- | --- | --- | --- |
| 1,000 x 128 | 128,000 | 1.06 MB | 28,672 B | 0.5 ms | 0.2 ms |
| 10,000 x 128 | 1,280,000 | 10.6 MB | 24,576 B | 0.3 ms | 0.2 ms |
| 1,000,000 x 128 | 128,000,000 | 1.06 GB | 36,864 B | 0.3 ms | 0.2 ms |
| **100,000,000 x 128** | **12,800,000,000** | **105.6 GB** | **40,960 B** | **0.2 ms** | **0.2 ms** |

A hundred million scans costs 40 KB and 0.2 ms more than a thousand does. So
"the dimension is fixed at create" is a limit on *knowing a ceiling*, not a
limit on size — pick a number you will not reach and the cost of being wrong by
five orders of magnitude is four pages.

Three things still bound it, and only the second is likely to stop you:

- The table above is **ext4, with holes**. On a filesystem that does not give
  you them, `set_len` allocates the whole declared extent — see the precondition
  in §1.1. That turns row four from 40 KB into 105 GB.
- Explicit dimension/cell/file caps are checked before creation. The old default
  16,000,000-cell gate is removed in 0.12.0 (§1.5).
- The allocation-map term in §1.1 and the scatter arithmetic in §1.3 both
  respond to a larger declared extent.

**What over-provisioning still cannot do** is represent a stream with no ceiling
at all. Some number has to be named. If there genuinely is not one, that is the
job of a declared growing dimension — or of the append log itself — not of a
matrix dimension that stays fixed.

**Planned.** No second grow path is planned. A matrix cell's address within its
chunk is arithmetic — `slot_region_off + ordinal * stride` — with no indirection
to update, and that is what makes it random-access; the growing dimension buys
growth by adding exactly one indirection layer *outside* that arithmetic — a
chunk directory, built on the first chunked read — which is why it is declared
rather than always on.

---

## 2. Native APIs: bounded memory does not remove scan and scratch I/O

<a id="21-stratavafile-scans-the-whole-file-at-open-and-holds-a-record-index"></a>

### 2.1 Native open can still scan the input

Ordinary `StratavaFile`, reader and writer opens now keep a bounded directory
buffer and spill excess entries into private temporary storage. Scan/follow
staging uses the same strategy. An explicit finite `max_index_bytes` sets each
buffer capacity; the unbounded default uses 2 MiB. This is not a file record-count
limit. Spilled rows use a private read window of at most 64 KiB, also clamped to
the buffer capacity. Fixed row/page scratch is additional.
Explicit caller-owned directory/collection APIs still enforce their result size.

This removes whole-directory RAM growth, **not scanning work**. A format without
usable segment/digest metadata can still frame every record at open. Segment
chains larger than the scan buffer currently fall back to bounded record framing.
Reordered sequences use a temporary uniqueness index. Temp-space exhaustion and
I/O errors can fail open/follow; a failed follow retains the old snapshot.
Use stream/indexed or declared digest routes when bounded open I/O is required.

Replacement/rewrite paths can still build owned staging arrays and enforce their
operation limits. These paths are not claimed to have fully external staging.

### 2.2 A keyed writer's construction cost scales with block types times index size

**What it is.** A generated writer primes one keyed-tail map per keyed block type
at construction, and each priming pass walks the resident record index.
Construction is `Theta(M * N)` for `M` keyed block types and `N` index entries.

The writer's per-block tail cache now spills to a private temporary index when
its nominal byte budget is reached (unreleased 0.12.0). Cardinality no longer
refuses writer construction or append on that budget alone. The in-memory batch
is capped at 2 MiB per block; fixed tree scratch, one encoded key and allocator
overhead are additional. Scratch I/O can fail and is performed before append.
An explicit `key_tail_offsets` request still returns an owned map and can fail
its memory limit.

Construction still walks the input directory once per keyed block type. The
resident input directory and those scans remain costs of this API; use the
stream/indexed APIs to avoid them.

<a id="23-keyed-merge-and-compact-are-resident-only-and-explicitly-not-petabyte-scale"></a>

### 2.3 External merge needs scratch disk and per-record workspace

Merge and compact retain bounded index buffers and one operation's decoded
values. Per-key state, encoded value history and final ordering stay in bounded
RAM buffers until full, then spill independently to private unnamed files.
Explicit finite `max_index_bytes` is honored; the unbounded default uses 2 MiB
per buffer. These are per-buffer capacities, not a process-wide RAM limit;
fixed tree-page scratch and one key/record are additional minimum working space.
`max_materialized_bytes` applies to the current operation, not every value ever
seen. An explicitly requested owned `HashMap` still has a cumulative result limit.

Scratch disk usage grows with input history; disk exhaustion can fail the run.
The output path is replaced only after the complete result is written and synced.
Shard/sequence/physical ordering, tombstones, ops, and custom key equality are
preserved. `*_with_key_limit` remains an optional work/cardinality guard, counting
tombstoned keys too. This is bounded-memory execution, not a claim of measured
petabyte throughput.

`estimate_keyed_merge` still reports record counts and a hypothetical fully
resident structural size. Its byte fields and `peak_resident_structural_bytes()`
are **not current external-merge RAM or scratch-disk estimates**. They exclude
payload heap allocations and allocator overhead.

### 2.4 Under a transaction-marker commit policy, reopening for write discards the uncommitted tail

**What it is.** With `CommitPolicy::TransactionMarker`, records are visible only
behind a commit marker. Two consequences that surprise a first-week user, both
deliberate and both silent:

- **Opening for write truncates.** `truncate_uncommitted_tail_if_needed` sets the
  file length back to the end of the last commit marker. Records appended after
  the last `flush()`/`commit()` in a previous session are **deleted from the file
  on the next write open**, not merely hidden.
- **A file with no marker at all reads as empty.** `scan_records_range` returns an
  empty index when it finds no marker, whatever the file contains. A process that
  appended 10,000 records and exited without flushing reopens to zero records, and
  a write open then truncates them away.

There is no error, no warning and no diagnostic on either path — the index is
simply short. This is the correct behaviour for the policy (an uncommitted tail
must not become committed by a later append landing next to it), but it is a
data-loss shape if you assumed `push` was durable.

**Who it affects.** Anyone on a `transaction_marker` format who appends without
flushing, and anyone who treats process exit as a checkpoint. `on_flush` markers
make every `flush()` a commit point, which is why that is the common declaration.

**Workaround.** `flush()` (or `commit()` under `transaction_marker(explicit)`)
before you rely on anything being in the file, and treat an unflushed tail as
lost. For the exact ordering guarantees see [Durability Model](durability-model.md)
and [Recovery Model](recovery-model.md).

**Planned.** No change. The alternative — retaining an uncommitted tail across a
write open — is the defect this prevents.

---

## 3. Stream/indexed API status

`StratavaStreamWriter`, `StratavaIndexedWriter`, their readers, disk-index plans,
finite-key storage and scan-control APIs are included in the default 0.10.0
development build. No feature enables them, and `--no-default-features` keeps
these APIs available. The former `high-cardinality-dev` flag has been removed.
Generated stream constructors are available alongside resident constructors;
indexed constructors are generated for schemas declaring `key_index = disk`.
The caller still chooses which storage path to open.

This family follows the same pre-1.0 public API versioning policy as the other
storage paths. Promotion is not publication of a crate or proof of unmeasured
hardware behavior. The next deep-test campaign for the final native-index and
matrix-generation implementation remains separate. Historical reports retain
their revision-specific results, including findings since fixed.

- `sync()` and explicit Immediate publish confirmed generations; `flush()` does
  not publish them. Readers opened while the writer is Dirty serve the last
  confirmed generation and advance explicitly with `follow()`.
- Missing, stale and identity-mismatched sidecars have named recovery operations
  (`bootstrap_stream_checkpoint`, `rebuild_disk_index`), not fallback scans.
- Long scans are cancellable via `ScanCancellationToken`; cancellation returns
  `Error::ScanCancelled { progress }` and publishes no new sidecar.
- `compact_index()` explicitly reclaims obsolete sidecar storage. Whole-native-log
  merge/compact still retains resident key maps.
- `scalable-fault-injection` remains opt-in test infrastructure. redb remains a
  development-only comparison oracle and is absent from runtime dependencies.

---

## 4. Concurrency: what is `&self`, and what a reader sees of a writer

### 4.1 What takes `&self` today

**Every matrix read entry point takes `&self`.** All six of them, on all three
handle types — that is the complete inventory, not a sample:

| Matrix read entry point | 0.3.0 | 0.4.0 |
| --- | --- | --- |
| `read_matrix_cell::<T>` | `&mut self` | `&self` |
| `matrix_cell_payload::<T>` | `&mut self` | `&self` |
| `read_matrix_aux` | `&mut self` | `&self` |
| `matrix_cell_status::<T>` | `&self` | `&self` (unchanged) |
| `matrix_aux_len` | `&self` | `&self` (unchanged) |
| `matrix_resume_signal` | `&self` | `&self` (unchanged) |

Each exists on `StratavaReader`, `StratavaWriter` and `StratavaFile`, so 18 signatures.
`crates/stratava/tests/matrix_concurrent_reads.rs` pins 15 of them ("5 entry points
x 3 handle types") by holding two shared borrows of a non-`mut` handle;
`matrix_resume_signal` is the sixth and was already `&self`.

**Ownership contract:** matrix and resident record handles (`StratavaReader`,
`StratavaFile`, `StratavaWriter`, and generated wrappers) are `Send + !Sync`. Move
ownership to a worker, or open a separate reader for each thread. Sharing one
reader through `Arc` or a cross-thread reference fails to compile. Local reads
still take `&self`; changing writer state requires `&mut self`.

Matrix bitmap caches use reader-owned `RefCell<PageStore>` without mutexes.
Cache hits, misses, clear counting and rebuild queries use local borrows;
exclusive mutation preparation uses `get_mut()` without dynamic borrow checks.
Windows private read handles are also cached per owner, without a shared handle
list or thread-local registry. Cloning internal backing state starts a fresh
handle cache; immutable file handles and page bytes can be retained by `Arc`,
while maps, LRU state and eviction budgets remain private.

The existing `ReadLimits::with_matrix_metadata_residency(
MatrixMetadataResidency::Lazy { cache_bytes })` controls each bitmap's cache.
The default is **2 MiB per bitmap**, clamped by the bitmap ceiling, and allocated
on demand. It is not a process-wide or whole-reader budget: multiple bitmap
caches and readers add their memory footprints, alongside page indexes and other
metadata. Each thread can open with its own limit.

Compile-fail cases reject shared raw and generated readers. Runtime tests check
independent caches with different budgets, isolated eviction, warmed-reader
transfer, concurrent payload/aux/status reads, and corruption rejection. The
historical `matrix_region_reads_under_bitmap_lock` audit now counts local cache
borrows across I/O; its name does not imply a remaining mutex. Writer preparation
is checked for zero dynamic cache borrows.

Fixed-work scaling measurements remain in `matrix_concurrent_reads.rs` and
`matrix_lazy_residency.rs`, using one reader per worker. Strict timing thresholds
are manual ignored benchmarks, because shared-host timing is not proof of
ownership or cache isolation. The earlier shared-reader Windows measurements
are not measurements of this implementation.

Matrix storage additionally uses immutable confirmed page generations. There is
no reader mutex or shared mutable page cache in that protocol.

### 4.2 Where a reader gets a snapshot rather than live state

Stratava is **single-writer**, enforced by a native object lock on the target file.
Within that:

- **Resident record reads are a snapshot as of open, until the handle is asked
  to advance.** The record index is built at open, and records a writer appends
  afterwards are not visible to that handle until it calls
  [`StratavaFile::follow`]/[`StratavaReader::follow`], which frames the bytes past the
  end it holds and adopts them to the same commit boundary an open would stop
  at. A handle never advances on its own — that is what keeps a `&self` read
  from observing a record mid-write.
- **Matrix reads capture one confirmed generation** of payloads, commit maps,
  CRC metadata, aux bytes and growing-chunk EOF. The single writer may overwrite
  the same cells concurrently; each reader adopts changes only on `follow()`.
  `flush()` does not publish; `sync()` and explicit Immediate do. The generation
  file is required alongside the primary, and grows until `compact_matrix()`.
  That compaction does not reclaim obsolete chunk records in the native log.
  Ordinary follow diffs immutable roots, skips equal subtrees, and updates only
  changed bitmap indexes and verification pages, retaining unchanged caches.
  Whole-layout rebuilding occurs only when the persisted compaction epoch changes.
  A bitmap with duplicate index slots left by interrupted removal may require a
  scan of that bitmap's resident slots to find surviving duplicates. A reader
  already carrying fatal index damage or a quarantined category must reopen to
  re-enumerate repaired metadata; incremental follow refuses without advancing.
  Invalid changed metadata or resource-limit failures also preserve the old view;
  they neither lock nor stop the writer. Diff staging is bounded by reader
  limits; a large missed run may require higher limits or reopening.
- **Stream and indexed readers open from a published sidecar generation** and see
  that generation, not the writer's in-flight state. `sync()` publishes.
- **Stream/indexed readers use immutable confirmed generations.** Independent
  readers open read-only across threads/processes. Dirty opens read only the last
  confirmed root and EOF; `follow()` adopts later confirmed generations. There is
  no reader writer-admission or shared mutable index cache. See
  [the native protocol](scalable-io.md).
- **Append-only sidecars retain obsolete pages until explicit compaction.**
  `compact_index()` requires a clean writer and publishes a new file without
  waiting for readers. Follow or close old handles to reclaim the old file's physical
  space. Tombstones remain; native-log reclamation is separate. Snapshot counts
  are observational, process-local, and per sidecar identity, not a global
  reclamation gate. On Windows, matrix/index companion replacement requires
  `FileRenameInfoEx` with `POSIX_SEMANTICS` support from the OS/filesystem. An
  unsupported replacement reports an OS error and poisons the writer; it does
  not wait for readers or fall back to deleting the target first. Reopen the
  writer after reconciling the failed operation. See
  [space management](scalable-io.md#explicit-sidecar-compaction).

**Who it affects.** Anyone assuming a Stratava handle is a live view of a file
another handle is writing. It is not.

**Usage.** Each thread owns a reader (`Send + !Sync`). `follow(&mut self)` adopts
new confirmed work. A replaced primary requires `is_current()` and reopening;
companion-only matrix compaction is traversed by `follow()`. A matrix-only
advance returns zero appended bytes; compare `matrix_generation()` if needed.
An unstable publication read may return `WouldBlock`; retry while retaining the
current reader. The writer never waits for a reader retry.

**Storage and cache bounds.** Matrix storage requires both the primary and its
nonce-named `.vmg` companion. Coordinate backups/renames with the writer and copy
both. The generation page cache is separately configurable through
`with_matrix_generation_cache_bytes` (2 MiB default, zero disables). The bound
covers page buffers, not allocator/map overhead. Compaction retains old physical
files until old readers/mappings close. The matrix mmap view overlays changed
pages into a private mapping, so those changed pages consume private memory.
Generation-directory enumeration for allocation hints stops at 65,536 entries;
if unavailable, verification uses the persisted bitmap page indexes, with the
same limits on detecting out-of-band stray writes described in §1.7.

**Planned.** No live-view mode is planned. `follow()` is an explicit advance,
not a live view: between two calls the handle is still a fixed snapshot.

### 4.2b Nothing tells a reader that its generation was replaced

`follow()` advances a handle along the object it opened. The replacement paths
(`replace_fixed`, `replace_block`, `replace_rewrite`, and any future compaction)
publish by writing a new file and renaming it over the pathname — `replace_fixed`
included, which copies the generation and patches one same-length record in the
copy, so it moves no offset but still leaves the old object behind. A handle open
across that keeps reading the old object — which is what makes a republish safe
for readers in flight, and is measured: a handle holding 21 records read them all
back, unchanged and without error, after the pathname had been replaced.

**There is no notification, and there is no design that would give one for
free.** A handle learns its generation was replaced only by asking, and the
question costs a syscall. Two mechanisms that look like they would avoid it do
not:

- **A marker record appended to the old object before the rename.** Only a
  handle that is following the tail can see it, because a handle's snapshot
  length is fixed and a record past that length is outside it. A handle that
  never calls `follow()` never sees the marker.
- **A flag or magic word in the file header.** No read a live handle performs
  goes near it. Of the eight sites that read the header, five are the opens
  themselves; the other three are a generation check on the `replace_*` paths
  and the two disk-index sidecar windows in the default API, and all
  three re-read the file rather than consulting a handle. So a flag there would
  inform only a *new* open, which already resolves the pathname to the current
  generation anyway.

Both cover exactly the handles that are already in a polling loop, and neither
covers the rest. What each shape of reader actually needs:

| Reader | Calls `follow()` | How it learns |
| --- | --- | --- |
| Streaming loop | yes | `follow()` returning `0` does not say which of its reasons applies — "nothing appended", "appended but not yet committed", or "this generation is finished"; `is_current()` separates the last from the other two, on a path that is idle by definition |
| Opens, reads, closes | no | **Not affected.** Every open resolves the pathname to the current generation |
| Long-lived, read-only, no loop | no | **Nothing informs it.** The application decides when freshness matters and calls `is_current()` — a timer, a user action, a filesystem watch |
| Shared `Arc<StratavaFile>` | owner only | The owner polls and swaps in `reopen_readonly()`; readers hold `&self` throughout |

**Why stratava does not check on every read.** It could, and the check is one
`open` plus two metadata calls — per read, on a path whose cost is the reason
positional reads exist. That collides with the standing requirement that reads
stay cheap and take `&self`, and it would charge every format for a capability
most of them never use. The check is offered, not imposed.

**What staleness costs.** Two things, and neither is corruption. The data is old
but whole: the superseded generation is complete and self-consistent, and every
offset the handle holds still means what it meant. And the object stays on disk
— an unlinked file is freed when the last descriptor closes, so a forgotten
read-only handle holds a whole generation's bytes for as long as it lives. A
compaction that halves a file frees nothing until the readers of the old one let
go.

**Who it affects.** Any long-lived reader that must not serve stale results, and
any deployment where a compaction's disk saving is expected promptly.

**Workaround.** `is_current()` before the reads that matter, and
`reopen_readonly()` when it answers `false`. Drop handles that are no longer
being read, rather than keeping them for a possible later question.

**Planned.** No push notification is planned, for the reason above: every
candidate mechanism reaches only readers that are already asking.

### 4.3 Writer-lock edge cases

- `WriterLock::drop` discards the error from `clear_writer_lock_info` because
  `Drop` cannot report (open item 20). A caller that never runs a self-test gets
  no signal that lock-info cleanup failed. Closing this needs an explicit
  `release()`/`close()` — an API addition, not planned.
- On Windows, writer-lock **marker** removal has no directory confinement (open
  item 5). The marker's identity is captured by opening the same unverified
  pathname, so the check is self-referential: it closes the
  capture-to-unlink window but proves nothing about ownership, and a marker
  swapped in before the capture is still deleted. Unix has exclusive-directory
  confinement; Windows has no equivalent. This never affected authoritative
  single-writer exclusion, which is a native lock on the target file object.
- `clear_stale_writer_lock` recovers a stale lock in `O(1)` without opening or
  scanning native data.

---

## 5. Features that are designed but not implemented

### 5.1 Channel-selective access

**What it is.** An internal design study — an option model for reading a subset
of channels without paying full block I/O, with its case analysis and its layout
variants — exists as working notes. It proposes a set of independently selectable
options, each defaulting to off, in the same shape as the existing `index: [...]`
DSL keys.

**It is a design study, not a feature.** There is no `channel_view` type, no
DSL key, and no generated method anywhere in `stratava-core` or `stratava-macros`.
Nothing that study describes is callable, and the notes are not published.

One conclusion from it is worth carrying here because it is a property of the
current on-disk layout rather than of the unimplemented feature: **an interleaved
payload cannot be read channel-selectively below full block I/O.** If your data
is interleaved, no future option will make a single-channel read cheaper than a
whole-block read without re-emitting the payload.

**Who it affects.** Anyone who saw the design notes and planned around them.

**Workaround.** Read whole blocks, or lay the data out planar yourself in
separate blocks.

**Planned.** Designed; not scheduled.

### 5.2 There is no way to discard unverifiable matrix visibility

**What it is.** The F-04 refusal shipped without its named companion operation
(open item 17). `rebuild_matrix_commit_from_crc` now returns
`Err(Error::MatrixFatalCorruption)` rather than publishing a commit view built on
evidence it could not read — correctly — but an operator who genuinely wants to
discard unverifiable visibility and start from a known state has no supported way
to do it.

The refusal also reuses `Error::MatrixFatalCorruption` (open item 16), and its
message recommends `with_matrix_fatal_forensics` — which is useless advice to a
caller that has already enabled forensics and is being refused anyway.

**Who it affects.** Operators recovering a damaged matrix.

**Workaround.** Repair or regenerate the validity evidence, or recreate the
matrix.

**Planned.** The discard operation is acknowledged as missing. Not scheduled.

### 5.3 A cleared category cannot be rebuilt until reopen

`clear_category` deliberately does not reset completeness, so a writer that
clears a category still cannot `rebuild_matrix_commit_from_crc` for that category
in the same session (open item 24). Fail-closed, but operator-visible. Reopen the
file.

---

## 6. Not verified

Almost everything in this section is a statement about **assurance** rather than a
known defect: it is here because a user is entitled to know which claims rest on
executed tests and which rest on inspection.

This section used to carry two exceptions — defects in the project's own gates
rather than in the library: a Linux Clippy failure and a default-feature test
failure. Both are fixed, and every gate is green. What replaced them is not a
shorter list of caveats but a sharper one: the suite now runs on Linux (§6.1),
and running it there found three defects that every Windows run had passed. The
distinction that matters throughout this section is **executed** versus
**measured** — Linux is now the first, and is still not the second.

### 6.1 The Unix code paths, and what the first Linux run found

This section used to say the Unix paths had never been executed. **That stopped
being true on 2026-07-27, and what the first run found is why the section is
still here.** The suite is executed on Linux as well as
Windows: CI runs `ubuntu-latest` and `windows-latest` on every push to `main`,
on every pull request, and on demand. The three paths this section used to name
as unexecuted — `diagnostics.rs`'s `openat`/`unlinkat` exclusive-directory
cleanup, `file.rs`'s `O_NOFOLLOW` lock-marker open, and the matrix
concurrent-read path without the Windows private-handle pool (§4.1) — all run
there now, `compression-zstd` included under `--all-features`.

The first Linux run found **three defects that every Windows run had passed**,
which is the reason to state the platform behind a claim rather than the claim
alone:

- a writer lock leaked across `fork`: a POSIX advisory lock belongs to the open
  file *description*, `Command::spawn` forks and duplicates it, and dropping the
  guard released nothing until the child execed. Dropping is not releasing, so
  the release is now explicit.
- identity-checked deletion compared recycled inode numbers. Identity bytes read
  from a handle that is then closed name nothing: ext4 hands a just-freed inode
  straight back to the next create, and the check said "same object" about a file
  it had never seen. A handle is now held open for as long as the identity is
  used, which pins the inode.
- an open-cost assertion hard-coded NTFS's ~128 KiB allocation-run granularity.
  ext4 reports extents precisely, so the same file measured 2 pages and 8,240
  bytes where Windows measured at least 4 and 64 KiB.

What this section still says, unchanged: **the numbers are Windows numbers.**
Executed on Linux is not measured on Linux. Every syscall count, residency
figure and open cost in these documents was taken on Windows x86_64, and the
third defect above is exactly what a different filesystem does to such a
constant. The only Linux numbers on record are two runs of the eager 1-vs-4
shared-handle ratio, 2.14x and 1.43x, published as the evidence that that ratio
is dominated by the host rather than as a result; no Linux throughput, syscall
count, residency figure or open cost has been measured.

`cargo clippy --target x86_64-unknown-linux-gnu -p stratava --no-default-features
--lib -- -D warnings` previously failed with `field 'handles' is never read` at
`crates/stratava-core/src/matrix.rs`, because `MatrixReadPool::reopen` is
`#[cfg(windows)]`-only and the field's other readers are `#[cfg(test)]`. The
field is an ownership anchor rather than dead weight — it holds the `Arc<File>`s
whose `Weak`s live in the thread-local cache — so it now carries
`#[cfg_attr(not(any(windows, test)), allow(dead_code))]` with that reason
recorded at the declaration. That was a cross-compiled lint pass when it was
written; Clippy now runs natively on Linux in CI, across nine feature
configurations, and is green.

### 6.2 Sparse offset evidence is filesystem-specific

`pib_probe::real_file_positional_io_at_one_pib` is `#[ignore]`d and additionally
soft-passes: `ProbeError::Unsupported` prints "skipped" and returns success unless
`STRATAVA_REQUIRE_PIB_SPARSE=1` is set. Its sibling
`real_file_positional_io_at_one_tib_smoke` is also
`#[ignore]`d and has no required-mode escape at all — `Unsupported` always passes.

Both probes were explicitly executed on Linux x86_64 on 2026-10-01. The 1 PiB
required-mode probe passed on `/tmp` tmpfs. The workspace overlay passed the
1 TiB probe, but refused a 1 PiB write with `File too large` (OS error 27).
Successful probes allocated 4,096 bytes, not their logical file lengths.
The `scripts/qualify_scalable.py` runner requires execution evidence
and rejects skipped probes even when Cargo itself returns success.

This establishes sparse addressability on those filesystems. It does not
establish petabyte throughput, storage endurance, power-loss recovery, or
support on every filesystem; deployment filesystems still need qualification.

### 6.3 Performance and scale probes do not run in any job

- `crates/stratava/tests/perf_smoke.rs` is entirely `#[ignore]`d. The performance
  smoke suite runs in no job.
- The one-million-key allocator stress probe
  (`crates/stratava/tests/high_cardinality.rs`) is `#[ignore]`d.

The explicit qualification campaign on 2026-10-01 executed 10k/100k/1m-key
profiles, with peak additional Rust heap of 6,289,194 / 14,962,766 / 16,170,598
bytes. It passed the 64 MiB peak and 16 MiB growth budgets. These are allocation
measurements, not RSS. The runner makes the campaign repeatable, but there is
still no automatically scheduled long performance campaign.

### 6.4 The fuzz, Miri and ASan runs are a weekly smoke, not a campaign

- `ci.yml`'s `fuzz-supply-chain` job is not fuzzing despite its name: it does
  `cargo audit`, `cargo deny`, `cargo metadata`/`check` and a lockfile drift
  check. The fuzzing is in `sanitizers.yml`, below.
- The most recent recorded fuzz evidence for this project is dated
  2026-07-11 (four core targets) and 2026-07-18 (three sidecar targets). Both
  predate several rounds of change to `matrix.rs`, `file.rs`, `codec.rs` and
  `format.rs`. The cited ASan run covered 21 library tests; the library suite is
  now 139.
- **This changed in 0.7.0, and the change is smaller than it sounds.** All three
  now run, weekly, in `.github/workflows/sanitizers.yml`: `fuzz-smoke` builds and
  runs **every** target for 90 seconds each, `asan` runs the `stratava` and
  `stratava-core` test suites under AddressSanitizer with `--all-features`, and
  `miri` runs `stratava-core`'s no-default-feature unit tests. Executed on Linux
  before the workflow landed: Miri 28 tests green under strict provenance and
  symbolic alignment checking, ASan 163 tests green with zero reports, and a
  bounded campaign on two targets producing no artifact.

  Ninety seconds per target is a smoke test, not a campaign. It proves the
  harness runs and catches what is shallow; it explores almost nothing. The
  disparity is measurable rather than theoretical: `codec_arbitrary` executes
  about 300,000 inputs per second, while the historical redb-backed
  `sidecar_state_machine` -- which built a sidecar per input and `fsync`ed it --
  managed 64, so the same 90 seconds
  buys 27 million executions on one target and under 6,000 on another. **No long
  fuzz campaign has been run against this code**, and the modules that most need
  one are the slowest to fuzz.

  The 2026-10-02 native-index replacement has newer bounded ASan/libFuzzer
  evidence, including a shared redb differential oracle and a fixed checkpoint
  corruption regression. The subsequent 2026-10-02 campaign ran three ASan fuzz
  targets for five minutes each: 2,578 matrix-model, 151,403 matrix-corruption
  and 2,221 native/redb differential executions, without an oracle mismatch.
  This remains a bounded campaign, not a long production soak.
- A libFuzzer OOM reproducer from 2026-07-20 sat unpromoted in
  `fuzz/artifacts/codec_arbitrary/` until 0.7.0, blocking
  `scripts/run-security-fuzz.ps1` (exit 2) the whole time. It is now a test --
  `codec_hardening.rs::the_2026_07_20_oom_reproducer_stays_refused`, with the
  eleven bytes inlined so deleting the artifact did not delete the regression --
  and the artifact is gone. Replayed through the real fuzz target on Linux before
  removal: executed in 0 ms, no OOM.

  **It has now been triaged, and it does not reproduce against this release's
  code.** The 11 bytes select `decode_from_slice::<HashMap<(), ()>>` (selector
  105, `105 % 12 = 9`) with a declared entry count of 587,203,068. Replayed
  through that public entry point today, every outcome is a typed error returned
  in microseconds:

  | Declared count | Outcome | Time |
  | --- | --- | --- |
  | 587,203,068 (the reproducer) | `LimitExceeded { resource: "HashMap entries", actual: 1073741888, limit: 1073741824 }` | 7.5 µs |
  | 293,601,534 (largest the 1 GiB ceiling admits) | `InvalidCanonicalEncoding("duplicate HashMap key")` | 6.5 µs |
  | `u64::MAX` | `InvalidCanonicalEncoding("collection count cannot make bounded input progress")` | 0.1 µs |

  No admitted count can produce a large allocation on this path: the map reserves
  `min(count, MAP_PREALLOCATION_ENTRIES = 1024)` entries before a single entry has
  been proven decodable (SAFE-01), so the real allocation is bounded by 1024
  regardless of what the file claims. The remaining gap is **test hygiene, not an
  unbounded allocation**: the reproducer was never promoted to a deterministic
  regression test, so nothing in the suite pins the behaviour that fixed it.
  **This release still makes no fuzz-pass claim** — one triaged reproducer is not
  a campaign.

### 6.5 Tests that can pass without proving anything

Named here so that a green run is not over-read:

- Both `pib_probe` tests (§6.2).
- The Windows lock-marker reparse-point test skips loudly where the host does not
  grant `SeCreateSymbolicLinkPrivilege`; the hard-link branch does run (open item
  10).
- The convoy scaling assertions in `matrix_concurrent_reads.rs` and
  `matrix_lazy_residency.rs` are skipped, with an announcement, on a single-core
  host.

### 6.6 What was verified, and what "green" does not cover

Every gate in the table below is green as of 0.5.0. The heading used to read "and
the one gate that is red"; that gate — a default-feature-only test defect — is
fixed, and the paragraphs after the table state exactly what a full green run still
does not establish.

Re-run in full for **0.5.0** on Windows x86_64 MSVC, 2026-07-25. Everything below
was executed; numbers are from that run. The 0.4.0 column is kept where a count
moved, because a count that moves without explanation is how a suite quietly
shrinks.

| Gate | Result (0.5.0) | Was (0.4.0) |
| --- | --- | --- |
| `cargo fmt --all --check` | clean | clean |
| `cargo check --workspace --all-features --all-targets --locked` | clean | clean |
| Clippy `-D warnings`, 9 configurations (all-features, default, no-default-features, and each of the 6 optional features alone) | clean, 9/9 | clean, 9/9 |
| `cargo doc`/`rustdoc -D warnings`, 3 crates x {all-features, default} | clean, 6/6 | clean, 6/6 |
| `stratava-test-runner test --workspace --all-features` | **651 passed, 0 failed, 8 ignored** across 49 binaries; artifact cleanup verified empty | 648 passed |
| `stratava-test-runner test --workspace` (default features) | **418 passed, 0 failed, 5 ignored** across 48 binaries | 168 passed, **1 failed** |
| `cargo test -p stratava-core --all-features --lib`, five consecutive runs | 139 passed each time, 0 failed; no flake | same |
| `rename-fixture` and `public-api-fixture`, built and run | both OK | both OK |
| `cargo deny check`, root **and** fuzz workspace | `advisories ok, bans ok, licenses ok, sources ok` for both | clean |
| `cargo audit` (root) | clean, 79 crate dependencies scanned | clean |
| fuzz workspace `cargo metadata --locked` + `cargo check --locked --all-targets` | clean | clean |
| `cargo package --locked`, verification enabled | **verified archives for all three crates** (`stratava-core`, `stratava-macros`, `stratava`) | verified |
| Clean-tree copy (no `target/`, no VCS, no `mck-*`/`slipway-*`, no fuzz corpus), `cargo metadata` + `cargo check --workspace --all-features` | clean | clean |

**Two notes on this run, both about honesty rather than results.**

The default-feature count rose from 168 to 418 because the 0.4.0 figure was taken
from a run that aborted at the first failing binary; 418 is the whole default
suite. Nothing was added to the suite to produce that number.

`cargo package` was run in a clean copy of the tree rather than in the repository,
because the repository working tree was dirty (this release's own uncommitted
changes) and `cargo package` refuses a dirty tree without `--allow-dirty`. The
archives were verified from exactly the release content, but the run that produced
them was not a run in a committed repository. It should be repeated after the
release commit. `cargo package -p stratava` alone still cannot verify — `stratava-core`
is not on crates.io, so the facade's dependency cannot be resolved from the index
— and `cargo package --workspace`, which unpacks the sibling archives into a
temporary local registry first, is the form that works.

**The red one, and its fix.** In the run tabulated above, under the **default**
feature set,
`crates/stratava/tests/matrix_concurrent_reads.rs::threads_sharing_one_handle_read_every_cell_correctly`
failed with `Error::IntegrityFeatureDisabled`: the test looped over
`[IntegrityPolicy::None, IntegrityPolicy::Crc32]` unconditionally, and `Crc32` is
refused when the `integrity` feature is off. It was a **defect in the test, not in
the library** — no library behaviour was wrong, and the same test passed under
`--all-features`. The loop is now gated on `cfg!(feature = "integrity")`, so the
default run covers `None` only. Re-measured 2026-07-25: the binary passes 4/4
under default features and 4/4 under `--all-features`.

Both gates that were red are now green — this one and the Linux Clippy lint in
§6.1. **As of the 0.5.0 run the whole table above was re-executed**, so those two
are no longer individually-remeasured exceptions; the default-feature suite in
particular is now a full green run rather than an abort at the first failure. The
Linux result in §6.1 remains a cross-compiled lint rather than an executed Linux
test, and that has not changed. This is stated precisely because "all green" was
the shape of every previous over-claim in this project's history.

One thing the 0.5.0 run does **not** cover: it was executed against a working tree
that a separate, unrelated documentation reorganisation was being written into at
the same time. The numbers above are real and were taken from the release content,
but they are not a certification of a committed, quiescent tree. Re-run this table
after the release commit.

Also verified in this run, and quoted in §1 and §4: the 13 `matrix_lazy_residency`
tests (10 at 0.4.0; the three added for 0.5.0 pin `MatrixMetadataResidency::DEFAULT`,
the derived cache, and §1.3's still-occurring refusal), the 4
`matrix_concurrent_reads` tests, and
`matrix_integrity_scaling::measured_resident_bitmap_bytes_against_the_ceiling`.

### 6.7 The backward-compatibility table: what is now executed, and what still is not

**No test in the suite opens an artifact written by an older version.** The
repository stores none: there is no tracked `.stratava`, `.vks` or `.vki` file
anywhere and no fixtures directory, so every row of the compatibility table in
[API Changes](api-changes.md) is *derived from the current source*.

Three of those rows were checked by hand on 2026-07-25 against files produced by
genuinely older builds, and all three behaved as documented:

| Claim | How it was checked | Result |
| --- | --- | --- |
| A plain native append-log file written by **0.1.0** opens normally | a producer built against the real `stratava-core 0.1.0` source package wrote an 8-record fixed-block file; a 0.4.0 consumer opened it | **opens; all 8 records and every field byte-for-byte correct** |
| The same, written by **0.3.0** | producer built against the real `stratava 0.3.0` / `stratava-core 0.3.0` / `stratava-macros 0.3.0` source packages | **opens; all 8 records and every field correct** |
| A file pinned with a **computed schema hash** from 0.3.0 is refused | the same declaration compiled under 0.3.0 (hash `0xbaf33c482858b060`) and under 0.4.0 (hash `0x4bf7fa8d7d7e9e4f`) | **refused with `Error::SchemaHashMismatch`**, carrying the 0.4.0 value as the wanted hash and 0.3.0's as the stored one |

That is a **manual, out-of-tree check, not a regression test.** It was run once, on
one host, over one small format with two `u32` fields. It does not cover variable
blocks, compression, keyed collections, matrices or sidecars, and nothing prevents
a future change from breaking it silently — there is still no committed fixture and
no test.

The rows that remain unexecuted:

| Row | Evidence | What it does not cover |
| --- | --- | --- |
| Matrix `VMAT` v1/v2/v3/v4 refused | `matrix_integrity_scaling.rs` writes a current file and **patches its version field** | that a genuinely old matrix file's other regions also refuse rather than misparse |
| Pre-nonce matrix refused at the nonce region | **no test, no manual check** | the whole row |
| Matrix and disk-index sidecar version refusals | source inspection only | the whole rows |

The positive claim is also sound in code: `read_file_header`,
`file_header_extensions`, `write_record_header` and `encode_record_footer` are
byte-identical to v0.1.2 and the record-footer constants match. The correct summary
is: **the append-log compatibility claim now has one executed demonstration behind
it; the matrix and sidecar refusal claims still rest on inspection.**

### 6.8 Platform support, stated once

| Target | Status |
| --- | --- |
| Windows x86_64 MSVC | executed, and the platform every number in this document is from. |
| Unix (Linux) | **executed**, including `compression-zstd`, and green: tests, Clippy and fmt run natively on `ubuntu-latest` in CI (§6.1). The first such run found three defects every Windows run had passed. **No Linux throughput, syscall count, residency figure or open cost has been measured** — the numbers above are not Linux numbers, and the only Linux figures on record are the two eager scaling ratios in §6.1. |
| macOS / other targets | compile paths exist, nothing executed. Two behaviours differ by construction rather than by accident: no allocation map (§1.1) and no hole punch (§1.7). |

### 6.9 CI reproducibility is bounded

Actions and cargo tools are pinned. `ubuntu-latest`, `windows-latest` and
`stable` are rolling by design; `msrv (1.95.0)` is the only fixed-toolchain job.
The Clippy feature matrix is a fixed list — no-default-features, default, each
optional feature alone, all-features — so a defect requiring a specific *pair* or
*triple* of features is not covered.

---

## 6.10 The editable header region: what its fixed size actually forbids

`header_slots` reserves a run of bytes in the file header that a caller can
rewrite for the life of the file. Its size is fixed and folded into the schema
hash, and five consequences follow that are limitations rather than details:

- **The capacity cannot be changed for an existing file.** Reopening under a
  different capacity is a `SchemaHashMismatch`. There is no grow path and no
  migration for it; growing the region would move the append log, which means
  moving every record in the file — which is republishing it. Size the region
  when you declare the format, using `header_slots_used()` after one write of
  each block you intend to hold.
- **The ceiling on the capacity is a reader-side declaration, so two readers of
  the same file can disagree about it.** The region shares the file-header
  extension budget, `ReadLimits::max_file_header_extension_len` — 64 KiB when
  nothing declares it, raised with `limits { header_extension: .. }`. A capacity
  above the budget is refused at create. But `ReadLimits` are deliberately *not*
  folded into the schema hash, so a reader that declares the default ceiling and
  opens a file whose region is 256 KiB is refused at open with no schema
  mismatch to explain it. The refusal is correct — an unbounded reader would let
  a hostile `u32` length field name a 4 GiB allocation before a block is parsed
  — but it means the ceiling has to be declared in every copy of the format
  declaration, and nothing checks that it was. Measured in
  `crates/stratava/tests/header_extension_limit.rs`: the undeclared ceiling is
  still 64 KiB, an over-budget region without a declaration is refused at
  create, a declared ceiling carries a 256 KiB region through create, write,
  reopen and read, and a reader that declares the default ceiling is refused at
  open by the larger region with `Error::InvalidCompressionHeader` — before the
  schema-hash comparison, which is what makes "no schema mismatch to explain it"
  literal rather than figurative.
- **A format declaring matrix blocks cannot declare a region.** Refused at
  validation with `Error::InvalidFormatSpec`. The matrix creation nonce and the
  matrix layout header sit at fixed offsets *after* the file header, and a
  reserved region moves both. Lifting this is a matter of establishing that
  every consumer of those two offsets derives them from `header_len` rather than
  storing a constant; that walk has not been done, so this refuses rather than
  guesses — the same refusal, for the same reason, that `index: header_tails`
  carries.
- **An already-open handle sees the region as it was at its open, not live.**
  `read_header_block` decodes from the header bytes the handle read when it
  opened, which is exactly what lets it take `&self` and touch no I/O. A reader
  opened before a write keeps answering with the old value until it is reopened;
  a reader opened after it sees the new one. Measured, not inferred
  (`a_reader_opened_before_a_write_does_not_see_it`). This is the same snapshot
  model the rest of the read surface has, and it means the region is not a
  channel for pushing a changed value to live readers.
- **The region has no crash-atomicity guarantee across a write.** A write is a
  positional overwrite of the region followed by `sync_data`. A crash between
  the two leaves a region whose checksum does not match its bytes: under
  `integrity: rolling` and a sealed `sealed` region, reads report
  `Error::ChecksumMismatch` — which is the correct answer, but it is *detection*,
  not recovery. There is no second slot to fall back to, unlike the `VBTT`
  header-tail region, which carries two. A caller who needs the region to
  survive a crash mid-write with its previous value intact does not have that
  here.

What has been measured: the seventeen cases in
`crates/stratava/tests/header_slots.rs`, plus the four in
`crates/stratava/tests/header_extension_limit.rs` for the capacity ceiling, over all
three integrity policies —
reserve at create, survive a reopen, rewrite without growth, removal, refusal on
overflow, refusal on an undeclared block, refusal of a write after a seal,
verification on read under `rolling` and under a sealed `sealed`, non-verification
under `none` and under an unsealed `sealed`, the file length and record offsets
unchanged across an edit, and the region carried verbatim across a republishing
`replace`, and the reader-snapshot semantics above. What has **not** been
measured: the crash behaviour above — no fault injection covers this region, so
the statement that a torn write is *detected* rests on the checksum's
construction and on the corruption tests, not on an interrupted write.

## 7. Resource limits: what they do and do not bound

### 7.1 Limit accounting is nominal, not peak RSS

`ReadLimits` ceilings are charged against modelled sizes. They do not fully
account for container bucket and node overhead, the `8 * N` sequence-uniqueness
temporary, mmap index/map duplication, or auxiliary maps built during keyed
materialization. **A true peak-RSS ceiling belongs to an external process, cgroup
or job-object policy**, not to `ReadLimits`.

### 7.2 Some allocations abort instead of returning an error

Published deliberately. Content-sized allocations — those whose size a file can
choose — are charged to a `ReadLimits` ceiling and reserved fallibly, surfacing as
`Error::AllocationFailed` or `Error::LimitExceeded`. Shape-sized allocations —
those bounded by the program's own compile-time shape — use ordinary infallible
allocation and **abort the process** if the allocator refuses.

The consequence, written down rather than discovered: an allocator refusal *after*
a publication terminates the process rather than substituting a different outcome.
No caller ever observes a wrong outcome, and the on-disk state is the published
one.

`AllocatedExtents` is capped by a compile-time constant rather than by a
`ReadLimits` charge (open item 6) and grows with infallible `Vec::push`, so an
allocator refusal there aborts rather than returning a typed error.

### 7.3 `max_keyed_tail_bytes` is per block id

See §2.2. A format with `M` keyed block types can hold about `M *
max_keyed_tail_bytes`.

---

## 8. `unsafe` API contracts that cannot be checked

**`replace_fixed_in_place_exclusive` cannot verify that a key is unchanged**
(open item 15). At a `T: StratavaBlock` signature there is no way to check it. The
API does guarantee unconditionally that the block's keyed-tail map is dropped
before the first byte is written. **If the contract is violated, the physical
`prev_same_key_offset` chain is unrepairable** — this path publishes no new
generation in which the chain could be rebuilt.

**Who it affects.** Callers of the `unsafe` in-place replacement on a
`keyed_offset_chain` format.

**Workaround.** Use `replace_rewrite` or `replace_block`, which publish a new
generation.

---

## 9. Contributor-facing notes (not reachable by users)

These are recorded for completeness and because they are self-reported in the
source rather than hidden. **A downstream crate cannot express any of them**, so
they are not user-facing limitations and are listed separately for that reason.

Three enforcement bypasses still compile *inside* `stratava-core`. The catalogues are
the doc comments on `mod bypass_catalogue` in `crates/stratava-core/src/matrix.rs`
("What still compiles from inside this file") and in
`crates/stratava-core/src/file.rs`, and the one on
`a_static_decoy_flag_cannot_be_declared` in
`crates/stratava-core/src/writer_permit.rs`.

1. **`SparseBitmap` and its prepared values are declared at file scope**
   in `matrix.rs`. `BitmapByteUpdate`, `CommitBitUpdate`, `PreparedByteWrite`
   and `PreparedWriteBit` are constructible by literal from anywhere in
   `matrix.rs`; a fabricated `PreparedByteWrite { fresh: None, .. }` reaches
   `commit_byte_write` for a non-resident page. Open item 26. The fix is a
   `mod sparse_bitmap`, which is a large refactor of the hot bitmap path.
2. **Commit maps are not behind an evidence type.** `MatrixCommitLayout::bits`
   is a plain `SparseBitmap` field of a file-scope struct, so
   `layout.commits[i].bits = attacker_bits;` compiles from anywhere in
   `matrix.rs` and publishes a commit view nothing committed. Open items 25 and
   28. This is the one with teeth: it is the same class as F-04 — a fact a reader
   consumes as proof — with nothing between the fact and the assignment.
3. **`Box::leak(Box::new(PoisonFlag::healthy()))` still yields a
   `&'static PoisonFlag`** that a `GuardedWriter::poison_flag` implementation
   could return in place of the writer's own field, permitting every guarded
   operation on a poisoned writer. The `static DECOY` spelling was removed;
   the heap spelling is caught only by a source gate over the four `poison_flag`
   implementations. Open items 21 and 27.

**Why these are unreachable from outside the crate**, verified: `SparseBitmap` and
`MatrixCommitLayout` are private (`struct`, no `pub`). `MatrixLayout` is
declared `pub struct` in `matrix.rs` but is **not** in `lib.rs`'s
`pub use matrix::{...}` list, so it has no external path. `PoisonFlag` is `pub`
inside the private `mod writer_permit` and escapes only via the `#[doc(hidden)]`,
`scalable-fault-injection`-gated `enforcement_probe` — whose own rustdoc says
"This is not API" — where `PoisonFlag::issue` is private and `GuardedWriter` is
not exported at all.

Two further catalogued items — `CrcValidEvidence::newly_created` and a
`MatrixBlockLayout` literal — launder in the **fail-closed** direction. They
cannot make a cell look verified, and are recorded for completeness rather than as
live risk.

Other contributor-facing items, each confirmed present in code:

- `MutationPermit` binds the writer's *type*, not its instance (open item 21). A
  permit held across an intervening poisoning call would still be accepted. Every
  cross-module entry point consumes it by value and re-takes it before each
  disk-touching step, so no live code holds a stale one — but that is a
  convention, not a type.
- `ResidentIndex::adopt_generation` is unchecked (open item 29). The cheap fix
  does not work: an extent-check replacement still admits
  `adopt_generation(vec![RecordIndexEntry { committed: true, .. }])` against a
  zero-length file, and a constructor that would refuse it adds `O(records)` I/O
  to every open that does not already scan.
- Three mirrors are safe by inspection rather than by type (open item 23):
  `disk_index.rs::store_tail_cache`, `stream.rs::set_tail`, and the matrix
  quarantine map. All are installed infallibly today; nothing stops a fallible
  step being added after their disk write.
- `matrix.rs`'s prepared page-index values have no `tests/ui` proof (open item
  22), deliberately: exposing them to prove them would destroy the module privacy
  that *is* the enforcement.
- `SparseBitmap: Clone` still exists (open item 19). The hand-written
  `impl Clone for SparseBitmap` is retained because **`MatrixCommitLayout`
  derives `Clone`** and holds a `SparseBitmap` field. It is *not* retained for
  `MatrixLayout`, which was deliberately stripped of its `Clone` derive in round
  16 (a clonable layout made `FatalAccessGate` clonable, and a clonable gate is an
  installable gate); the comment above `struct MatrixLayout` records that. The
  item is live, the reason previously given for it was stale.
- `diagnostics.rs::classify_error` is an exhaustive match over `Error` (open item
  11), so every new error variant forces an edit to a file the fixing round does
  not otherwise own. Kept on purpose as a forcing function.
- `format.rs`'s manifest region has never been walked by any review round (open
  item 12).
- The `#[cfg(test)]` sidecar publication interposition hook in `stream.rs` is
  process-global state keyed by canonical primary path (open item 4). It is
  compiled out of every non-test build. A test that arms it and then fails before
  the publication point leaves its entry armed for the rest of that binary's run;
  this is now visible — the next registration for the same path asserts — rather
  than silently changing another test's behaviour.
