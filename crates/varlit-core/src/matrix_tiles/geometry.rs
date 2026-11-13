//! Where every byte of a tiled matrix lives: tile shape and layout, the
//! metadata record, and Morton keys. Pure arithmetic over a validated spec.

use crate::{Error, MatrixKey, Result};

pub(crate) const TILE_MAGIC: &[u8; 4] = b"VTIL";
pub(crate) const META_MAGIC: &[u8; 4] = b"VMTA";
pub(crate) const LAYOUT_VERSION: u16 = 1;

/// Tile record: fixed header, then the commit bitmap and its checksum (one
/// unit, rewritten whole), then the slots (each the cell bytes and their
/// CRC32).
pub(crate) const TILE_HEADER_LEN: u64 = 40;
/// Bytes of the metadata record before the per-block directory roots:
/// fields (48), the creation nonce (16), their CRC32 (4), padding.
pub(crate) const META_FIXED_LEN: u64 = 72;
/// One directory root: `(page offset, height)`.
pub(crate) const META_ROOT_LEN: u64 = 16;

/// Tile coordinates: the cell row and column divided by the tile shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// One matrix block's storage shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockGeom {
    pub block_id: u32,
    pub stride: u64,
    pub tile: [u64; 2],
}

impl BlockGeom {
    pub(crate) const fn cells(&self) -> u64 {
        self.tile[0] * self.tile[1]
    }
    pub(crate) const fn slot_len(&self) -> u64 {
        self.stride + 4
    }
    pub(crate) const fn bitmap_len(&self) -> u64 {
        self.cells().div_ceil(8)
    }
    /// Offset of the bitmap unit (bitmap then its CRC32) in a tile.
    pub(crate) const fn bitmap_at(&self) -> u64 {
        TILE_HEADER_LEN
    }
    pub(crate) const fn bitmap_unit_len(&self) -> u64 {
        self.bitmap_len() + 4
    }
    pub(crate) const fn slots_at(&self) -> u64 {
        (TILE_HEADER_LEN + self.bitmap_unit_len()).next_multiple_of(8)
    }
    pub(crate) const fn tile_bytes(&self) -> u64 {
        self.slots_at() + self.cells() * self.slot_len()
    }
    pub(crate) const fn slot_at(&self, slot: u64) -> u64 {
        self.slots_at() + slot * self.slot_len()
    }
    /// The tile holding `key` and the cell's slot index inside it.
    pub(crate) const fn locate(&self, key: MatrixKey) -> (TileCoord, u64) {
        let [rows, cols] = self.tile;
        (
            TileCoord {
                row: key.scan / rows,
                col: key.ch / cols,
            },
            (key.scan % rows) * cols + key.ch % cols,
        )
    }

    /// A tile record's initial payload: header, an empty bitmap with its
    /// checksum, zeroed slots.
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
        let bitmap = self.bitmap_at() as usize;
        let len = self.bitmap_len() as usize;
        let crc = crc32fast::hash(&out[bitmap..bitmap + len]);
        out[bitmap + len..bitmap + len + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Checks a tile header read back from `offset` is this block's tile `at`.
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
    pub(crate) fn encode_meta(&self, ring: (u64, u64), nonce: [u8; 16], out: &mut [u8]) {
        out.fill(0);
        out[0..4].copy_from_slice(META_MAGIC);
        out[4..6].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&u16::from(self.growing).to_le_bytes());
        out[8..12].copy_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        out[16..24].copy_from_slice(&self.dims[0].to_le_bytes());
        out[24..32].copy_from_slice(&self.dims[1].to_le_bytes());
        out[32..40].copy_from_slice(&ring.0.to_le_bytes());
        out[40..48].copy_from_slice(&ring.1.to_le_bytes());
        out[48..64].copy_from_slice(&nonce);
        let crc = crc32fast::hash(&out[..64]);
        out[64..68].copy_from_slice(&crc.to_le_bytes());
    }

    /// Reads the fixed part of a metadata record: the extents and the ring.
    /// The blocks and the extra area come from the spec, which must agree.
    pub(crate) fn decode_meta(&self, bytes: &[u8]) -> Result<(u64, u64)> {
        if bytes.len() < META_FIXED_LEN as usize
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
    pub(crate) fn decode_nonce(bytes: &[u8]) -> [u8; 16] {
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
