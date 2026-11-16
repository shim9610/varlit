# Known limitations

This describes Varlit 0.13.0. Choose the storage path and budgets for the actual
workload; [API reference](api-reference.md) gives the supported options.

<a id="1-matrix-opening-a-matrix-is-not-o1-because-opening-it-verifies-it"></a>
<a id="11-open-cost-is-proportional-to-candidate-pages-not-to-the-working-set"></a>
<a id="12-bitmap-directories-read-original-metadata"></a>
<a id="13-bitmap-limits-control-the-working-set"></a>
<a id="14-lazy-is-the-default-verification-is-what-still-happens-at-open"></a>
<a id="15-default-matrix-admission-is-separate-from-memory-budgets"></a>
<a id="16-the-matrix-residency-and-verification-policies-have-no-varlit_format-dsl-key"></a>
<a id="17-clearing-a-category-costs-thetacells--8-unless-the-platform-can-punch-holes"></a>
<a id="18-matrix-dimensions-are-fixed-at-create-unless-one-is-declared-growing"></a>

## Matrix I/O and storage

A matrix is stored as tiles inside the primary file. A tile is an internal
append-log record written the first time any of its cells is written; cells are
then overwritten in place. Each publication goes through a write-ahead ring in
the same file and syncs the file once (twice when it appended tiles) before it
is visible; a publication that needs ring space also runs a checkpoint, which
syncs twice more. A reader that falls further behind than the ring holds gets
`MatrixSnapshotExpired` and must `follow()`. `verify_matrix_metadata()` visits the tiles that exist; its cost is
proportional to written tiles, not to the cells a reader intends to access.

`ReadLimits` has no matrix residency, verification or cache-size settings; they
were removed in 0.13.0. Budget accounting covers nominal page/cache payload, not allocator
overhead or total process memory. Some caller output operations still require
materialization.

Overwrites do not grow a tiled matrix, and tiles, once appended, are never
removed; there is no matrix compaction. A single publication larger than half
the write-ahead ring streams through overflow records and an on-disk index:
the file grows by about the largest such publication: its staged bytes,
each restage of a unit (a tile's commit bitmap after every cell write)
included, and about 24 to 48 bytes of index per staged unit. Those records are reused by later large publications, a
reopened writer included, and their free space takes new tiles and directory
pages; nothing returns them to the filesystem (`compact_matrix()` was
removed in 0.13.0). Starting such a publication first puts every published
generation in place, so a reader more than one generation behind gets
`MatrixSnapshotExpired` and must `follow()`. A tiled matrix does not rely on
sparse files, allocation maps or hole punching.

Dimensions are fixed at creation unless explicitly declared growing. Generic
channel-selective reads of interleaved payloads are not provided; use planar
blocks or read the containing block. A tile whose commit bitmap fails its
checksum makes reads and writes of that tile's cells fail with
`InvalidMatrixLayout` until `clear_matrix_category` resets the category; the
same writer can then call `rebuild_matrix_commit_from_crc` without reopening.
That rebuild commits every written cell whose slot checksum holds, including
cells that were never committed or were cleared, because a slot checksum shows
that a cell was written, not that it was committed. Directory pages, commit
flags and aux bytes carry no checksum. See [recovery](recovery-model.md).

<a id="2-native-apis-bounded-memory-does-not-remove-scan-and-scratch-io"></a>
<a id="21-native-open-can-still-scan-the-input"></a>
<a id="22-a-keyed-writers-construction-cost-scales-with-block-types-times-index-size"></a>
<a id="23-external-merge-needs-scratch-disk-and-per-record-workspace"></a>
<a id="24-under-a-transaction-marker-commit-policy-reopening-for-write-discards-the-uncommitted-tail"></a>
<a id="12-what-residency-still-costs-the-page-index-mirror-and-a-per-bitmap-cache-bound"></a>
<a id="13-max_matrix_bitmap_bytes-bounds-a-cache-and-the-residual-admission-limit-is-the-index-mirror"></a>
<a id="15-the-default-limits-cap-a-matrix-at-16000000-cells--create-fails-above-it"></a>
<a id="21-varlitfile-scans-the-whole-file-at-open-and-holds-a-record-index"></a>
<a id="23-keyed-merge-and-compact-are-resident-only-and-explicitly-not-petabyte-scale"></a>
<a id="3-streamindexed-api-status"></a>
<a id="3-stream-api-status"></a>

## Keyed storage

- **Opt-in for now.** Formats keep appending keyed blocks to the log unless
  they declare `keyed_storage: in_place`. Keyed history
  (`keyed_offset_chain`, `keyed_chain`, tombstones) is removed in one later
  change with a converter; until then both exist
  ([design](design/bounded-keyed-storage.md#implementation-order)).
- **No listing yet.** A keyed-storage block is read by key; there is no
  iteration over its keys, and `key_order = increasing` (sorted runs) is not
  implemented.
- **Not with matrices.** A format cannot declare both keyed storage and
  matrix blocks yet: each keeps its metadata as the first record.
- **Constant slack.** Beyond the live data a file holds whole-region slack
  (1 MiB regions), the ring (16 MiB by default) and the values the latest
  publications replaced until a checkpoint frees their space: up to one
  publication's worth, so a publication that rewrites every value needs
  about twice the live data. These do not grow with updates, but they
  dominate a small file.
- **Two barriers per publication with updates.** New values written straight
  to the file are made durable before the commit frame, so a `sync` that
  wrote any costs two barriers instead of one, and every few publications a
  checkpoint adds two more.
- **Fragmentation at the extremes.** With values of every size and more
  holes than the writer's hole cache holds, holes that fit are not always
  found and the idle space can grow past the bound (a test reproduces it
  with a cache of four holes and is ignored). Complete, cache-independent
  knowledge of free space is being designed
  ([design](design/bounded-keyed-storage.md#bounds-at-the-extremes)).
- **The hole cache is in memory.** A reopened writer starts with an empty
  cache; holes it does not know are reclaimed by compaction when they pass
  the ratio, not reused at once.

## Native scans and temporary storage

Native reader/writer open may scan every record. An ordinary private directory
row uses 13 bytes; finite buffers spill excess metadata. Compact rewrite rows,
open digests and incremental follow reduce particular costs but do not make
every native open or mark constant time.

Native generated keyed writers may prime one tail map per keyed block, costing
`Theta(block types * records)` at construction. Tail maps spill at their nominal
budget, with a 2 MiB in-memory batch per block. `max_keyed_tail_bytes` is per
block, not a global sum. Stream handles avoid those construction scans.
Explicit requests for an owned full key map can still exceed admission limits.

There is no disk-backed lookup by key. The indexed primary (`.vki` sidecar,
`key_index`) was removed; keyed lookup is resident only, and every keyed update
or delete appends a record, so the file grows with updates until an explicit
keyed compaction. Stream readers have no lookup by key. The replacement is
described in [bounded keyed storage](design/bounded-keyed-storage.md) and is
not implemented.

External merge and rewrites require scratch disk, a complete rewrite generation
and per-record/key workspace. `VARLIT_SPILL_DIR` selects disposable spill storage;
`VARLIT_SPILL_MAX_BYTES` limits each spill file, not aggregate storage; unset,
each file is capped at 4 GiB. Atomic replacements stay beside their destination. Filesystem-full
and short-write errors propagate. Scratch on tmpfs consumes memory beyond process
RSS accounting. See [spill policy](scalable-io.md#private-spill-policy).

Under transaction-marker commit, reopening a writer discards its uncommitted
tail. Save required pending work before closing or recovering that writer.

<a id="4-concurrency-what-is-self-and-what-a-reader-sees-of-a-writer"></a>
<a id="41-what-takes-self-today"></a>
<a id="42-where-a-reader-gets-a-snapshot-rather-than-live-state"></a>
<a id="42b-nothing-tells-a-reader-that-its-generation-was-replaced"></a>
<a id="43-writer-lock-edge-cases"></a>

## Concurrency and publication

One writer owns file mutations. Readers are `Send + !Sync`; each thread owns a
reader and creates its cursors there. Readers hold a confirmed generation until
explicit `follow()`. Failed adoption preserves the old view; unstable publication
can return `WouldBlock` for a retry. The writer does not wait for reader retries.
Large missed changes may exceed staging limits and require reopening or a caller
budget decision. A matrix reader keeps its snapshot only while the write-ahead
ring holds the publications after it; once a checkpoint writes a newer
generation in place, its reads return `MatrixSnapshotExpired` until it
follows.

A primary replaced at its pathname leaves existing readers on the old file
object. Poll `is_current()` and reopen when that replacement matters; `follow()`
on an old object cannot discover a different object at the path. Matrix-only
follow can return zero appended bytes; compare `matrix_generation()` when needed.
`follow()` stops before a record the writer has only half written and reads it
on a later call. Opening a file is different: under a format without a commit
marker, `open_readonly` that races a writer mid-record reports `CorruptTail`:
the open scan treats an incomplete last record as damage and does not check
whether a writer is still active. Open readers before heavy writing, retry the
open, or use a commit-marker policy.
`flush()` alone does not publish a matrix generation; `sync()` or explicit
Immediate does. See [durability](durability-model.md) for each storage policy.

Stream sidecar compaction retains old files until old owners close. On Windows,
companion replacement requires filesystem support for the native POSIX
rename semantics used by the implementation. Failure returns an OS error and
poisons the writer; reconcile and reopen. Lock files coordinate cooperating
writers. Unsupported native locking returns an error; advisory locks do not
prevent arbitrary external file changes.

## Files and I/O progress

Path-based opens accept only regular files. A FIFO, socket, device or
directory is refused with an `Io` error of kind `InvalidInput`, and on Unix the
refusal does not wait: the data file, its lock marker and its sidecars are
opened with `O_NONBLOCK`, which returns at once for a FIFO with no peer and is
ignored for regular files, and the file type is checked on the open handle,
not on the path. Opening a FIFO for writing with no reader can instead fail
with `ENXIO`. A symbolic link to a regular file is followed for the data file,
as before.

Once a regular file is open, every read, write, `fsync` and lock is an ordinary
blocking system call. Varlit assumes each call eventually returns, and does not
time it out. A hung network mount, a stalled FUSE server or a failing disk can
block a call indefinitely; nothing in the library ends that wait. `Interrupted`
is retried without limit, so a signal storm that interrupts every call keeps
the operation retrying.

Cancellation is cooperative. `ScanCancellationToken` is checked between
records and at progress callbacks during scans, bootstrap and rebuild; it is
not observed inside a system call, and other operations have no cancellation
point. A caller that needs a deadline runs the operation on a thread or in a
process it can abandon or terminate, and treats the file as it would after a
crash: a writer's next open recovers to the last publication.

<a id="7-resource-limits-what-they-do-and-do-not-bound"></a>
<a id="71-limit-accounting-is-nominal-not-peak-rss"></a>
<a id="72-some-allocations-abort-instead-of-returning-an-error"></a>
<a id="73-max_keyed_tail_bytes-is-per-block-id"></a>

## Resource and allocation limits

Limits account for named payloads, cache entries and traversal work. They do not
reserve physical memory or bound total peak RSS, OS page cache, caller-retained
results or arbitrary user codecs/callbacks. Directory/page bookkeeping and one
encoded record/key can add workspace beyond a nominal cache payload allowance.
Header slots have fixed reserved capacity; overflow needs a different file layout
or a generation rewrite, not an automatic header extension.

A native handle holds fixed read buffers from open (about 144 KiB for a format
with variable blocks and no compression). `MmapMatrix` copies a cell whose slot an anchor splits on
its first read and keeps the copy for the mapping's lifetime: at most one slot
per anchor, and only for split cells that are read.

Fallible buffer/owner APIs report supported allocation refusals and clean partial
output. Existing public `Clone`, infallible constructors and convenience builders
retain their contracts. `BTreeMap`, formatting/report construction, some extent
collection and dependency/application internals remain outside the audited
fallible paths. OS OOM kills and allocators that terminate instead of returning
null are outside that contract. A `Result` cannot guarantee termination or
non-panicking behavior of arbitrary callbacks.

An allocation error after a write can describe completed or uncertain
publication. Inspect `AllocationFailureContext`, poison state and the operation's
result contract before retrying. See [error interpretation](api-reference.md#common-error-interpretation).

<a id="5-features-that-are-designed-but-not-implemented"></a>
<a id="51-channel-selective-access"></a>
<a id="52-there-is-no-way-to-discard-unverifiable-matrix-visibility"></a>
<a id="53-a-cleared-category-cannot-be-rebuilt-until-reopen"></a>
<a id="8-unsafe-api-contracts-that-cannot-be-checked"></a>
<a id="9-contributor-facing-notes-not-reachable-by-users"></a>

## Unsafe operations and experimental features

Mapped backing bytes must remain stable for the lifetime of views and borrowed
references. External truncation can cause an OS fault instead of a Rust error.
CRC verification cannot prevent later external writes. Unsafe typed views also
require valid layout, alignment and values. In-place exclusive replacement
requires unchanged key identity; prefer generation replacement when exclusivity
or key preservation cannot be established. See [SECURITY.md](../SECURITY.md).

`experimental-matrix-reuse` requires explicit feature and runtime opt-in and uses
its own format/compatibility rules. It does not make every matrix operation
interchangeable with the native tiled matrix format.

<a id="6-not-verified"></a>
<a id="61-the-unix-code-paths-and-what-the-first-linux-run-found"></a>
<a id="62-sparse-offset-evidence-is-filesystem-specific"></a>
<a id="63-performance-and-scale-probes-do-not-run-in-any-job"></a>
<a id="64-the-fuzz-miri-and-asan-runs-are-a-bounded-smoke-not-a-campaign"></a>
<a id="65-tests-that-can-pass-without-proving-anything"></a>
<a id="66-what-was-verified-and-what-green-does-not-cover"></a>
<a id="67-the-backward-compatibility-table-what-is-now-executed-and-what-still-is-not"></a>
<a id="68-platform-support-stated-once"></a>
<a id="69-ci-reproducibility-is-bounded"></a>
<a id="610-the-editable-header-region-what-its-fixed-size-actually-forbids"></a>

## Verification scope

Native CI runs on Linux, Windows and Intel macOS. Other targets, ARM64 macOS
and every mounted filesystem are not covered by those jobs.

32-bit and big-endian targets are checked in CI but not on hardware, and
support for them is unstable:

- **32-bit.** CI builds the workspace for `i686-unknown-linux-gnu` and runs
  the library and integration tests as 32-bit binaries on an x86_64 runner.
  `varlit-core` converts a file-sized `u64` to `usize` only through checked
  helpers, so a length that does not fit a 32-bit address space is refused
  with `LengthOverflow` instead of being cut short. Offsets stay `u64`, so
  files beyond 4 GiB can be read record by record; a single value, buffer or
  mapping cannot exceed the address space, and nothing here was run on a
  32-bit device or on a 32-bit architecture other than x86.
- **Big-endian.** No CI job executes big-endian code natively. Miri interprets
  the unsafe modules and the byte-order-sensitive unit tests (wire layouts,
  row packers, snapshot codecs) for `s390x-unknown-linux-gnu`. File formats
  use explicit byte orders; the one place that depends on the host's is the
  bulk numeric vector decode, which swaps bytes when the two differ. The
  matrix tile tests, the integration tests and every feature-gated path are
  not interpreted for a big-endian target.
- **Raw views on another byte order.** A zero-copy raw view checks the
  implementer's `RAW_ENDIAN` against the file's byte order, not against the
  host's. A struct of plain integer fields declared `RAW_ENDIAN = Little`
  passes that check on a big-endian host and reads swapped values; such a
  type is only correct where the host's byte order is the declared one.
  Use explicit-endian field types (for example `zerocopy::byteorder`) for
  views that must work on both. Single-feature,
pairwise and all-feature runs do not enumerate every higher-order configuration.
Runner images and some compiler jobs roll over time.

Kani establishes registered contracts under their stated bounds and assumptions,
not whole-library OOM freedom or every crash schedule. Loom covers selected
cancellation/progress behavior and the tile reader lock, not the shared-owner implementation. Miri
interprets only the modules containing `unsafe` code it can execute; the rest
of that code has native/ASan coverage and explicit gaps. Fuzz smoke runs are
finite campaigns. Process kills do not simulate loss of the OS page cache.
See the [verification guide](verification.md), live CI artifacts and manifests.

Performance qualification is pending a controlled local rerun. The current tree
retains the [benchmark harness](../tools/storage-bench/README.md), without cloud
results or performance conclusions. Historical material remains in Git history.
