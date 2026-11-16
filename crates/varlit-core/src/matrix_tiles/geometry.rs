//! Where every byte of a tiled matrix lives: tile shape and layout, the
//! metadata record, and Morton keys. Pure arithmetic over a validated spec.

use crate::{Error, MatrixKey, Result};
#[cfg(verus_keep_ghost)]
use vstd::prelude::*;

pub(crate) const TILE_MAGIC: &[u8; 4] = b"VTIL";
pub(crate) const META_MAGIC: &[u8; 4] = b"VMTA";
pub(crate) const LAYOUT_VERSION: u16 = 1;

/// Tile record: fixed header, then the commit bitmap and its checksum (one
/// unit, rewritten whole), then the slots (each the cell bytes and their
/// CRC32).
#[cfg_attr(verus_keep_ghost, verus_verify)]
pub(crate) const TILE_HEADER_LEN: u64 = 40;
/// Bytes of the metadata record before the per-block directory roots:
/// fields (48), the creation nonce (16), their CRC32 (4), padding.
pub(crate) const META_FIXED_LEN: u64 = 72;
/// One directory root: `(page offset, height)`.
pub(crate) const META_ROOT_LEN: u64 = 16;

/// Tile coordinates: the cell row and column divided by the tile shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(verus_keep_ghost, verus_verify)]
pub(crate) struct TileCoord {
    pub row: u64,
    pub col: u64,
}

impl TileCoord {
    /// The interleaved bits of row and column, so tiles near each other in
    /// both directions share directory pages, and neither direction's growth
    /// renumbers the other's.
    pub(crate) fn morton(self) -> Result<u64> {
        let (Ok(row), Ok(col)) = (u32::try_from(self.row), u32::try_from(self.col)) else {
            return Err(Error::InvalidMatrixLayout);
        };
        Ok(spread(row) | (spread(col) << 1))
    }
}

fn spread(x: u32) -> u64 {
    let mut x = u64::from(x);
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// The offset after a region of `bytes` starting at `at`, rounded up to
/// whole pages of `page` bytes, or `None` when it is not an offset.
pub(crate) const fn after_pages(at: u64, bytes: u64, page: u64) -> Option<u64> {
    match bytes.checked_next_multiple_of(page) {
        Some(len) => at.checked_add(len),
        None => None,
    }
}

/// One matrix block's storage shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(verus_keep_ghost, verus_verify)]
pub(crate) struct BlockGeom {
    pub block_id: u32,
    pub stride: u64,
    pub tile: [u64; 2],
}

// The functions below carry Verus specifications behind `verus_keep_ghost`,
// a cfg only the Verus driver in `tools/verus` sets: ordinary builds see
// plain functions. Verus checks each body against its `ensures`, checks that
// no arithmetic in it overflows under its `requires`, and checks every call
// between them against the callee's `requires`.
#[cfg_attr(verus_keep_ghost, verus_verify)]
impl BlockGeom {
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures r == self.spec_cells(),
    ))]
    pub(crate) const fn cells(&self) -> u64 {
        self.tile[0] * self.tile[1]
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures r == self.spec_slot_len(),
    ))]
    pub(crate) const fn slot_len(&self) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            self.lemma_bounds();
        }
        self.stride + 4
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures r == spec_div_ceil(self.spec_cells(), 8),
    ))]
    pub(crate) const fn bitmap_len(&self) -> u64 {
        self.cells().div_ceil(8)
    }
    /// Offset of the bitmap unit (bitmap then its CRC32) in a tile.
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        ensures r == TILE_HEADER_LEN,
    ))]
    pub(crate) const fn bitmap_at(&self) -> u64 {
        TILE_HEADER_LEN
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures r == spec_div_ceil(self.spec_cells(), 8) + 4,
    ))]
    pub(crate) const fn bitmap_unit_len(&self) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            self.lemma_bounds();
        }
        self.bitmap_len() + 4
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures
            r == self.spec_slots_at(),
            r >= TILE_HEADER_LEN + spec_div_ceil(self.spec_cells(), 8) + 4,
            r % 8 == 0,
    ))]
    pub(crate) const fn slots_at(&self) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            self.lemma_bounds();
        }
        (TILE_HEADER_LEN + self.bitmap_unit_len()).next_multiple_of(8)
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures r == self.spec_tile_bytes(),
    ))]
    pub(crate) const fn tile_bytes(&self) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            self.lemma_bounds();
        }
        self.slots_at() + self.cells() * self.slot_len()
    }
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires
            self.admitted(),
            slot < self.spec_cells(),
        ensures
            r == self.spec_slot_at(slot as int),
            r >= self.spec_slots_at(),
            r + self.spec_slot_len() <= self.spec_tile_bytes(),
            self.spec_tile_bytes() <= u64::MAX,
    ))]
    pub(crate) const fn slot_at(&self, slot: u64) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            self.lemma_bounds();
            self.lemma_slot_within_tile(slot as int);
        }
        self.slots_at() + slot * self.slot_len()
    }
    /// The tile holding `key` and the cell's slot index inside it.
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures
            r.1 < self.spec_cells(),
            r.1 / 8 < spec_div_ceil(self.spec_cells(), 8),
            r.1 == (key.scan % self.tile[0]) * self.tile[1] + key.ch % self.tile[1],
            r.0.row == key.scan / self.tile[0],
            r.0.col == key.ch / self.tile[1],
    ))]
    pub(crate) const fn locate(&self, key: MatrixKey) -> (TileCoord, u64) {
        let [rows, cols] = self.tile;
        #[cfg(verus_keep_ghost)]
        proof! {
            lemma_slot_index_in_range(
                (key.scan % rows) as int,
                (key.ch % cols) as int,
                rows as int,
                cols as int,
            );
        }
        (
            TileCoord {
                row: key.scan / rows,
                col: key.ch / cols,
            },
            Self::slot_index(key.scan % rows, key.ch % cols, cols),
        )
    }

    /// How many cells, starting with `key`'s, lie in its row of its tile: a
    /// run of that many slots starting at `key`'s slot stays inside the tile.
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires self.admitted(),
        ensures
            1 <= r <= self.tile[1],
            r == self.tile[1] - key.ch % self.tile[1],
            (key.scan % self.tile[0]) * self.tile[1] + key.ch % self.tile[1] + r
                <= self.spec_cells(),
    ))]
    pub(crate) const fn row_run(&self, key: MatrixKey) -> u64 {
        #[cfg(verus_keep_ghost)]
        proof! {
            lemma_row_ends_inside_tile(
                (key.scan % self.tile[0]) as int,
                self.tile[0] as int,
                self.tile[1] as int,
            );
        }
        self.tile[1] - key.ch % self.tile[1]
    }

    /// The slot of the cell at `row`, `col` inside a tile `cols` wide: cells
    /// are laid out row by row.
    #[cfg_attr(verus_keep_ghost, verus_verify)]
    #[cfg_attr(verus_keep_ghost, verus_spec(r =>
        requires row * cols + col <= u64::MAX,
        ensures r == row * cols + col,
    ))]
    pub(crate) const fn slot_index(row: u64, col: u64, cols: u64) -> u64 {
        row * cols + col
    }

    /// A tile record's initial payload: header, an empty bitmap with its
    /// checksum, zeroed slots.
    #[cfg_attr(verus_keep_ghost, verifier::external)]
    pub(crate) fn encode_empty_tile(&self, at: TileCoord, out: &mut [u8]) {
        debug_assert_eq!(out.len() as u64, self.tile_bytes());
        out.fill(0);
        out[0..4].copy_from_slice(TILE_MAGIC);
        out[4..6].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&self.block_id.to_le_bytes());
        out[16..24].copy_from_slice(&at.row.to_le_bytes());
        out[24..32].copy_from_slice(&at.col.to_le_bytes());
        let crc = crc32fast::hash(&out[..32]);
        out[32..36].copy_from_slice(&crc.to_le_bytes());
        let bitmap = crate::width::bounded(self.bitmap_at(), out.len());
        let len = crate::width::bounded(self.bitmap_len(), out.len());
        let crc = crc32fast::hash(&out[bitmap..bitmap + len]);
        out[bitmap + len..bitmap + len + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Checks a tile header read back from `offset` is this block's tile `at`.
    #[cfg_attr(verus_keep_ghost, verifier::external)]
    pub(crate) fn check_tile_header(&self, at: TileCoord, header: &[u8]) -> Result<()> {
        let stored = u32::from_le_bytes(header[32..36].try_into().expect("four"));
        if &header[0..4] != TILE_MAGIC
            || u16::from_le_bytes([header[4], header[5]]) != LAYOUT_VERSION
            || u32::from_le_bytes(header[8..12].try_into().expect("four")) != self.block_id
            || u64::from_le_bytes(header[16..24].try_into().expect("eight")) != at.row
            || u64::from_le_bytes(header[24..32].try_into().expect("eight")) != at.col
            || crc32fast::hash(&header[..32]) != stored
        {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(())
    }
}

/// A bitmap unit's bitmap, checked against its trailing CRC32.
pub(crate) fn checked_bitmap(unit: &[u8]) -> Result<&[u8]> {
    let (bits, crc) = unit.split_at(unit.len() - 4);
    if crc32fast::hash(bits) != u32::from_le_bytes(crc.try_into().expect("four")) {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bits)
}

/// Reseals a bitmap unit after its bits changed.
pub(crate) fn seal_bitmap(unit: &mut [u8]) {
    let at = unit.len() - 4;
    let crc = crc32fast::hash(&unit[..at]);
    unit[at..].copy_from_slice(&crc.to_le_bytes());
}

/// Where tiles already looked up are: a fixed table made when a matrix is
/// opened, one slot per tile its extents hold now (within bounds), indexed
/// by a hash of the tile. Tiles never move once allocated, so an entry is
/// right for as long as it is there; a colliding tile replaces it, and a
/// lookup that misses walks the directory again. Nothing here grows.
pub(crate) struct TileCache {
    /// `(block + 1, tile, payload offset)`; block 0 marks an empty slot.
    slots: Vec<(u32, u64, u64)>,
}

impl TileCache {
    const MIN_SLOTS: u64 = 64;
    const MAX_SLOTS: u64 = 1 << 16;

    pub(crate) fn for_geometry(geom: &Geometry) -> Result<Self> {
        let mut tiles = 0u64;
        for block in &geom.blocks {
            let rows = geom.dims[0].div_ceil(block.tile[0]).max(1);
            let cols = geom.dims[1].div_ceil(block.tile[1]).max(1);
            tiles = tiles.saturating_add(rows.saturating_mul(cols));
        }
        let len = crate::width::index(
            tiles
                .clamp(Self::MIN_SLOTS, Self::MAX_SLOTS)
                .next_power_of_two(),
        )?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix tile cache",
                requested: (len * std::mem::size_of::<(u32, u64, u64)>()) as u64,
            })?;
        slots.resize(len, (0, 0, 0));
        Ok(Self { slots })
    }

    fn slot(&self, block: usize, tile: u64) -> usize {
        // Multiplicative hashing, top bits: every bit of the tile and of the
        // block reaches them. The table length is a power of two.
        let mixed = tile.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (block as u64 + 1).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        let bits = self.slots.len().trailing_zeros();
        crate::width::bounded(
            mixed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits),
            self.slots.len(),
        )
    }

    pub(crate) fn get(&self, block: usize, tile: u64) -> Option<u64> {
        let (tag, key, at) = self.slots[self.slot(block, tile)];
        (tag as usize == block + 1 && key == tile).then_some(at)
    }

    pub(crate) fn insert(&mut self, block: usize, tile: u64, at: u64) {
        let slot = self.slot(block, tile);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "block indexes a format's matrix blocks, a static list far below 2^32"
        )]
        let tag = block as u32 + 1;
        self.slots[slot] = (tag, tile, at);
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.slots.fill((0, 0, 0));
    }
}

/// The whole matrix: dimensions, blocks, and the extra metadata area the
/// matrix API lays out (commit flags, aux regions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// Extents at create. Dimension 0 is unbounded when `growing`.
    pub dims: [u64; 2],
    pub growing: bool,
    pub blocks: Vec<BlockGeom>,
    /// Bytes of the metadata area after the roots.
    pub extra_len: u64,
}

impl Geometry {
    /// A copy, refused rather than aborting when memory is short.
    pub(crate) fn try_clone(&self) -> Result<Self> {
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(self.blocks.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix geometry",
                requested: (self.blocks.len() * std::mem::size_of::<BlockGeom>()) as u64,
            })?;
        blocks.extend_from_slice(&self.blocks);
        Ok(Self {
            dims: self.dims,
            growing: self.growing,
            blocks,
            extra_len: self.extra_len,
        })
    }

    pub(crate) fn check_key(&self, key: MatrixKey) -> Result<()> {
        if key.ch >= self.dims[1] || (!self.growing && key.scan >= self.dims[0]) {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        Ok(())
    }

    pub(crate) fn root_at(&self, block: usize) -> u64 {
        META_FIXED_LEN + block as u64 * META_ROOT_LEN
    }
    pub(crate) fn extra_at(&self) -> u64 {
        META_FIXED_LEN + self.blocks.len() as u64 * META_ROOT_LEN
    }
    pub(crate) fn meta_len(&self) -> u64 {
        self.extra_at() + self.extra_len
    }

    /// The metadata record's initial payload. `ring` is the ring record's
    /// payload offset and length.
    pub(crate) fn encode_meta(&self, ring: (u64, u64), creation_id: [u8; 16], out: &mut [u8]) {
        out.fill(0);
        out[0..4].copy_from_slice(META_MAGIC);
        out[4..6].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&u16::from(self.growing).to_le_bytes());
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a format's matrix blocks, a static list far below 2^32"
        )]
        let count = self.blocks.len() as u32;
        out[8..12].copy_from_slice(&count.to_le_bytes());
        out[16..24].copy_from_slice(&self.dims[0].to_le_bytes());
        out[24..32].copy_from_slice(&self.dims[1].to_le_bytes());
        out[32..40].copy_from_slice(&ring.0.to_le_bytes());
        out[40..48].copy_from_slice(&ring.1.to_le_bytes());
        out[48..64].copy_from_slice(&creation_id);
        let crc = crc32fast::hash(&out[..64]);
        out[64..68].copy_from_slice(&crc.to_le_bytes());
    }

    /// Reads the fixed part of a metadata record: the extents and the ring.
    /// The blocks and the extra area come from the spec, which must agree.
    pub(crate) fn decode_meta(&self, bytes: &[u8]) -> Result<(u64, u64)> {
        if bytes.len() < crate::width::small(META_FIXED_LEN)
            || &bytes[0..4] != META_MAGIC
            || u16::from_le_bytes([bytes[4], bytes[5]]) != LAYOUT_VERSION
            || crc32fast::hash(&bytes[..64])
                != u32::from_le_bytes(bytes[64..68].try_into().expect("four"))
            || u32::from_le_bytes(bytes[8..12].try_into().expect("four")) as usize
                != self.blocks.len()
            || (u16::from_le_bytes([bytes[6], bytes[7]]) != 0) != self.growing
        {
            return Err(Error::InvalidMatrixLayout);
        }
        let ring_base = u64::from_le_bytes(bytes[32..40].try_into().expect("eight"));
        let ring_len = u64::from_le_bytes(bytes[40..48].try_into().expect("eight"));
        // The ring's length comes from the file: bounded by the format, not
        // by whatever limits the reader declared.
        if !(super::wal::MIN_RING..=super::wal::MAX_RING_BYTES).contains(&ring_len) {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok((ring_base, ring_len))
    }

    /// The creation nonce stored in a metadata record.
    pub(crate) fn decode_creation_id(bytes: &[u8]) -> [u8; 16] {
        bytes[48..64].try_into().expect("sixteen")
    }

    /// The extents stored in a metadata record.
    pub(crate) fn decode_dims(bytes: &[u8]) -> [u64; 2] {
        [
            u64::from_le_bytes(bytes[16..24].try_into().expect("eight")),
            u64::from_le_bytes(bytes[24..32].try_into().expect("eight")),
        ]
    }
}

// The mathematical model the specifications above refer to, and the lemmas
// their proofs use. Compiled only by Verus.
#[cfg(verus_keep_ghost)]
verus! {

/// `u64::div_ceil` as its documentation defines it.
pub open spec fn spec_div_ceil(a: int, b: int) -> int {
    if a % b == 0 {
        a / b
    } else {
        a / b + 1
    }
}

/// `u64::next_multiple_of` as its documentation defines it.
pub open spec fn spec_next_multiple_of(a: int, b: int) -> int {
    if a % b == 0 {
        a
    } else {
        a + (b - a % b)
    }
}

impl BlockGeom {
    pub(crate) open spec fn spec_cells(&self) -> int {
        self.tile[0] * self.tile[1]
    }

    pub(crate) open spec fn spec_slot_len(&self) -> int {
        self.stride + 4
    }

    pub(crate) open spec fn spec_slots_at(&self) -> int {
        spec_next_multiple_of(TILE_HEADER_LEN + spec_div_ceil(self.spec_cells(), 8) + 4, 8)
    }

    pub(crate) open spec fn spec_slot_at(&self, slot: int) -> int {
        self.spec_slots_at() + slot * self.spec_slot_len()
    }

    pub(crate) open spec fn spec_tile_bytes(&self) -> int {
        self.spec_slots_at() + self.spec_cells() * self.spec_slot_len()
    }

    /// What `FormatSpec::validate` admits for a matrix block: a non-empty
    /// tile of at most 2^20 cells and 64 MiB of slots.
    pub(crate) open spec fn admitted(&self) -> bool {
        &&& self.stride >= 1
        &&& self.tile[0] >= 1
        &&& self.tile[1] >= 1
        &&& self.tile[0] * self.tile[1] <= 0x10_0000
        &&& self.tile[0] * self.tile[1] * (self.stride + 4) <= 0x400_0000
    }

    pub(crate) proof fn lemma_bounds(&self)
        requires
            self.admitted(),
        ensures
            1 <= self.spec_cells() <= 0x10_0000,
            self.spec_cells() * self.spec_slot_len() <= 0x400_0000,
            self.spec_slot_len() <= 0x400_0000,
            48 <= self.spec_slots_at() <= 40 + 131072 + 4 + 8,
            self.spec_tile_bytes() <= 40 + 131072 + 4 + 8 + 0x400_0000,
    {
        let cells = self.spec_cells();
        let len = self.spec_slot_len();
        assert(cells >= 1) by (nonlinear_arith)
            requires
                cells == self.tile[0] * self.tile[1],
                self.tile[0] >= 1,
                self.tile[1] >= 1,
        ;
        assert(len <= cells * len) by (nonlinear_arith)
            requires
                cells >= 1,
                len >= 0,
        ;
    }

    /// A slot below the cell count ends inside the tile record.
    pub(crate) proof fn lemma_slot_within_tile(&self, slot: int)
        requires
            self.admitted(),
            0 <= slot < self.spec_cells(),
        ensures
            self.spec_slot_at(slot) + self.spec_slot_len() <= self.spec_tile_bytes(),
            self.spec_slot_at(slot) >= self.spec_slots_at(),
    {
        let cells = self.spec_cells();
        let len = self.spec_slot_len();
        assert(slot * len + len <= cells * len && slot * len >= 0) by (nonlinear_arith)
            requires
                0 <= slot < cells,
                len >= 0,
        ;
    }
}

/// A cell inside a tile gets a slot below the tile's cell count.
pub(crate) proof fn lemma_slot_index_in_range(row: int, col: int, rows: int, cols: int)
    requires
        0 <= row < rows,
        0 <= col < cols,
    ensures
        0 <= row * cols + col < rows * cols,
        rows * cols >= 1,
{
    assert(row * cols + col < rows * cols && row * cols >= 0) by (nonlinear_arith)
        requires
            0 <= row < rows,
            0 <= col < cols,
    ;
}

/// The last cell of a tile row is still inside the tile.
pub(crate) proof fn lemma_row_ends_inside_tile(row: int, rows: int, cols: int)
    requires
        0 <= row < rows,
        0 <= cols,
    ensures
        row * cols + cols <= rows * cols,
{
    assert(row * cols + cols <= rows * cols) by (nonlinear_arith)
        requires
            0 <= row < rows,
            0 <= cols,
    ;
}

/// A run of slots that ends inside the tile ends inside the tile record.
pub(crate) proof fn lemma_run_within_tile(geom: BlockGeom, slot: int, run: int)
    requires
        geom.admitted(),
        0 <= slot,
        0 <= run,
        slot + run <= geom.spec_cells(),
    ensures
        geom.spec_slot_at(slot) + run * geom.spec_slot_len() <= geom.spec_tile_bytes(),
{
    let cells = geom.spec_cells();
    let len = geom.spec_slot_len();
    assert(slot * len + run * len <= cells * len) by (nonlinear_arith)
        requires
            0 <= slot,
            0 <= run,
            slot + run <= cells,
            len >= 0,
    ;
}

/// Two cells of one tile share a slot only when they are the same cell.
pub(crate) proof fn lemma_slot_index_injective(
    row_a: int,
    col_a: int,
    row_b: int,
    col_b: int,
    rows: int,
    cols: int,
)
    requires
        0 <= row_a < rows,
        0 <= row_b < rows,
        0 <= col_a < cols,
        0 <= col_b < cols,
        row_a * cols + col_a == row_b * cols + col_b,
    ensures
        row_a == row_b,
        col_a == col_b,
{
    if row_a < row_b {
        assert(row_a * cols + col_a < row_b * cols + col_b) by (nonlinear_arith)
            requires
                0 <= row_a < row_b,
                0 <= col_a < cols,
                0 <= col_b,
        ;
    } else if row_b < row_a {
        assert(row_b * cols + col_b < row_a * cols + col_a) by (nonlinear_arith)
            requires
                0 <= row_b < row_a,
                0 <= col_b < cols,
                0 <= col_a,
        ;
    }
}

} // verus!

#[cfg(kani)]
#[path = "../verification/matrix_geometry.rs"]
mod geometry_proofs;
