# Varlit Implementation Spec

This specification describes [Varlit 0.13.0](api-changes.md#0130). Dated sections
and versioned wire markers retain their stated compatibility scope.

## Goal

Define typed binary block formats with macros and provide append-friendly files,
lazy typed access, keyed merge semantics, explicit durability, recovery policy
and compile-time schema validation.

## Workspace

- `varlit`: facade crate re-exporting runtime and macros.
- `varlit-core`: runtime format spec, file reader/writer, codec, collections, merge, and errors.
- `varlit-macros`: `#[derive(VarlitBlock)]` and `varlit_format!`.

## Wire Contract

- Successful writer output and compatible reader expectations are specified in
  [the generated-file contract](verification.md#successfully-generated-files),
  including publication, codec, reader-limit and companion-file conditions.
- `ReadValidation` is runtime read policy and is not stored or included in a
  schema fingerprint. It selects optional data CRC and additional structure
  checks; bounds, resource/decoder limits and publication guards remain required.
  Writer checksums and explicit verification/recovery contracts are unchanged.
- Fallible ownership and caller-buffer chunk range APIs add no wire fields.
  Native 13-byte directory rows, compact rewrite rows and cached nonresident
  offsets are private scratch representations, not persistent format revisions.
- `LayoutSpec` is the physical layout layer. If no layout is declared,
  `LayoutPreset::VarlitNative` is used and the existing Varlit header/record
  bytes remain unchanged.
- `FormatSpec::effective_layout()` exposes the active physical plan with the
  same file-header, segment, lead-in, raw-region, and footer vocabulary for both
  native and custom formats. For `LayoutPreset::VarlitNative`, the plan is
  synthetic and mirrors the existing native writer/reader byte contract without
  changing native bytes or schema hashes.
- The Varlit-native file header, record lead-in, and optional record footer are
  generated from the same internal native layout field contract used by
  `effective_layout()`. The native append-log semantics remain in the native
  reader/writer; this is a parity extraction, not a semantic rewrite.
- `FormatSpec::inspect_layout_file(path)` validates an existing native or
  custom file and returns `LayoutFileInfo` with the effective plan, file-header
  length, and physical segment ranges. Native files are projected from strict
  `RecordIndexEntry` scans into the same `LayoutSegmentInfo` shape used by
  custom segment layouts.
- `LayoutPreset::None` lets a declared layout own byte zero. This is the first
  custom physical-layout path and is intended for append segment formats whose
  lead-in is not `VARVE1/2/3`, such as TDMS-style `TDSm` segments.
- Non-native layout descriptors can declare a file header, segment lead-in
  fields, metadata regions, raw regions, footer descriptors, field constants,
  caller-supplied fields, and finalized/backpatched offset fields.
- The first runtime slice supports one optional file header followed by repeated
  segments with lead-in + caller metadata bytes + contiguous raw bytes +
  optional footer. The writer backpatches finalized offsets after the segment is
  written, and the reader validates literals, finalized offsets, forward
  progress, footer bounds, and region bounds.
- File header includes user magic, Varlit container marker, format version, endian, flags byte, and schema hash.
- Record header includes block id, block version, flags, sequence, payload length, checksum, and reserved bytes.
- The final record header word is `uncompressed_len_hint` for a compressed record of at most 64 KiB and `0` otherwise (including every chunked record, see [Chunked Records](#chunked-records)). `payload length` is always the physical stored byte length.
- `VARVE1` is used for plain records, `VARVE2` is used for file-header extensions such as file-explicit compression, and `VARVE3` is used when `record_footer`/`transaction_marker` commit policy or block/keyed offset chains are enabled.
- A `VARVE3` record has a fixed 32-byte footer after the stored payload: `b"VRF1"`, footer version, footer flags, `prev_same_block_offset`, `prev_same_key_offset`, reserved `footer_crc32`, and reserved bytes. Offset fields are `0` when absent.
- User block ids are explicit `u32` values below `0xFFFF_FF00`; higher ids are reserved for internal records, all carrying `RECORD_FLAG_INTERNAL` (`0x8000`):

  | Id | Record |
  | --- | --- |
  | `0xFFFF_FFFE` | tombstone (keyed delete) |
  | `0xFFFF_FFFD` | op (keyed operation for merge) |
  | `0xFFFF_FFFC` | application metadata entry (`write_metadata(key, value)`) |
  | `0xFFFF_FFFB` | index checkpoint |
  | `0xFFFF_FFFA` | embedded manifest |
  | `0xFFFF_FFF9` | commit marker (`VCMT` or `VCM2`, see [Commit markers](#commit-markers)) |
  | `0xFFFF_FFF8` | stream creation nonce (see [Stream records](#stream-records)) |
  | `0xFFFF_FFF7` | internal segment |
  | `0xFFFF_FFF6` | reserved: the removed whole-chunk matrix record; refused |
  | `0xFFFF_FFF5` | open digest |
  | `0xFFFF_FFF4` | sync marker `VSYN` (see [Stream records](#stream-records)) |
  | `0xFFFF_FFF3` | matrix and keyed-storage tile/ring/page records (in place) |
- Fixed blocks use canonical field encoding, not raw Rust memory layout.
- Variable blocks encode fields as `field_id + wire_type + length + payload`, allowing unknown fields to be skipped.
- Variable user blocks may be compressed after canonical field encoding and before record write. A format may set one global variable-block compression policy, or opt individual variable block ids into record-explicit compression with `BlockCompressionDescriptor`. Fixed blocks and internal records are not compressed.
- `ChunkedBytes` is a value-level helper for caller-managed blobs. With
  `compression-zstd` and `integrity`, it stores zstd-compressed chunks with
  per-chunk CRC32 validation and can be used inside variable fields or aux
  payloads.
- The generated `Format::read_payload(&entry, path)`, `scan()`, and mmap payload windows expose physical stored bytes. Typed reads, migrations, and merge/materialization use logical payloads and transparently decompress when needed.
- Endian priority is block override, then format setting, then little-endian default in macros.

### File-header flags and anchors

- The native file header's flags byte is zero, or an anchor policy: bit 7
  (`0x80`) *anchored*, bit 6 (`0x40`) a 256-byte instead of a 4 KiB anchor, and
  bits 0-5 the spacing as `2^(10 + bits)` bytes (at most `2^40`). Any other
  non-zero value is refused (`native file-header reserved flags must be
  zero`). A file is always read and appended to as its own header says.
- An anchored file reserves the anchor size at every physical offset
  `start + k * spacing` (`k >= 1`, `start` the append-log start). Records and
  every offset a record, chain or index names are *logical*: the log flows
  around the reserved ranges, and a record that reaches one continues after it.
  A file shorter than `start + spacing` has no anchor and is byte-identical to
  an unanchored one.
- Each anchor, written when the log first reaches it and never rewritten
  (except its summary, below), is little-endian: `"VANC" | version u16 = 1 |
  reserved u16 | k u64 | logical offset u64 | record_start u64 | record_end u64
  | sequence u64` (the record spanning the boundary and its sequence), then an
  optional open summary at byte 48, zero padding, and a CRC-32 of every
  preceding byte in the last four bytes.
- The open summary is `flags u16 (0x1 complete, 0x2 sequence present, 0x4
  segment present) | reserved u16 | tail count u32 | records_before u64 |
  sequence_high_water u64 | segment next position u64 | segment covered start
  u64`, then `tail count` entries of `block id u32 | newest offset u64`. A
  summary that does not fit is left out; an open resumes at the newest anchor
  that carries one. Matrix slots an anchor splits are copied on read (see
  `MmapMatrix`).

### Chunked Records

- A variable record whose logical payload is longer than 64 KiB, in a format
  whose integrity policy is not `none`, is stored chunked: record flag
  `RECORD_FLAG_CHUNKED` (`0x0002`), with `RECORD_FLAG_COMPRESSED` too when its
  chunks are compressed. The header's `uncompressed_len_hint` is `0`.
- The payload is little-endian: a 24-byte header `"VRCK" | version u8 = 1 |
  algorithm u8 (0 none, otherwise the compression algorithm byte) | reserved
  u16 = 0 | logical length u64 | chunk count u32 | CRC-32 of bytes 0..20`; then
  one 16-byte table entry per chunk, `stored end u64 (from the start of the
  data) | CRC-32 of the chunk's logical bytes | reserved u32 = 0`; then the
  chunks, each its 64 KiB of logical bytes (the last shorter) or one
  independent compressed frame. There is no `VCMP` envelope.
- The chunk count is `ceil(logical length / 64 KiB)`. A reader verifies and
  decodes one chunk at a time, so it never holds more than one chunk of the
  stored record. The record checksum still covers the whole stored payload,
  and byte-for-byte copies (rewrites, replacement) carry the table unchanged.

### Commit markers

- A `transaction_marker` commit marker (`0xFFFF_FFF9`) has payload `"VCMT"`
  (4 bytes). Under `single_barrier` it is the 28-byte `VCM2` payload instead,
  in the format's endianness: `"VCM2" | anchor u64 | count u64 | chain u64`.
  `anchor` is the offset of the newest marker known durable when this one was
  written (`u64::MAX` for none); `count` and `chain` cover every record from
  that marker's commit point to this marker. The chain starts at 0 and folds
  each record's `(record offset, sequence, header checksum)` with an
  order-sensitive splitmix64 finaliser
  (`x = chain ^ rotl(offset, 17) ^ rotl(sequence, 41) ^ checksum`, then the
  splitmix64 output mix). See the [durability model](durability-model.md#single-barrier-commit).

### Stream records

- A stream primary's first record is its creation nonce (`0xFFFF_FFF8`,
  version 1, internal): the bare 16-byte nonce, unique per create (not secret), written
  once at create. It
  binds a state sidecar to one creation of the file (see
  [primary generation binding](scalable-io.md#primary-generation-binding)).
- A checkpointed stream writer appends a sync marker (`0xFFFF_FFF4`, version 1,
  internal) after the records each sync commits. Its 32-byte payload is
  little-endian: `"VSYN" | version u16 = 1 | reserved u16 | link start u64 |
  link length u64 | CRC-32 u32 | CRC-32 of bytes 0..28`. The link starts where
  the previous marker ended (or at the sidecar's confirmed end) and its CRC
  covers every physical byte from there to the marker, headers and footers
  included, except the payloads of in-place records (`0xFFFF_FFF3`). Recovery
  accepts a prefix only up to the last marker whose bytes verify.
- The state sidecar (`.vks`) uses the `VARVEIX5` paged container with
  `VARVEVKI` metadata at record version 3 (300 bytes); see
  [scalable I/O](scalable-io.md#durability-and-recovery).

## Preallocated Matrix Wire Contract

All integers below are little-endian. "CRC32" is the IEEE CRC32 computed by
`crc32fast`. Offsets inside matrix structures (ring position, directory
entries, frame targets, the metadata record's ring offset) are logical offsets
of the file's append log. The code is in `crates/varlit-core/src/matrix_tiles`
(engine) and `crates/varlit-core/src/file/matrix_tiles_io.rs` (native host).

### Records

- A matrix lives in internal records of the append log. There is no matrix
  region between the file header and the append log; in a native matrix file
  the append log starts where the file header ends, as in any other file.
- Every matrix storage record has block id `MATRIX_TILE_BLOCK_ID`
  (`0xFFFF_FFF3`) and flags `RECORD_FLAG_INTERNAL | RECORD_FLAG_IN_PLACE`
  (`0x8000 | 0x4000`). The block version names the record kind:

  | Block version | Kind | Payload |
  | --- | --- | --- |
  | 1 | metadata | the `VMTA` record below, one per matrix |
  | 2 | ring | the write-ahead ring, one per matrix |
  | 3 | directory | one 4096-byte directory page |
  | 4 | tile | one `VTIL` tile |
  | 5 | overflow | one `VOVF` overflow region (see Overflow Regions) |
  | 6 | keyed metadata | the `VKMT` record (see Keyed Storage Wire Contract) |
  | 7 | keyed block | 4 KiB pages of keyed storage: tree, overflow or free pages |

- Each record is appended once and its payload is then rewritten in place. For
  a record with `RECORD_FLAG_IN_PLACE` the record checksum leaves the payload
  out (it still covers what the integrity policy covers outside the payload);
  the payload carries its own CRC32s
  (`checksum_record_fields` in `file.rs`). The record validator refuses
  `RECORD_FLAG_IN_PLACE` on any other block id, and `RECORD_FLAG_IN_PLACE`
  missing on `0xFFFF_FFF3`, with `InvalidCanonicalEncoding("in-place record
  flag disagrees with block id")`, and a block version outside `1..=7` with
  `InvalidCanonicalEncoding("unsupported internal record version")`.
- Creation appends the metadata record, then the ring record, and syncs. In a
  native file the metadata record is the first record of the append log: its
  payload is read at `append_log_start + 32`. Tiles and directory pages are
  appended later, the first time a cell in them is written, so a tile that was
  never written does not exist in the file. A new tile or directory page is
  placed in the free space of an overflow region instead of a record of its
  own when one has room (see Overflow Regions); directory entries and roots
  hold payload offsets either way.
- The matrix tile layout version is **2** (`LAYOUT_VERSION` in
  `matrix_tiles/geometry.rs`). It is stored in the metadata record and in
  every tile header. The schema hash of a matrix block folds `matrix-tile-v1`
  and the block's effective tile shape. A 0.12.x file stores its matrix in a
  `VMAT` region and cannot be read by this layout; with a declared schema hash
  its open is refused with `SchemaHashMismatch`, because a matrix spec no
  longer hashes as the `VMAT` build did
  (`a_tiled_spec_no_longer_hashes_as_the_v6_layout_did`).

### Metadata Record (`VMTA`)

| Bytes | Field |
| --- | --- |
| 0..4 | magic `b"VMTA"` |
| 4..6 | layout version `u16` |
| 6..8 | growing flag `u16`: non-zero when the format declares a growing dimension |
| 8..12 | matrix block count `u32` |
| 12..16 | zero |
| 16..24 | extent of block dimension 0 at create, `u64` |
| 24..32 | extent of block dimension 1 at create, `u64` |
| 32..40 | ring record payload offset `u64` |
| 40..48 | ring record payload length `u64` |
| 48..64 | 16-byte creation nonce |
| 64..68 | CRC32 of bytes 0..64 |
| 68..72 | zero |
| 72..80 | payload offset of the newest overflow region, `u64`; `0` for none |

- Bytes 72..80 are outside the CRC32: like the roots, they change through the
  ring.
- From byte 80, one 16-byte directory root per matrix block in spec order:
  root page offset `u64`, then directory height `u64`. Offset `0` means the
  block has no tile yet.
- After the roots comes the extra area, at offsets relative to the start of
  the metadata payload:
  - one 8-byte unit per `single` commit category, in declaration order; byte 0
    non-zero means committed;
  - then, starting at the next multiple of 256, one bitmap per `per_channel`
    commit category in declaration order, `ceil(channels / 8)` bytes rounded
    up to a multiple of 256; channel `c` is bit `c % 8` of byte `c / 8`;
  - then one region per declared aux, `byte_len` rounded up to a multiple of
    256, in declaration order.
- Per-channel bitmaps and aux regions are staged in whole 256-byte pages
  counted from the start of the extra area. The extra area has no checksum of
  its own.
- On open, the reader checks the magic, the layout version, the CRC32, that the block count and the growing flag match
  the spec, and that the ring length is within
  `[MIN_RING, MAX_RING_BYTES]` = `[69632, 64 MiB]`. Any failure is
  `InvalidMatrixLayout`. The extents are taken from the record; the block
  shapes, commit categories and aux sizes come from the spec, whose schema
  hash fixes them.

### Cells, Tiles And Slots

- Each matrix block has a tile shape `[rows, cols]`: the declared `tile`, or,
  when none is declared, `matrix_default_tile(slot_stride)` — the largest
  power of two of cells whose `slot_stride + 4` bytes fit 64 KiB (at least
  one), split as `[cells / cols, cols]` with `cols = 2^ceil(log2(cells) / 2)`.
  Spec validation refuses an empty tile, one over `2^20` cells, or one whose
  cells take more than 64 MiB.
- Cell `(scan, ch)` is in tile `(scan / rows, ch / cols)`, at slot
  `(scan % rows) * cols + ch % cols`.
- Tile payload:

  | Bytes | Field |
  | --- | --- |
  | 0..4 | magic `b"VTIL"` |
  | 4..6 | layout version `u16` |
  | 6..8 | zero |
  | 8..12 | block id `u32` |
  | 12..16 | zero |
  | 16..24 | tile row `u64` |
  | 24..32 | tile column `u64` |
  | 32..36 | CRC32 of bytes 0..32 |
  | 36..40 | zero |
  | 40.. | commit bitmap: `ceil(cells / 8)` bytes, then its CRC32 |
  | next multiple of 8 | `cells` slots of `slot_stride + 4` bytes |

- Slot `s` is committed when bit `s % 8` of bitmap byte `s / 8` is set. The
  bitmap and its CRC32 are one unit, rewritten whole and resealed on every
  change. There is one commit bitmap per tile per block, and a block's
  `category` is the commit category its bits belong to.
- A slot holds the encoded payload (`slot_stride` bytes) followed by the CRC32
  of the payload. A slot that was never written is all zero. A new tile is
  appended with zeroed slots and an empty, sealed bitmap.
- Writing a cell stores payload and CRC32 in its slot and clears its commit
  bit. Committing a cell checks the slot CRC first: a slot whose tile does not
  exist, or which is all zero, is `MatrixCellNotWritten`; any other mismatch is
  `MatrixChecksumMismatch`. Clearing a cell clears its bit and leaves its bytes,
  so committing it again without a write publishes the value it held.
- Reading a cell: a key outside the extents is `MatrixKeyOutOfBounds`
  (dimension 0 is unbounded when the format declares a growing dimension); a
  missing tile or a clear bit is `MatrixNotCommitted`; a bitmap whose CRC32 does
  not match is `InvalidMatrixLayout`; a slot whose CRC32 does not match is
  `MatrixChecksumMismatch` when the read-validation policy's `data_crc` is on.
  Slot bytes are not read when the bit is clear.
- Tile headers are checked by `verify_matrix_metadata`, together with each
  bitmap's CRC32. A damaged tile is a `Recoverable` `CommitMap` finding with a
  `ClearCategory` action for its category. `clear_matrix_category` rewrites
  the bitmaps of that category without checking them, and
  `rebuild_matrix_commit_from_crc` sets the bit of every slot whose CRC32
  holds (`damage_in_place_is_reported_and_a_category_clear_recovers`).

### Tile Directory

- Each block's tiles are found through a radix directory keyed by the Morton
  code of the tile coordinates: bit `i` of the tile row goes to key bit `2i`,
  bit `i` of the tile column to key bit `2i + 1`. A tile row or column above
  `u32::MAX` is `InvalidMatrixLayout`.
- A directory page is `DIR_FANOUT = 512` `u64` offsets (4096 bytes); each level
  resolves 9 key bits. From the root, level `height - 1` down to level 0, the
  entry index is `(key >> (9 * level)) & 511`. A level-0 entry is the tile's
  payload offset; a higher entry is the next page's offset; `0` is absent.
- A height of 0 or above 8 under a non-zero root is `InvalidMatrixLayout`. A
  key wider than the height resolves is an absent tile.
- When a key needs more levels, the writer appends a new root page whose entry
  0 is the old root, so existing keys keep their paths
  (`every_direction_grows_without_moving_a_tile`). Tiles and pages never move
  once appended.

### Write-Ahead Ring

The ring record's payload holds two 128-byte header slots at offsets 0 and
128, and frames from offset 4096 to the end of the payload, written
circularly.

- Ring header:

  | Bytes | Field |
  | --- | --- |
  | 0..4 | magic `b"VRNG"` |
  | 4..6 | version `u16` = 1 |
  | 8..16 | `seq` |
  | 16..24 | `checkpointed`: every generation up to it is in place and durable |
  | 24..32 | `mark`: the file may hold values of generations up to it |
  | 32..40 | `reclaimed`: frames of generations up to it may be overwritten |
  | 40..48 | `tail`: ring offset of the oldest kept frame |
  | 48..56 | `tail_gen`: generation of the frame at `tail` |
  | 56..64 | `head`: ring offset just past the newest committed frame |
  | 64..72 | `published`: newest committed generation |
  | 72..76 | CRC32 of bytes 0..72 |

  Ring offsets are relative to the start of the frame area. A header with
  sequence `seq` is written to slot `seq % 2`. A slot is valid when its magic,
  version and CRC32 match and `reclaimed <= checkpointed <= published`. The
  current header is the valid slot with the higher `seq`; when neither slot is
  valid after eight reads, the result is `InvalidMatrixLayout`.
- Frame: a 32-byte header, `count` entries, zero padding, and a CRC32 of
  everything before it; the total length is a multiple of 8.

  | Bytes | Field |
  | --- | --- |
  | 0..4 | magic `b"VWFR"` |
  | 4..8 | flags `u32`: `1` commit, `2` wrap, `4` index |
  | 8..16 | generation `u64` |
  | 16..20 | frame length `u32`, CRC32 included |
  | 20..24 | entry count `u32` |
  | 24..32 | `requires` `u64`: the file length the frame's targets lie within |

  Each entry is a target offset `u64`, a length `u32`, and that many bytes to
  be written at the target. An index frame (flag `4`) is 64 bytes, has an
  entry count of 0 and holds, instead of entries, the root of the
  generation's on-disk index (see Publication Indexes): root page offset
  `u64`, height `u32` (at most 16), four zero bytes, range count `u64`, then
  four zero bytes before the frame CRC32. A wrap marker is a 40-byte frame with the wrap
  flag and no entries; it sends the scan to ring offset 0. With fewer than 40
  bytes left before the end of the ring, the scan wraps without a marker.
- A publication is one generation: one or more frames carrying the same
  generation number, the last with the commit flag. The writer keeps each frame
  at most a quarter of the ring. Once a publication's frames would take more
  than half the ring, a staged range does not fit in a frame, or a reader's
  overlay (`ring length / 256` ranges, at least 256) could not hold its ring
  ranges, the rest of the publication goes to an index, and its commit frame
  is the index frame. Before it starts an index the writer puts every
  published generation in place, so at most one published generation has an
  index at a time.
- Staged bytes reach the file as they are staged: into a record appended
  since the last publication directly, else into the open ring frame, else
  into overflow space for the index. The open frame's bytes collect in the
  writer's fixed 64 KiB copy buffer and are written whenever it fills and
  when the frame closes, with the header and CRC32 (in the same write when
  the whole frame is still buffered). At most that buffer of staged bytes is
  held in memory; the writer reads what it staged through it.
- A scan reads frames from `tail` and stops at the first frame with a wrong
  magic, unknown flags, a `requires` past the file's current length, a length
  under 36, not a multiple of 8 or past the end of the ring, a generation
  other than the expected next one, a CRC32 mismatch, entries that overrun the
  frame, or an index frame whose length, count or descriptor is wrong.
  Entries of a generation count only once its commit frame is read; frames of
  a generation that never committed are ignored
  (`unpublished_and_torn_publications_never_reach_a_reopen`). A scan streams
  each frame through a 64 KiB buffer: it checks a generation's frames, and
  reads them again to report their entries once its commit frame is found.
  Within a generation, ranges of its index win over its ring entries.
- Publishing (`sync()`, `immediate()`, `commit_durable()`, the durable cell
  writes) writes the staged ranges as frames. When the generation appended
  records, the file is synced before its frames are written. The frames are
  then synced, and the ring header is rewritten with the new `published` and
  `head`. A generation with an index writes its index pages and its overflow
  data before that first sync. `flush()` does not publish. Bytes staged into a record appended
  since the last publication are written to it directly, not through the ring,
  since nothing published points at it yet.
- Staged ranges are whole units: a slot, a bitmap unit, a directory entry, a
  root, a single-commit unit, or a 256-byte page of the extra area.
- Checkpoint: when a frame does not fit, the writer puts the oldest committed
  generations in place until the frame fits and at least half the ring is
  free. It first writes a header raising `mark` to the last generation being
  applied, then writes the ranges of those generations in place in ring
  order (each generation's ring entries, then its index in key order, so the
  newest bytes of each target land last), syncs, writes
  a header with `checkpointed`, `reclaimed`, `tail` and `tail_gen` advanced,
  and syncs again. A checkpoint of every published generation without an
  index reads the ring once, from `tail` to `head`, without checking the
  frames again (the writer published or checked them), and skips a range
  whose target a later range of the same checkpoint rewrites whole. No publication is refused for its size: a range too large
  for a frame, like a publication too large for the ring, goes to an index.
- Ring size: `FormatSpec::with_matrix_ring_bytes`, or by default twice the
  slot bytes of the create-time extents over all blocks, clamped to
  `[256 KiB, 16 MiB]` and to the record payload limit. A declared size over
  `MAX_RING_BYTES` (64 MiB) is `LimitExceeded`. The writer raises a size below
  `MIN_RING` to `MIN_RING` and rounds it up to a multiple of 8.

### Overflow Regions

- An overflow region is the payload of a record with block version 5. It
  starts with a 40-byte header:

  | Bytes | Field |
  | --- | --- |
  | 0..4 | magic `b"VOVF"` |
  | 4..8 | zero |
  | 8..16 | payload offset of the region appended before this one, `0` for none |
  | 16..24 | payload length, header included |
  | 24..32 | `carved`: bytes at the end of the region holding tiles and directory pages |
  | 32..36 | CRC32 of bytes 0..32 |
  | 36..40 | zero |

- Regions form a list from the metadata record's bytes 72..80, newest first;
  each region's offset is lower than the one before it in the list. A new
  region (a whole number of MiB, at least as long as what the publication put
  in overflow space before it, at most the record payload limit) is appended
  zeroed but for its header, and the new head is staged in the same
  publication.
- Between the header and `len - carved` are the staged bytes and the index
  pages of one generation, 8-byte aligned. The writer writes over them only
  when that generation is at or below `reclaimed`; a reopened writer treats
  every region as holding the newest published generation.
- New tiles and directory pages are placed at the end of the free space of
  such a region: `carved` grows by their 8-byte-rounded length and the
  header, CRC32 included, is staged through the ring with the publication
  whose directory first points at them. Until that publication commits
  nothing points into the space, and a crash leaves the header as it was.
- A reopened writer walks the list as its snapshot shows it. It stops at a
  header whose magic, CRC32 or `carved` (a multiple of 8, at most `len - 40`)
  does not check out, at a region past the file's length, at an offset not
  lower than the previous one, or after 64 regions; the regions past that
  point are never reused.

### Publication Indexes

- A generation's index is a B+tree of 4096-byte pages in overflow space,
  keyed by target offset; each range it holds is the newest staged bytes of
  one unit. The writer builds it as it stages and never changes it once the
  generation commits.
- Page: magic `b"VIDX"` (0..4), kind `u16` (4..6: `1` leaf, `2` inner),
  entry count `u16` (6..8), the next leaf's offset `u64` (8..16, leaves only;
  `0` for the last, zero in inner pages), CRC32 of the page with bytes 16..20
  zero (16..20), zero (20..24), entries from byte 24.
- A leaf entry (24 bytes) is target `u64`, offset of the bytes `u64`,
  length `u32`, zero `u32`, in ascending target order; a leaf holds at most
  169. An inner entry (16 bytes) is key `u64`, child page offset `u64`;
  child `i` holds the targets from key `i` up to key `i + 1`, key 0 of the
  leftmost path is zero, and an inner page holds 1 to 254 entries.
- A lookup of `[at, at + len)` descends to the leaf the largest key not
  above `at` routes to and walks leaves from there: units never overlap and
  are restaged whole, so no entry before that leaf reaches into the range.
  A page whose magic, kind, count or CRC32 does not check out, a child or
  next-leaf offset outside the file, or a walk longer than the range count is
  `InvalidMatrixLayout`.
- Readers and the writer reach index pages through a cache of 256 pages
  allocated at open, keyed by page offset and generation.

### Snapshots And Overlay

- A writer may also raise `mark` to its last publication without putting
  anything in place, by writing a header; readers older than it then expire.
  Keyed storage does this before it writes space a published generation
  freed straight to the file.
- A reader's snapshot is the `published` generation it last adopted. On open
  it scans the ring from `tail` and keeps an overlay: for every range written
  by a generation after `checkpointed`, where its newest bytes are in the
  ring. A read takes the file bytes and lays the overlay over them.
- After reading, the reader rereads the ring header. When `mark` is above its
  snapshot, the file may hold newer values, and the read is
  `MatrixSnapshotExpired { snapshot, checkpointed }` instead of a mix of
  generations
  (`a_reader_that_falls_behind_the_ring_is_told_to_follow`). When a range it
  laid from the ring was reclaimed during the read, the read is repeated (up
  to eight times, then `MatrixSnapshotExpired`). An open whose scan is spoiled by a concurrent
  reclaim retries up to 32 times before `MatrixSnapshotExpired`.
- `follow()` adopts the newest `published` generation, scanning only frames
  after its snapshot; when the ring has reclaimed frames past the snapshot it
  rebuilds the overlay from what the ring keeps. Queries never advance the
  snapshot.
- Opening a writer scans the ring the same way and takes the newest generation
  whose frames verify as `published`, rewriting the header when it disagrees;
  nothing is written in place on open, so an open reader's snapshot stays
  readable across a writer reopen.
- A matrix overwrite does not grow the file: slots change in place and the
  ring has a fixed size. Only a first write into a tile appends the tile and
  any directory pages it needs
  (`overwrites_keep_the_file_size_and_new_tiles_add_one_tile`).

## Keyed Storage Wire Contract

A format that declares keyed storage (`keyed_storage: in_place` in
`varlit_format!`, `FormatSpec::with_keyed_storage`) stores its keyed blocks
once per key, in a B-tree of 4 KiB pages inside the file, instead of
appending them to the log. The engine is `crates/varlit-core/src/keyed_heap`;
it shares the write-ahead ring, publication indexes, overflow regions and
snapshots of the matrix engine (`matrix_tiles/ring_store.rs`,
`matrix_tiles/view.rs`), so everything under Write-Ahead Ring, Overflow
Regions, Publication Indexes and Snapshots And Overlay applies unchanged. All
integers are little-endian. Its records have block id `0xFFFF_FFF3` and block
versions 6 and 7 (see the table under Records): the metadata and blocks of
pages. The schema hash folds `keyed-storage-v1` and the free-space ratio. A
format cannot declare both keyed storage and matrix blocks, nor keyed storage
and `keyed_offset_chain`.

### Metadata Record (`VKMT`)

- The first record of the log, appended at create; its payload is read at
  `append_log_start + 32`. 1224 bytes:

  | Bytes | Field |
  | --- | --- |
  | 0..4 | magic `b"VKMT"` |
  | 4..6 | version, 6 |
  | 6..8 | free-space ratio in thousandths of the record bytes, 10 to 50 |
  | 8..24 | zero |
  | 24..40 | ring payload offset and length |
  | 40..48 | change budget: bytes one round may change, 1 MiB to 1 TiB |
  | 48..56 | zero |
  | 56..60 | CRC32 of bytes 0..56 |
  | 60..64 | zero |
  | 64..72 | overflow region list head (as the matrix metadata's bytes 72..80) |
  | 72..200 | the state: 16 `u64` fields, changed through the ring |
  | 200..1224 | the first branch: 64 type roots of 16 bytes, changed through the ring |

- The state fields, in order: two zeros; live keys; the bytes of their keys and values; pages carved;
  the free page list's head, tail and length; the block pages are carved
  from (its first page, its pages, pages carved from it); leaf, interior and
  overflow pages; two zeros. Leaf, interior, overflow and free pages add up
  to the pages carved.
- The first branch: one slot per keyed block type, in the order each type
  stored its first key: `block id u32 | height u32 | root page u64` (height
  1: the root is a leaf). Used slots come first and hold distinct block
  ids; unused slots are zero. A lookup takes its type's slot and searches
  that type's tree; a type with no slot has no keys. A file holds at most
  64 keyed block types.

### Blocks And Pages

- A block is a record of block version 7 holding whole 4096-byte pages: 16
  for the first, twice the last after that up to 256 (1 MiB); a reader
  accepts blocks of up to 4096 pages (16 MiB). Pages are
  carved from the last block in order; a new block is appended when it is
  used up and the free list has none to give.
- A page's first 16 bytes are its link while it is free (`next free page
  u64 | the generation that freed it u64`); the tree and overflow pages
  leave them zero.
- A free page is taken again once the ring's `mark` reaches the generation
  that freed it. The writer raises `mark` to its last publication without
  putting anything in place when freed pages have waited past the
  free-space ratio of the record bytes (at least 1 MiB, at most half the
  change budget), and at the end of every round; readers older than it
  expire.
- An overflow page: link (16) | `magic b"VKOV"` | bytes used `u16` | zero
  `u16` | next overflow page `u64` | up to 4064 bytes.

### The Tree (`VKLF`, `VKIN`)

- A tree key is the record's declared key fields, encoded big-endian
  whatever the format's endian; the block type is the first branch, not
  part of the key. Byte order is key order for unsigned key fields, so keys
  put in increasing order fill pages in order. A signed key field orders negative values
  after positive ones.
- A record's value is the block encoded without its key fields (a fixed
  block: the other fields in order; a variable block: the other fields
  with their headers). A read takes the key fields from the key it looked
  up.
- A tree page: link (16) | header (`magic b"VKLF"` for a leaf or `b"VKIN"`
  for an interior page `| count u16 | high u16 | garbage u16 | six zero
  bytes`) | `count` entries (`offset u16 | length u16`, sorted by key) from
  byte 32 up | cells from the end of the page down to `high` (0 stands for
  4096). `garbage` counts the bytes of removed cells not yet reclaimed.
- A leaf cell: `flags u8 | key length (LEB128) | value length (LEB128) |
  the record's first bytes | overflow page u64` (the page only when `flags`
  is 1). The record is the key then the value. A record of up to 1024 bytes
  is held whole; past that the cell holds `256 + (total - 256) mod 4064`
  bytes, or 256 when that would pass 1024, so its overflow pages are full
  but for the last.
- An interior cell: `child page u64 | flags u8 | separator length (LEB128)
  | the separator, or its first 256 bytes and an overflow page`. The first
  entry's separator is empty. A child holds the keys from its separator
  (included) up to the next entry's (excluded), within the page's bounds.
- A leaf that cannot hold a cell splits: the upper half by bytes moves to a
  new page (a key past every key moves alone), and the shortest prefix of
  the right page's first key greater than the left page's last key goes to
  the parent. An interior split moves its upper half too, and the separator
  of the first entry moved goes up. A root split adds a level.
- A delete removes the cell. A leaf left under a quarter full merges with
  the next or the previous leaf under the same parent when the two fit one
  page. An emptied page leaves the tree with its parent entry; when that
  entry is a page's first, the next entry's separator moves up to the
  lowest ancestor where the path does not take the first child. A root left
  with one child is replaced by it.

### Writing

- Tree pages are staged whole through the ring, and only those whose bytes
  changed. Overflow pages are written whole too: past their link, straight
  to the file when they were carved or freed by a generation already in
  place, else through the ring.
- A round changes at most the change budget, and ends before its
  publication would take more than half the ring less room for the writer's
  page buffers: past either, the writer publishes the round and raises the
  mark to it, so a `sync` that changes more is several publications, each
  atomic. The ring is 2 MiB unless the format declares another size.
- After a publication that leaves half the ring or more taken by
  publications, the writer puts all of them in place (raising `mark` to the
  last), so a frame rarely finds no room and checkpoints only some.
- A reader keeps up to 512 tree pages (2 MiB, allocated when it opens) of
  its generation in memory and empties them when it follows. A lookup whose
  pages all come from there does not read the ring header: its answer is
  its generation's.
- `sync` publishes the changes staged since the last publication as one
  generation. A writer that stops without one publishes nothing past the last
  round it ended: a put or remove that ends a round publishes everything
  staged up to and including it, so changes larger than a round reach readers
  (and survive a reopen) in round-sized atomic steps before the `sync`.
- Pages freed by deleting most keys stay in the file for new keys to take;
  the file is not shrunk (rebuild it to reclaim them).

## Runtime Policy

- Writer model is single-writer per file, enforced with an OS lock on the
  native file object; the `.lock` marker next to it carries diagnostic and
  stale-lock data only (see [durability model](durability-model.md#single-writer-lock-on-the-native-file-object)).
- Reader model is snapshot-on-open, advanced explicitly by `follow()`.
- `FormatSpec::create_layout_writer`,
  `FormatSpec::create_layout_writer_with_header`, and
  `FormatSpec::open_layout_writer`, and `FormatSpec::open_layout_reader` are
  the custom physical-layout entry points. They do not write or expect the
  Varlit-native container header.
- Custom layout files use `LayoutWriter::write_segment` with
  `SegmentWrite { fields, footer_fields, metadata, raw }`; incomplete
  backpatch windows are treated as corrupt physical data by strict open in the
  first slice.
- `open_layout_writer` validates the existing file header, segment literals,
  finalized offsets, and bounds before seeking to EOF for additional appends.
- `LayoutSegmentInfo` exposes validated lead-in and footer field values so
  callers can inspect ToC masks, versions, offset fields, and footer metadata
  without re-parsing bytes manually.
- Durability is explicit: writes are buffered until `flush`, and durable fsync is exposed as `sync`.
- `RecoveryPolicy::Strict` rejects incomplete tails.
- `RecoveryPolicy::TruncateTail` truncates incomplete record header/payload tails and returns a recovery report through `open_recover_with_report`.
- Read-only open never truncates. Recovery truncation is explicit through `open_recover` or `open_recover_with_report`.
- Read-write open for `CommitPolicy::TransactionMarker` truncates uncommitted tail after the latest valid marker so appends cannot accidentally commit stale tail data.
- `IntegrityPolicy::Crc32` and `IntegrityPolicy::Crc32WithHeader` are feature-gated behind `integrity` and reject corrupted covered bytes.
- `IndexPolicy` is a bitset-style policy with `scan_on_open`, `checkpoint_on_flush`, `block_offset_chain`, `keyed_offset_chain`, `segment_on_flush`, `open_digest_on_flush`, and `header_tails`. Offset-chain policies are written automatically in `VARVE3` footers.
- `CommitPolicy::RecordFooter` treats valid record footers as the commit flag for each record.
- `CommitPolicy::TransactionMarker(on_flush|explicit)` appends internal `COMMIT_BLOCK_ID` marker records. Readers expose the latest marker-covered snapshot; writer open truncates uncommitted tail after the latest marker.
- `IndexPolicy::segment_on_flush` makes every commit point append an internal segment record covering exactly the records that commit point added. See [Internal Segments](#internal-segments).
- `IndexPolicy::CheckpointOnFlush` writes an internal checkpoint record, spaced geometrically so cumulative checkpoint bytes stay bounded. **Checkpoint-seeded open is specified but not implemented**: an open that scans walks the full record region (`scan_records_range_at_len`), validates any checkpoint it meets, and discards the checkpoint's decoded entries. See the design target below and [known limitations](known-limitations.md#native-scans-and-temporary-storage).
- `CompressionPolicy::VariableBlocks` and block-specific compression descriptors are feature-gated by the selected backend. The first backend is optional `compression-zstd`; compressed records are rejected when the backend is not enabled.

## Update And Merge

- `replace_fixed` performs snapshot-preserving copy-on-write replacement and
  requires an unchanged encoded payload size. It is refused for a format with
  `segment_on_flush` or with `open_digest_on_flush`, which is also true of
  `replace_fixed_in_place_exclusive`: both restamp a record that an already
  written derived record describes. A segment names the record's offset; a
  digest records the file's sequence high-water mark, and an in-place
  replacement takes a *fresh* sequence while rewriting no derived record, so the
  mark is left below the file's true maximum. A lazy open does not recount — it
  seeds its writer from that mark — so the next append would reuse a sequence
  and the file would stop opening.
- A format declaring `index: header_tails` is **not** refused. That region has a
  defined "claims nothing" state, which a digest record has not, so an in-place
  replacement resets it to cold instead: the next open falls back to the scan
  once and the next commit warms it again.
- `replace_rewrite` rewrites through a completed temporary file and atomically replaces the original file path.
- `replace_block` is the generated/core sequence-preserving replacement path.
  It permits a native fixed or variable payload to grow or shrink, rebuilds
  headers, CRCs, checkpoints, and offset-chain footers, and publishes a new
  snapshot generation. Keyed replacement must preserve the key.
- `replace(index, block)` is the policy-facing entry point. It takes no
  strategy: the route follows from the declaration — a record-footer format
  goes to `replace_block`, a fixed block on a footerless format to
  `replace_fixed`, anything else to `replace_rewrite`.
- `unsafe replace_fixed_in_place_exclusive` is the explicitly unsafe expert
  path for coordinated callers that exclude all overlapping readers/writers.
- Keyed deletes use a common internal tombstone record.
- Keyed ops use user-defined `VarlitMerge::Op`.
- Merge conflict ordering is shard order, then local sequence, then record ordinal; later delta shards win over base.
- `keyed_blocks::<T>()` is latest-put indexed access after tombstones.
- `materialized_keyed_blocks::<T>()` applies same-file puts, ops, and tombstones into current state.
- `VarlitReader` and `VarlitWriter` are additive handle wrappers for clearer read-only and write-capable workflows. Existing `VarlitFile` APIs remain available.

## Codec Contract

- Scalar, option, fixed array, selected vector, tuple, `BTreeMap`, and `HashMap` codecs are canonical and endian-aware.
- `BTreeMap<K, V>` encodes in native sorted key order.
- `HashMap<K, V>` encodes borrowed entries in sorted key order, requiring
  `K: Ord` but not `Clone`, so equivalent maps produce stable bytes independent
  of insertion or hash iteration order.
- Decoding `HashMap<K, V>` preserves values but not insertion order.
- Custom field codecs are supported by implementing `VarlitEncode` and `VarlitDecode` for the field type. The derive macro uses the type's `WIRE_TYPE` in field descriptors and manifests.
- Every codec declares `SCHEMA_ID`, the structural identity of its bytes.
  Built-in scalars declare leaf identities and built-in containers fold their
  elements, so an element that declares none propagates outward as "no
  identity". `#[derive(VarlitBlock)]` folds every field's encode and decode
  `SCHEMA_ID` into the block fingerprint and rejects at compile time any field
  whose codec left `SCHEMA_ID` at the default `0`. A hand-written codec used as
  a block field must therefore declare a value that changes whenever its emitted
  bytes change; identical source spelling is no longer enough to make two custom
  codecs share a schema identity.
- Every built-in public codec declares an identity, including the two value-level
  helpers: `ChunkedBytes` folds the `chunked_bytes` tag, its container-format
  version, and `Vec<u8>`'s identity; `PackedBitmap` folds the `packed_bitmap` tag
  with the `u64` bit length and `Vec<u8>` payload it emits. Both are const
  expressions over compile-time constants, so they are stable across builds and
  platforms, and both are usable as ordinary derived variable fields.
- `PackedBitmap` is **not** a fixed-width matrix field. A matrix slot needs a
  stride known at compile time, and a `Vec`-backed encoding has none. Inline
  matrix `SLOT_STRIDE` is generated from each element's `VarlitEncode::WIRE_TYPE`
  rather than the Rust object size, so a type without a fixed encoded width is
  rejected during const evaluation whatever it is spelled.
- Enum-like values should use explicit custom codecs; automatic enum representation inference is out of scope.
- Length and count limits are format-author policy. Varlit's built-in decoders
  avoid large allocation-before-validation patterns where the remaining bytes
  can be checked generically, but domain-specific maximum string, sequence,
  map, chunk, or decompressed sizes should be enforced by the format's custom
  codecs, compression policy, adapter, or caller validation.
- `ChunkedBytes::decode_to_vec_limited(limit)` is provided for callers that
  need an explicit decompressed-size ceiling.
- `RecordIndexEntry::read_payload_limited(path, limit)` validates the stored
  extent and caller byte ceiling before allocation.
- `RecordIndexEntry::read_logical_payload_limited(spec, path, physical_limit,
  logical_limit)` applies both caller ceilings before allocating the complete
  stored payload or decompressed logical payload.
- Boolean decoders accept only canonical bytes `0` and `1`. Map decoders reject
  duplicate destination keys before decoding a duplicate value.
- Every decoder-owned container charges the materialization budget before it
  reserves, including the set that tracks seen variable field ids above 63: each
  distinct such id charges 8 bytes and an exhausted budget fails with
  `Error::LimitExceeded { resource: "variable field ids" }`. Duplicate detection
  runs before the charge, so a repeated id cannot drain the budget.

## Macro Contract

- `#[derive(VarlitBlock)]` supports fixed and variable structs.
- Variable fields require non-zero unique field ids.
- Keyed blocks support single-field and composite keys.
- Duplicate key fields, missing key fields, duplicate block ids, and reserved block ids fail at compile time where macro input makes that possible.
- `varlit_format!` supports the legacy registry form `pub struct Format { blocks: [A, B]; }` and the format-first form `pub format Format { blocks { fixed A(...) { ... } } }`.
- In format-first form, block structs, `VarlitBlock` implementations, typed reader/writer wrappers, and typed read/write traits are generated from the format declaration.
- `varlit_format!` supports `magic`, `version`, optional `limits`, `endian`, optional `schema_hash`, optional `commit`, optional `integrity`, optional `index`, optional `recovery`, optional `manifest`, optional `liveness` (`none` or `footer_flags`), optional `keyed_storage` (`in_place` or `in_place(free_space = N%)`, N from 1 to 5), optional `header_slots { .. }`, optional `extension`, optional `compression`, optional `preset` / `layout`, optional `dims` / `aux` for matrices, and `blocks`.
- `limits { ... }` is an optional, partial set of operational defaults. Unknown
  and duplicate keys are compile errors. Limits do not affect wire bytes,
  schema hashes, or manifests. Ordinary handles fill missing fields from
  `ReadLimits::STANDARD`; `*_with_resource_limits` may raise or lower defaults,
  while compatibility `*_with_limits` methods only tighten them. The explicit
  trusted-unbounded APIs remain separately named.
- Optional read-check selection uses the Rust `ReadLimits::with_read_validation`
  API; it is not a new `limits { ... }` grammar key. An unset selection preserves
  compatibility. Use `ReadLimits::MISSING` for a runtime override that supplies
  only this selection and preserves the format's other resource defaults.
- `integrity` accepts `none`, `crc32`, or `crc32_with_header`. `crc32`
  covers payload plus native record footer when present. `crc32_with_header`
  additionally covers the native 32-byte record header with the checksum field
  normalized to zero.
- `varlit_format!` supports `preset: varlit_native|none|custom;`. Omitted preset
  means `varlit_native` unless a custom layout is declared; with declared layout
  parts, omission means `none` so the declaration owns byte zero.
- `varlit_format!` supports the first custom layout grammar:
  `layout { file_header Header { bytes sig = b"..."; } segment Name repeat
  until_eof { lead_in Name { bytes tag = b"..."; u32 caller_field; i64 offset =
  finalize(target = segment_end, relative_to = after_lead_in); } metadata Name;
  raw_region Name; footer Footer { bytes write = b"..."; u64 len =
  finalize(target = segment_end, relative_to = segment_start); } } }`.
- Custom physical layout scanning accepts numeric finalized fields with the
  target format's required integer width. Segment `raw_region_start` may be
  resolved relative to `segment_start`, `after_lead_in`, or `metadata_start`;
  `segment_end` may additionally be resolved relative to `raw_region_start`.
- Format-first custom layout declarations generate typed
  `FormatLayoutWriter` / `FormatLayoutReader` wrappers, typed header-field
  structs, typed header info getters, typed segment write structs, and typed
  segment info getters. The generated wrapper still exposes the low-level
  `SegmentWrite` path, streamed segment writes, tolerant scan reports, and
  whole-region or range metadata/raw readers.
- Physical layout declarations are not `VarlitBlock` payload declarations:
  `VarlitBlock` is the logical native append-log record unit, while
  `file_header`, `lead_in`, `footer`, `metadata`, and `raw_region` declare the
  surrounding byte-level framing for custom external formats.
- Custom layouts may declare multiple physical segment descriptors. The scanner
  dispatches each segment by its leading literal lead-in prefix; immediately
  following literal numeric fields extend the dispatch key. Segment-specific
  generated reader indexes are per segment kind, while low-level layout reader
  indexes remain physical stream indexes.
- `schema_hash: computed;` asks the macro to call `with_computed_schema_hash()` after policies are attached.
- `index` accepts either a single legacy identifier or a list such as `[scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain]`. `segment_on_flush` is deliberately **not** one of them: a block is the unit a declaration names, a segment is varlit's internal lookup unit, and its granularity is varlit's decision. Enable it on the `FormatSpec` with `IndexPolicy::with_segment_on_flush`.
- `commit` accepts `none`, `record_footer`, or `transaction_marker(on_flush|explicit)`, optionally `transaction_marker(<mode>, single_barrier)` (one storage barrier per `commit_durable`; requires `crc32_with_header`, refuses `mark_tail`; manifest commit-policy codes 5/6; commit markers carry the 28-byte `VCM2` payload: anchor offset or `u64::MAX`, record count and checksum chain).
- Generated typed writers expose both `commit()` and `commit_durable()`.
  `commit()` is a logical visibility marker and does not imply fsync.
  `commit_durable()` writes commit-covered metadata/checkpoints, flushes and
  syncs data, writes the marker, then syncs the marker.
- `varlit_format!` also supports optional `extension` and global `compression`. Compression syntax is `compression: none;` or `compression: variable_blocks(zstd, level = default|fast|best|N, header = record_explicit|file_explicit|format_contract, min_len = N, only_if_smaller = true|false, max_len = N);`.
- Per-block compression overrides are a runtime `FormatSpec::with_block_compression` API in 0.2, not a macro DSL clause.
- Legacy registry formats expose raw `VarlitReader`/`VarlitWriter` handles. Format-first declarations expose typed `FormatReader`/`FormatWriter` wrappers where methods such as `push_user`, `delete_user`, `users`, `commit`, `commit_durable`, `flush`, and `sync` are generated from block declarations.
- Duplicate top-level `varlit_format!` keys are compile errors. The derive
  macro rejects generic block structs in 0.2 with a direct diagnostic; use a
  concrete block type or manual trait implementations.
- The derive macro emits `VarlitBlock::FIELDS`, including field id, field name, wire type, and required/defaulted presence.
- `#[varlit(default)]` is variable-block-only. Fixed blocks are positional canonical payloads and cannot omit fields.
- `key = "..."` must contain one or more valid Rust identifiers separated by commas. Empty key strings, empty segments, duplicates, and missing fields are compile errors.
- `FormatSpec::computed_schema_hash()` computes a deterministic schema fingerprint from format version, endian, policies, block descriptors, and field descriptors. It intentionally excludes the pinned header `schema_hash` value.
- Custom layout descriptors participate in `computed_schema_hash()`. The
  default `LayoutPreset::VarlitNative` with no custom parts is intentionally not
  hashed so existing native format hashes and bytes remain stable.
- `FormatSpec::schema_debug_dump()` emits a human-readable registered schema view for diagnostics.
- `FormatSpec::diagnostics()`, `FormatSpec::diagnose_file(path)`, and
  `FormatSpec::self_test(path)` provide user-facing sanity checks that classify
  failures as format definition, caller usage, feature gate, file data,
  environment, or library invariant issues.

## Acceptance Criteria

- Roundtrip fixed, variable, nested, option, array, scalar vec, string vec, and bytes payloads.
- Roundtrip and byte-stability checks for deterministic map codecs.
- Lazy typed collections and keyed collections read back typed values.
- `keyed_blocks::<T>()` applies tombstones to latest-put lookup. It does not
  apply user-defined ops; use `materialized_keyed_blocks::<T>()` for put/op/
  tombstone materialization.
- Unknown variable fields are skipped.
- Header/version/endian/schema mismatch is rejected.
- Block version mismatch is rejected on typed read.
- Strict recovery rejects corrupt tails; recover mode truncates incomplete tails.
- CRC integrity detects payload/footer covered-byte modification when feature-enabled.
- `crc32_with_header` detects native record header tampering as well.
- Variable-block compression roundtrips under record-explicit, file-explicit, and format-contract metadata modes.
- Explicit `preset: varlit_native` produces byte-identical output to the omitted
  preset for the same native format declaration.
- Omitted or explicit `preset: varlit_native` exposes an effective layout plan
  containing `VarlitFileHeader`, repeated `VarlitRecord`, and, when applicable,
  `VarlitRecordFooter`.
- `inspect_layout_file` works for both native preset files and `preset: none`
  custom physical layouts, including validated file-header fields.
- A TDMS-style custom layout can write files whose first bytes are `TDSm`, whose
  `next_segment_offset` and `raw_data_offset` fields are backpatched to actual
  metadata/raw boundaries, whose ToC/version fields are visible through
  `LayoutSegmentInfo`, and whose mixed raw channel bytes are sliced by
  adapter-owned chunk metadata.
- The TDMS-style physical writer example tests an external adapter; it is not a
  Varlit-provided TDMS feature. It produces a three-segment scalar type matrix
  using Varlit's generated layout APIs plus adapter toolkit helpers, then
  reopens it with `open_layout_writer` to append another segment. The optional
  Python harness opens that file with `npTDMS` and verifies file properties,
  group/channel lookup, channel properties, changed raw-data-index segments,
  `same-as-previous` raw-index reuse, bool/string/integer/float tagged
  properties, sidecar inspection, byte-backed reading, and exact raw values for
  signed/unsigned integer widths, single/double floats, single/double floats
  with unit type ids, booleans, strings, timestamps, and complex single/double
  floats.
- The reverse TDMS-style harness generates a two-segment scalar type matrix
  with Python `npTDMS`, then parses it through a Varlit-based example adapter
  that uses the generated layout reader plus caller-owned TDMS metadata/raw
  logic. It then appends one segment with Varlit and verifies the result with
  both npTDMS and Varlit.
- The boundary between Varlit generic API obligations and external TDMS adapter
  obligations is fixed as follows. Varlit owns physical-layout declaration and
  validation, segment framing and appends, and the exposure of offsets, lengths,
  field values, metadata and raw byte ranges, and tolerant scan reports. The
  adapter owns TDMS object paths, raw-data-index grammar, property typing,
  timestamp and waveform conversion, scaling, DAQmx raw scalers, sidecar policy,
  and export. If strict layout open succeeds and the adapter can read the
  required ranges, an object-assembly, typing, scaling, timestamp or export
  failure is an adapter issue; if a valid external file cannot be expressed with
  Varlit layout declarations, or Varlit rejects a file before the adapter can
  report a documented physical status, that is a Varlit generic API issue.
- The BMP/Pillow harness verifies a non-TDMS custom physical layout in both
  directions: Varlit writes a 24-bit BMP opened by Pillow, and Pillow writes a
  BMP whose header fields, row-level chunk index, and pixel payload are decoded
  by Varlit.
- A custom physical layout with a declared file header and segment footer writes
  those bytes at the declared offsets, exposes `file_header_len` and validated
  file-header values, and excludes footer bytes from the segment raw-region
  read.
- Custom layout readers reject bad literal tags, truncated lead-ins, invalid
  file headers, invalid offset ordering, footer mismatches, and segment bounds
  beyond EOF.
- Block-specific record-explicit compression can compress one registered variable block while leaving other blocks uncompressed when the global policy is `None`.
- Compressed checkpoint entries and rewrite replacement preserve `uncompressed_len_hint`.
- `compression-zstd` disabled builds report `CompressionFeatureDisabled` when actual compression is attempted.
- Unknown variable field ids with known wire types are skipped, and unknown wire type values fail with `UnknownWireType`.
- Mmap payload windows, matrix slot windows, zero-copy raw fixed reads, and
  zero-copy raw matrix reads are opt-in, feature-gated, and do not change normal
  owned canonical decoding.
- Writer lock breaking is explicit only; default open/create/recover paths refuse an existing lock.
- Macro pass/fail UI tests cover valid formats and invalid schema declarations.
- Property tests cover append, rewrite, merge, tombstone, and op state transitions.
- Performance smoke tests are run during major implementation phases to catch accidental O(n^2) scans, excessive allocations, and slow open/merge paths.
- Benchmarks and smoke checks track encode, decode, append, open/scan,
  checkpoint open, materialized keyed state, merge/compact, direct base+delta
  compact, custom physical layout append/open-scan, recovery, mmap payload
  windows, matrix mmap windows, and zero-copy read paths.
- Compression checks track compressed append/read, physical scan behavior, checkpoint reuse, rewrite, file size, and backend-disabled failure paths.
- Dependency license audit reports only commercially usable permissive choices.

## Performance Guardrails

- Every major feature slice must include a lightweight performance check before it is considered integrated.
- Performance checks may start as ignored tests or benchmark examples, but they must be runnable from the workspace without external services.
- The dependency-free benchmark example is runnable with `cargo run -p varlit --example perf_bench --release -- 10000`.
- The baseline dataset should include at least small, medium, and large record counts so regressions are visible before real-world scale.
- The first benchmark suite should measure wall time, records per second, file size, and approximate allocation-sensitive behavior where practical.
- Any implementation that changes indexing, scanning, merge, codec, compression, recovery, mmap, or zero-copy behavior must document the expected complexity and include a regression-oriented performance check.
- Any implementation that changes custom physical layout framing or scanning
  must include the layout append/open-scan smoke path.
- Direct base+delta compact is measured separately from merge-then-compact so unnecessary intermediate file costs are visible.
- A performance result is not a hard product guarantee during pre-stable versions, but a large unexplained slowdown is a blocking implementation finding.

## Next Wave Contract

This section pins the P0-P2 implementation contracts so worker agents can implement the same behavior.

### Compression

- Compression is opt-in and disabled by default.
- Runtime policy enum: `CompressionPolicy::{None, VariableBlocks(VariableCompression)}`.
- Runtime block override descriptor: `BlockCompressionDescriptor { block_id, compression }`.
- `VariableCompression` includes `algorithm`, `level`, `header_mode`, `min_uncompressed_len`, `only_if_smaller`, and `max_uncompressed_len`.
- The first algorithm is `CompressionAlgorithm::Zstd`, behind the `compression-zstd` feature.
- Block-specific compression applies only to registered `variable` block ids, rejects duplicates, and is limited to `CompressionHeaderMode::RecordExplicit` so each compressed record remains self-describing without changing file-header semantics.
- `only_if_smaller` compares the final stored payload size, including the record-explicit envelope.
- `max_uncompressed_len` is a hard write/read decompression limit and is checked before allocation.
- `CompressionHeaderMode::RecordExplicit` stores a `VCMP` envelope in each compressed payload with algorithm, level, and `u64` uncompressed length.
- `CompressionHeaderMode::FileExplicit` stores compression configuration in a `VARVE2` file header extension. It uses raw compressed record payloads plus `uncompressed_len_hint`, and therefore requires `max_uncompressed_len <= u32::MAX`.
- `CompressionHeaderMode::FormatContract` stores no compression metadata beyond the compressed record flag and length hint. It requires `schema_hash == computed_schema_hash()` and `max_uncompressed_len <= u32::MAX`.
- Existing uncompressed files keep using `VARVE1`. File-explicit compression uses `VARVE2` with a length-prefixed `VCHD` extension.
- Unknown record flag bits are rejected during scan.
- Compressed flags on fixed, internal, metadata, op, tombstone, index, or manifest records are invalid.

### Internal Segments

- A **segment** is varlit's internal lookup unit: the records one commit point
  added. It has no declaration surface, and its granularity is not a knob — a
  block is what a declaration names, a segment is what varlit indexes by.
- Enabled by `IndexPolicy::segment_on_flush`, which is off by default. A file
  written with it off is byte-identical to one written before the option
  existed, and `computed_schema_hash()` is unchanged.
- It requires `block_offset_chain` — the chain *is* `prev_same_block_offset` —
  and a `crc32` integrity policy, because the chain walk trusts a payload that
  describes records it never reads.
- It **supersedes `checkpoint_on_flush`**, and declaring both is refused. Both
  answer "what is the whole index?", but the checkpoint serialises every entry
  from scratch on a geometric cadence while the chain carries the delta a commit
  point added; the checkpoint stops fitting in a record past
  `(max_record_payload_len - 22) / 73` entries while a segment is sized by its
  commit point; and open *reads* the chain, whereas it merely validates a
  checkpoint it walks past and discards the entries. With the chain on, a
  checkpoint is a periodic full copy of the index that nothing reads.
- Segment records use internal block id `SEGMENT_BLOCK_ID` (`0xFFFF_FFF7`),
  block version `1`, and the internal record flag.
- The payload layout is:
  - magic bytes `b"VSEG"`,
  - payload version `u16 = 1`,
  - flags `u16`, currently `0`,
  - covered start offset `u64` — the first record this segment covers,
  - entry count `u64`,
  - preceding record count `u64` — records in the file before the covered start,
  - `entry count` index entries in the checkpoint version 3 entry layout,
  - the segment record's own start offset `u64`.
- The trailing self offset is what makes the record findable from the end of the
  file: the footer carries no self offset, and the header that would give one is
  `payload_len` bytes further back.
- A commit point writes its segment **last**, after any commit marker, and only
  when every record it would cover is committed. A segment record that follows
  the latest commit marker is inside the committed prefix, **whether or not it
  is the final record in the file** — it describes only records that marker
  already committed, so a writer's later unflushed appends do not make it
  uncommitted. Every other record after that marker is uncommitted.
- A commit point that added no record writes no segment.
- A writer that stops without a commit point leaves a data record at the end of
  the file, so the chain has no entry point and the next open scans. This is the
  fallback below, not an error, but it costs the whole benefit and is silent.
- Open reads the last 32 bytes, confirms the record footer magic and version,
  reads the eight bytes before them as the candidate record offset, and confirms
  a segment record there whose extent ends at the file length. It then follows
  `prev_same_block_offset` backwards. Predecessor offsets must decrease strictly
  and stay at or above the append-log start, so the walk is bounded by the
  segments in the file whatever the bytes claim.
- A chain is accepted only if each link ends exactly where its successor's
  coverage began, the entries of each link tile that coverage with no gap and no
  overlap, the oldest link reaches the append-log start, and the whole walk ends
  at the file length. No link may cover another segment record.
- **When `open_digest_on_flush` is also declared, "the file length" above means
  the digest's start offset**, and the walk indexes the digest record itself to
  reach the file length. The digest closes the same commit point the newest
  segment does and is written after it, so the newest segment does not end at
  the file length — a walk that demanded it would reject a perfectly good chain
  and fall back to the scan, which is the segment chain silently turning off.
  The digest is a resident record, so a walk that stopped at the newest segment
  would also hand back an index one entry shorter than a scan of the same bytes.
- Under a transaction-marker policy a chain is additionally refused unless the
  index it produces is **wholly inside the committed prefix**. A chain can tile
  the whole append log and still contain no commit marker — structurally
  perfect, and a lie: the writer open that follows cuts the file back to the
  header, and a handle holding the untruncated index would then append onto an
  offset its own index already claims.
- A chain that fails any of this is not an error: open falls back to the full
  record scan and produces the identical index. So does an open under
  `IntegrityVerification::AtOpen`, and every recovery open.
- **A resource refusal raised while walking is one of those failures, not an
  error.** The walk charges ceilings against bytes it has not validated yet —
  the segment count, and the payload length at an offset the trailer only
  claims is a segment — so a limit hit there is evidence about the chain, not
  about the file. Falling back bypasses nothing: the scan charges `Records`,
  `IndexBytes`, `ScanBytes` and `RecordPayloadLen` itself and refuses honestly
  if the file really is too large. Only `MissingResourceLimit` and
  `TrustedUnboundedRequiresExplicitApi` propagate, because those describe the
  caller's configuration rather than these bytes.
- `replace_block` re-encodes every segment payload against the published
  generation's offsets. In-place replacement is refused, because nothing rewrites
  the segment describing the record it restamps.
- **The append-log start above is the file's, not the header's.** In a matrix
  file it is still where the file header ends: the matrix's storage records are
  the first records of the append log, and the segment chain covers them like
  any other record (`a_matrix_file_begins_its_log_with_its_matrix_records`). A
  matrix format may declare `segment_on_flush` and gets the same segment-chain
  traversal (`a_matrix_file_builds_and_walks_a_segment_chain`).

### Open Digest

- An **open digest** is the three facts an open needs that live in no single
  record: where the committed file ends, what sequence the next append takes,
  and where each block's newest record sits. Today they are the by-product of
  framing every record, and that walk is the only reason an open that builds no
  index still reads the whole file.
- Enabled by `IndexPolicy::open_digest_on_flush`, which is off by default. A
  file written with it off is byte-identical to one written before the option
  existed, and `computed_schema_hash()` is unchanged.
- It requires `block_offset_chain`. The digest hands out each block's tail
  offset and the only thing to do with a tail offset is walk back through
  `prev_same_block_offset`; without the chain it would be handing out an entry
  point to a dead end.
- It is **not** mutually exclusive with `segment_on_flush` or
  `checkpoint_on_flush`. The three answer different questions: a checkpoint and
  a segment chain both let open *build the index* cheaply, a digest lets open
  skip building one at all. A format may declare a digest with either.
- Digest records use internal block id `OPEN_DIGEST_BLOCK_ID` (`0xFFFF_FFF5`),
  block version `1`, and the internal record flag.
- The payload layout is:
  - magic bytes `b"VDIG"`,
  - payload version `u16 = 1`,
  - flags `u16` — bit 0 (`0x0001`) says the sequence field carries a mark.
    Every digest varlit writes sets it, so the word is `1` and never `0`: both
    writing paths always have a mark to report. A digest that sets any other
    bit was written by something that means more by it than this version
    defines, and open answers it with the scan rather than half-understanding
    it,
  - sequence high-water `u64` — the newest sequence in the file as it stands
    once this record is down, which counts the digest record's own sequence.
    It is meaningful only when bit 0 is set, and is written as `0` when that
    bit is clear. **No value is reserved**: `u64::MAX` is a mark like any
    other, and reading it as "no record carries one" would take a file's last
    usable sequence for an absent one,
  - block tail count `u32`,
  - `count` pairs of block id `u32` and newest record offset `u64`, **strictly
    ascending by block id**,
  - the digest record's own start offset `u64`.
- **The committed file end is deliberately not stored.** The digest is the last
  record in the file, so the committed end is the digest's own end — a number
  open holds as soon as it has framed the record, and one that cannot disagree
  with the bytes the way a stored copy could.
- The digest does not carry its own block's tail. It cannot: the record does not
  exist when its payload is built. Nothing walks a digest chain — a digest is
  found at the end of the file, never by following one.
- The size is **constant in the record count**: 20 bytes of prefix, 12 per
  distinct block id, an 8-byte trailer, inside a 32-byte header and a 32-byte
  footer. A file with three block ids carries a 128-byte digest whether it holds
  two hundred records or two billion. A segment, by contrast, carries an entry
  per record it covers.
- A commit point writes its digest **last** — after any commit marker and after
  any segment record — because it is found by probing the end of the file and
  anything appended behind it hides it.
- A digest record that follows the latest commit marker is inside the committed
  prefix, on the same grounds a segment record is: it describes only records
  that marker already committed. The shapes a writer produces after a marker are
  `[SEGMENT]`, `[DIGEST]` and `[SEGMENT, DIGEST]`; a longer run is a file varlit
  did not write, and the prefix is cut back to the marker.
- A commit point whose file already ends in a digest writes no new one, so idle
  flushes do not grow the file.
- A digest is exempt from the `checkpoint_on_flush` cadence's eligible-record
  tail, as the commit marker and the segment record are: it is written because a
  commit point closed, not because a record was added. It is **not** exempt from
  the checkpoint's geometric threshold, which is a function of index position —
  a digest is a resident record and enlarges the index the checkpoint must
  serialise, exactly as a commit marker does.
- `VarlitFile::open_readonly_lazy` reads the digest and frames **no other
  record**. The handle it returns keeps no record directory, because none was
  built: position-based reads (`blocks`, `scan`, `keyed_blocks`) return
  `Error::NoResidentDirectory`, and the caller supplies a directory with
  `with_directory` — typically one built incrementally by `record_map`.
  `block_chain`, `block_tail_offset`, `read_block_at` and every entry-taking
  `_into` read need no directory and work directly, which is what the block
  tails are in the digest for.
- Open reads the last 32 bytes, confirms the record footer magic and version,
  reads the eight bytes before them as the candidate record offset, and confirms
  a digest record there whose extent ends at the file length and whose checksum
  verifies. The payload's trailing self offset must equal that record offset,
  the block ids must ascend strictly, and every tail offset must name a position
  at or after the append-log start and strictly before the digest.
- Strict lazy open returns errors without scanning. It selects header tails when
  declared, otherwise the trailing digest. Unavailable summary metadata returns
  `LazyOpenMetadataUnavailable`; AtOpen verification returns
  `LazyOpenVerificationRequiresScan`. Specific I/O, allocation, checksum and
  resource errors propagate unchanged.
- The caller can explicitly reconstruct state with `open_readonly_scan` or
  `open_scan`. These operations also return errors if reconstruction fails.
  `*_lazy_or_scan[_with_report]` preserves the former automatic behavior:
  header tails, then digest, then scanning, with `LazyOpenSource::FullScan`
  reported only by that explicitly selected automatic mode.
- A writer that stops without a commit point can leave data past the digest.
  Strict lazy open returns an error; the caller decides whether to perform
  scanning recovery. Writer scanning can truncate an uncommitted tail, while
  a failed metadata-only open does not truncate it.
- `replace_block` re-encodes every digest payload against the published
  generation's offsets, as it does every segment payload. A digest payload is
  block tails and a self offset, and a rewrite moves both; copying it would
  publish a generation whose digest describes the file it replaced.
  `replace_rewrite` refuses record-footer formats outright, and a digest
  requires the footer, so that path is unreachable for a digest format.

### Published End

- A native file whose commit policy is `TransactionMarker` carries a `VPUB`
  header extension block, framed `magic[4] | len u32` like the blocks after
  `VCHD`, appended after every other extension block. Its payload (56 bytes)
  is `version u16 = 1 | reserved u16 | reserved u32` followed by two slots of
  24 bytes, each `end u64 | seq u64 | flags u32 | checksum u32`. The checksum
  is FNV-1a 32 over the slot's first 20 bytes. Flag bit `0` is *active*; the
  other bits are zero.
- A file is created with both slots zero. The current slot is the one with the
  larger `seq` among those whose checksum holds. An active current slot bounds
  readers at `end` (a logical offset); an inactive one, or two zero slots, does
  not, and neither does a file without the block (written before it existed).
  Two slots that are neither valid nor zero are unreadable: a reader keeps the
  bound it read last, and the start of the append log on first use.
- A reader treats the file as ending at `min(file length, max(end, append log
  start))` and frames, scans and follows only up to there.
- The writer publishes by writing slot `seq % 2` with the next sequence:
  after the barrier of `commit_durable` or `sync`; at once for `commit`,
  `flush` and its release; and at a read-write open, after syncing what an
  earlier writer left. The block is never synced on its own. A write torn by
  a crash fails its checksum and leaves the previous slot, which names an end
  no larger.
- The block is mutable: an open compares its length, not its contents, with
  what the format would write. A rewrite publishes a replacement with both
  slots zero, and the rebound writer publishes it again.
- Proofs: `published_end::proofs` (slot round trip, the visible length's
  bounds, torn-write fallback, blank and damaged slots).

### Header-Tail Table And Liveness Block

Both are header extension blocks framed `magic[4] | len u32`, rewritten in
place, little-endian.

- **`VBTT`** (`index: header_tails`). The payload is two slots, written
  alternately. A slot is `version u16 = 1 | flags u16 (bit 0: the table did
  not fit) | capacity u32 | count u32 | generation u64 | commit offset u64`,
  then `capacity` entries of `block id u32 | newest record offset u64` (the
  first `count` used, strictly ascending by block id, the rest zero), then a CRC-32 of every preceding byte of
  the slot. `capacity` is the declared block count plus 10 (one per internal
  block id) and is fixed at create. A write takes `generation` one past the
  largest stored and goes to slot `generation % 2`; a reader uses the slot with
  the largest generation whose CRC holds and whose commit offset frames as a
  commit point at the end of the file. A slot with `count = 0` (as created,
  after an in-place replacement, or after an overflow) claims nothing.
- **`VLIV`** (`liveness: footer_flags`). An 8-byte payload `version u16 = 1 |
  flags u16 (bit 0: dirty) | checksum u32`, the checksum being FNV-1a 32 over
  the first four payload bytes. A writer sets dirty when it takes the file and
  clears it on a clean release; a torn or unreadable block reads as dirty.

The mutable header blocks are `VBTT`, `VLIV`, `VHSL` (below) and `VPUB`
([published end](#published-end)). An open compares their lengths, not their
contents, with what the format would write, and the stream generation witness
blanks their payloads.

### Editable Header Region

- An **editable header region** is a fixed-size run of bytes reserved in the
  file header that a caller can rewrite for the life of the file. It exists for
  metadata a caller wants to revise without republishing: a watermark, a
  processing state, a pointer into an external system. Every other way varlit
  records a fact is append-only, which is exactly wrong for a fact that changes.
- Enabled by `HeaderSlots`, off by default. A format declaring none writes the
  bytes it wrote before this existed and `computed_schema_hash()` is unchanged;
  the schema-hash fold is skipped entirely when the region is undeclared rather
  than folding a zero.
- **The size is fixed and is part of the declaration.** `capacity` is folded
  into the schema hash, so a file written under one capacity does not open under
  another. This is what makes the region editable at all: the append log starts
  after it, so a region that could grow would move every record in the file.
  Writing into it moves nothing and changes no file length.
- Only block ids named in the declaration may be written there, and each id
  appears at most once. A second write of the same id **replaces** the first, so
  a fixed region absorbs an unbounded number of edits — writing one block a
  thousand times costs what writing it once costs. Two different blocks must
  both fit.
- A write that would not fit is refused with `Error::LimitExceeded` before any
  byte is written; the region is never left half-updated by a rejected write.
- The region is a mutable `VARVE2` file-header extension block with magic
  `VHSL`, framed `magic[4] | len u32` like every extension after `VCHD`. Its
  payload is:
  - payload version `u16 = 1`,
  - flags `u16` — bit `0` is *sealed*; any other bit set is refused rather than
    ignored,
  - capacity `u32`, which must equal the declared capacity,
  - used `u32` — how many of the capacity bytes the entries occupy,
  - checksum `u32`,
  - `capacity` bytes of entries followed by zero padding to the full capacity.
- One entry is `block_id u32 | block_version u16 | reserved u16 | len u32` then
  `len` payload bytes, canonically encoded exactly as the same block would be
  encoded in a record. `reserved` must be zero. The walk stops at `used`; the
  padding past it is never framed and is zeroed on every write, so a removed
  block's bytes do not survive in the file.
- The checksum is FNV-1a 32 over the payload with the checksum field itself
  excluded. It is **not** a crc32 and is deliberately not gated on the
  `integrity` feature: gating it would make the two verifying policies below
  silently inert in a default build, which is the shape this project treats as a
  defect rather than a convenience. It follows the liveness block's precedent.
- The checksum is *written* under every policy and *verified* under some, so a
  format that later turns its policy up does not have to rewrite files to make
  them verifiable.

**The three integrity policies** are the answer to "exempt from the CRC, or
compute it and freeze it at a chosen point":

| `HeaderSlotIntegrity` | verified on read | writes | the seal |
| --- | --- | --- | --- |
| `None` | never | always allowed | none; `seal_header_slots` is refused |
| `Rolling` | always | always allowed, checksum recomputed each time | none; `seal_header_slots` is refused |
| `Sealed` | only once sealed | allowed until the seal, refused after | `seal_header_slots()` fixes the checksum, one-way |

- `None` is the exemption: the region is editable forever and a changed byte is
  a changed value, not an error.
- `Rolling` verifies continuously, which is what a caller wants when the region
  is read as authoritative throughout the file's life.
- `Sealed` is the freeze point. Before the seal the checksum is advisory — the
  region is still being edited, so a mismatch says nothing a caller could act
  on. After it, reads verify and writes fail. Nothing unseals a region; a caller
  who could unseal would have gained nothing over declaring `Rolling`.
- Sealing is refused on any other policy, rather than doing nothing: `None` has
  no checksum to freeze and `Rolling` recomputes on every write, so one call
  would mean three different things.
- A format declaring matrix blocks may declare the region: the matrix's
  records start after the file header, wherever it ends
  (`header_tails_and_header_slots_combine_with_a_matrix`). A matrix block
  cannot be one of the region's blocks; `validate` refuses that with
  `InvalidFormatSpec("header_slots cannot hold a matrix block")`.
- A republishing `replace` carries the region across verbatim. `VBTT` is reset
  to cold there because every offset it records becomes a lie; this region
  records no offsets, so preserving it is what "the file kept its metadata"
  means.

### Checkpoint Index

- Checkpoint records use internal block id `INDEX_BLOCK_ID`.
- Checkpoint payload version is `u16 = 3`.
- Checkpoint payload layout is:
  - magic bytes `b"VIDX"`,
  - payload version `u16`,
  - covered record end offset `u64`,
  - covered record count `u64`,
  - repeated record index entries for all records covered by the checkpoint except the checkpoint record itself.
- v2 checkpoint entries include `uncompressed_len_hint`. v3 checkpoint entries additionally include footer offset, previous block offset, previous key offset, and committed state. v1/v2 checkpoints still decode with missing v3 fields as absent/defaults.
- A checkpoint is valid only if:
  - the payload decodes exactly,
  - covered offset is within the file length,
  - every entry has valid offsets and payload bounds,
  - entries tile the covered range exactly: each entry starts where the previous entry ended, the first starts at the append-log start, and the last ends at the covered offset,
  - no entry points to the checkpoint record itself,
  - CRC validation passes for the checkpoint payload when integrity is enabled.
- For `IndexPolicy::CheckpointOnFlush`, open should scan from the header until the latest valid checkpoint, then rebuild the index from the checkpoint and scan only records after the covered offset. **Not implemented as of 0.5.0** — this is a design target. The checkpoint is validated on the way past and its entries are discarded; open scans the whole region.
- Structurally corrupt checkpoints are ignored and full scan fallback is allowed.
- CRC mismatch remains fatal and must not be hidden by checkpoint fallback.
- Recovery open may truncate only incomplete header/payload tails after the checkpoint; it must not truncate complete-but-corrupt records.

### Integrity Policy

| Policy | Payload coverage | Header coverage | Checkpoint behavior | Recovery interaction |
| --- | --- | --- | --- | --- |
| `IntegrityPolicy::None` | No checksum validation. | Record headers are structurally parsed only. | Structurally invalid checkpoints can be ignored and full scan fallback is allowed. | Recovery can truncate incomplete tails only. Complete-but-wrong payload bytes may decode as data or fail codec validation. |
| `IntegrityPolicy::Crc32` | Per-record payload CRC32 when the `integrity` feature is enabled. For `VARVE3`, the checksum covers `payload + footer`. | Header fields are structurally parsed but not included in this CRC mode. | Checkpoint payload CRC mismatch is fatal and must not be hidden by fallback. | Recovery can truncate incomplete tails, and transaction-marker readers may ignore corrupt tail after the latest valid marker. Complete CRC mismatches in marker-covered or record-footer-visible records remain errors. |
| `IntegrityPolicy::Crc32WithHeader` | Same payload/footer coverage as `Crc32`. | Also covers the native record header with the checksum field normalized to zero. | Same as `Crc32`. | Same as `Crc32`. |

CRC integrity is a corruption-detection aid, not an authenticity or tamper-proofing mechanism.

### Manifest

- Manifest support is optional and disabled by default.
- Macro syntax: `manifest: none;` or `manifest: embedded;`.
- Runtime policy enum: `ManifestPolicy::{None, Embedded}`.
- Embedded manifests use internal block id `MANIFEST_BLOCK_ID`.
- Manifest payload version is `u16 = 5`.
- Manifest content includes format version, endian, schema hash, extension, index policy, commit policy, integrity policy, recovery policy, manifest policy, compression policy, registered block descriptors, and registered field descriptors.
- v2 field descriptors include field id, wire type, required/defaulted presence, and field name. v1 manifests can still be decoded with empty field lists for compatibility.
- `VarlitFile::schema_manifest()` returns the latest embedded manifest if present.
- Files without a manifest still open.
- Static `FormatSpec` remains authoritative for typed access; the manifest is diagnostic/debug data, not dynamic schema loading.

### Migration Scaffold

- Normal typed reads continue to reject block version mismatches.
- Migration is explicit only.
- Runtime trait: `VarlitMigration<From, To>` with a user-provided conversion function.
- Runtime API: `file.blocks_migrated::<From, To, M>() -> Result<Vec<To>>`.
- Migration requires `From::ID == To::ID`, reads records matching `From::VERSION`, decodes `From`, and applies `M`.
- Macro-generated code does not infer field renames, deletes, or defaults beyond normal `#[varlit(default)]`; semantic conversion stays in user Rust code.

### Compact And Replace

- `replace_fixed` performs a same-size, snapshot-preserving copy-on-write
  publication.
- `replace_rewrite` is explicit full-file rewrite for direct replacement.
- `replace(index, block)` picks among the three by reading the format spec, so
  a caller who wants the replacement rather than a particular mechanism does
  not have to know which one the declaration permits.
- `unsafe replace_fixed_in_place_exclusive` retains the lower-copy path under
  an explicit exclusivity contract.
- Delta/op append remains the recommended append-friendly update path.
- Compact API: `compact_keyed_file::<T, P>(spec, input, output)`.
- Compact reads one file, materializes final keyed state for `T`, drops tombstones and ops, writes only final values to a new output file, and writes metadata/checkpoint according to output format policy.
- Base+delta compact API: `compact_keyed_files::<T, P>(spec, base, deltas, output)`.
- Base+delta compact reads the base and ordered delta shards, applies the same shard-order conflict semantics as merge, and atomically publishes only final values.
- Compact is single keyed block type per call. Non-keyed blocks and unrelated block ids are not copied in this first API.
- Scale contract: `merge_keyed_files`, `compact_keyed_file`, and
  `compact_keyed_files` are resident operations and are explicitly not PB-scale.
  Time is `Theta(records + decoded bytes) + O(N log N) + O(K-live log K-live)`,
  where the `O(N log N)` term is the per-input open's sequence-uniqueness sort
  over that input's `N` records; memory is
  `O(K-ever + largest resident input index + retained live values)`, where
  `K-ever` counts every distinct key ever seen including tombstoned keys.
  Nothing spills to disk and no bounded-memory external merge/compact is
  exported.
- Guarded APIs: `estimate_keyed_merge::<T, P>(spec, base, deltas)` returns a
  decode-free `KeyedMergeEstimate`, and
  `merge_keyed_files_with_key_limit`, `compact_keyed_files_with_key_limit`,
  `compact_keyed_file_with_key_limit` refuse a run that would exceed
  `max_distinct_keys` with `Error::LimitExceeded { resource: "merge distinct
  keys", .. }` before publishing any output.

### Writer Safety

- The authoritative single-writer guard is an OS lock on the native file object. The `.lock` marker file is diagnostic and carries the stale-lock data below.
- Lock files use text payload version `varve-lock-v1` and contain process id, creation timestamp in Unix milliseconds, and target path for diagnostics.
- `FormatSpec::inspect_writer_lock(path)` returns `Ok(None)` when no lock exists, `Ok(Some(WriterLockInfo))` for a valid lock, and `WriterLockMalformed` for malformed metadata.
- Stale-lock breaking is not automatic by default. `FormatSpec::open_with_lock_policy(path, policy)` is the explicit opt-in entry point.
- `WriterLockBreakPolicy::Refuse` preserves the default behavior and returns `WriterLockHeld`.
- `BreakIfOlderThan` is timestamp based; `BreakIfProcessAbsent` is best-effort and conservative when process liveness cannot be determined.
- Malformed locks are never automatically broken in 0.2.
- Atomic rewrite/compact output uses same-directory temp files, flushes and syncs temp contents, then replaces or renames into place.
- Reader behavior is snapshot-on-open by default: a handle fixes its snapshot
  length at open and reads positionally against it. Since 0.8.0
  `VarlitFile::follow`/`VarlitReader::follow` is an explicit `&mut self` advance
  that frames only the bytes past the end the handle holds and adopts them to
  the same commit boundary an open would stop at. It never crosses a generation
  — a republished pathname answers `0` forever, and the pair for that is
  `is_current()` then `reopen_readonly()` — and a read-write handle answers `0`.

### Unknown Fields

- Unknown variable field ids with known wire types are skipped and dropped by default.
- Unknown wire type values are malformed data and return `UnknownWireType`.

### Zero-Copy And Mmap

- Mmap and zero-copy are opt-in features only. The `zero-copy` crate feature implies the `mmap` feature because the first raw-read API is mmap-backed.
- Default typed decode remains owned canonical decoding.
- Initial mmap scope exposes read-only payload windows from unsafe
  `VarlitFile::mmap_payloads() -> MmapPayloads`, or for resident positions
  `records` only, `VarlitFile::mmap_payload_range(records) -> MmapPayloads`,
  whose mapping, copied index and limits cover just that range.
- `MmapPayloads` owns a read-only mmap plus a cloned snapshot index and `FormatSpec`.
- `MmapPayloads::payload_window(entry)` accepts only entries that exactly match the cloned snapshot index, preventing forged public offsets from exposing arbitrary file bytes.
- `MmapPayloads::block_payload_window::<T>(index)` returns the typed block ordinal payload bytes or `None` when out of range.
- Unsafe `VarlitFile::mmap_matrix()` and `VarlitReader::mmap_matrix()` expose
  `MmapMatrix`: a private copy-on-write mapping of the file in which every
  range the handle's snapshot holds in the write-ahead ring is copied into its
  place when the mapping is made, plus a tile reader opened over that mapping.
  The overlay bytes copied are charged against `MaterializedBytes`, the mapped
  length against `MmapLen`.
- Every file-backed mmap constructor requires the caller to prevent mutation,
  truncation, replacement, or backing-object invalidation through every handle,
  thread, and process for the mapping's complete lifetime.
- Under that precondition, safe mmap accessors rely on a cloned existing handle,
  a read-only mapping, checked extents against mapped length, copied snapshot
  metadata, exact record membership, checked slices, and owner-bounded Rust
  lifetimes. Raw views additionally validate kind, version, endian, exact size,
  and alignment.
- `MmapMatrix::cell_payload_window::<T>(key)` returns a committed matrix slot
  payload window. It checks the commit bit and verifies the slot's CRC32,
  returning `MatrixChecksumMismatch` on a mismatch.
  A slot that an anchor splits is not contiguous in the file; its window is a
  copy made on first use and kept with the mapping (at most one per anchor).
- Initial zero-copy scope is limited to explicit raw fixed blocks whose implementor promises endian, alignment, and layout compatibility.
- Raw fixed access is exposed only as `unsafe MmapPayloads::raw_fixed::<T>(index)` where `T: VarlitRawFixedBlock`.
- `VarlitRawFixedBlock` is an unsafe opt-in trait over `VarlitBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout`.
- `raw_fixed` requires fixed block kind, registered id/version/kind, record version match, matching raw endian, exact `size_of::<T>()` payload length, and valid `align_of::<T>()` alignment.
- Raw matrix access is exposed as `unsafe MmapMatrix::raw_cell::<T>(key)` where
  `T: VarlitRawMatrixBlock`; it requires matrix block kind, matching raw
  endian, committed status and a matching slot CRC32 (through
  `cell_payload_window`), exact `size_of::<T>()` slot length, and valid
  alignment.
- Raw zero-copy calls require the caller to guarantee the mapped bytes are not
  mutated by any handle, thread, or process for the lifetime of returned
  references.
- Variable blocks are not zero-copy eligible in the first scaffold.
- No API may silently reinterpret canonical fixed encoding as Rust memory layout.

### External Adapter Toolkit

- The adapter toolkit is a generic layer above custom physical layouts.
  It must not hardcode TDMS. TDMS-style support should emerge from reusable
  declarations plus user-defined codecs, chunk builders, and reducers.
- `BinaryCursor` and `BinaryWriter` provide checked endian-aware
  primitive reads/writes, bounded byte windows, cursor position reporting, and
  length-prefixed bytes/string helpers.
- `TaggedValueCodec` lets adapters map format-specific
  type ids to user value enums without Varlit owning the meaning of those type
  ids.
- `ChunkIndex` and `ChunkIndexBuilder` map physical segment
  raw regions to logical stream chunks. The builder validates bounds against
  `LayoutSegmentInfo`, while the adapter supplies stream keys, value counts,
  stride/interleaving rules, and semantic metadata.
- `SegmentReducer`, `reduce_segments`, and `reduce_segments_by_ref` run
  stateful segmented metadata reductions. Varlit
  owns iteration and diagnostics; the adapter owns inheritance, replacement,
  same-as-previous, deletion, and version semantics.
- Sidecar policy helpers support companion index/cache files. Varlit owns
  path derivation and basic identity diagnostics; the adapter owns sidecar wire
  grammar and rebuild policy.
- A future declarative `varlit_adapter!` or nested `adapter { ... }` DSL may
  generate calls to this runtime layer. The
  DSL should generate adapter scaffolding from declarations but keep custom
  semantics in ordinary user Rust implementations.
- Adapter diagnostics should combine static declaration checks, physical layout
  reports, cursor parse errors, chunk-bound checks, reducer status, and sidecar
  status so users can decide whether a failure belongs to Varlit mechanics,
  adapter declarations, user domain code, or damaged file bytes.
- The layer model is fixed: external file bytes, then the Varlit physical layout,
  then the adapter toolkit primitives above, then user-defined domain semantics,
  then the public adapter API. Varlit declares and verifies reusable binary
  mechanics; the adapter author defines domain meaning wherever the format
  requires it. TDMS is one branch produced by these generic pieces, not a
  hardcoded Varlit feature.
