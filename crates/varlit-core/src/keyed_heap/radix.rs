//! Radix directories: 4096-byte pages of 512 eight-byte entries, nine bits
//! of the key per level, mapping a number (a bucket, a page of a size
//! class) to a file offset. A root is `(page, height)`; `(0, 0)` is empty.

use super::corrupt;
use super::lookup::View;
use crate::Result;
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::directory_offset;
use crate::matrix_tiles::{DIR_FANOUT, DIR_LEVEL_BITS};

/// The height a directory needs to hold `key`.
pub(crate) const fn height_for(key: u64) -> u64 {
    let bits = 64 - key.leading_zeros();
    let levels = bits.div_ceil(DIR_LEVEL_BITS);
    if levels == 0 { 1 } else { levels as u64 }
}

/// Whether a directory of `height` levels reaches `key`.
pub(crate) const fn reaches(height: u64, key: u64) -> bool {
    height >= 8 || key >> (DIR_LEVEL_BITS as u64 * height) == 0
}

/// The entry index of `key` at `level` (0 for the leaves).
pub(crate) const fn index_at(key: u64, level: u64) -> u64 {
    (key >> (DIR_LEVEL_BITS as u64 * level)) & (DIR_FANOUT - 1)
}

/// The offset `key` maps to; `0` when nothing is mapped.
pub(crate) fn get(
    view: &mut dyn View,
    source: &dyn TileSource,
    root: (u64, u64),
    key: u64,
) -> Result<u64> {
    let (mut page, height) = root;
    if page == 0 || !reaches(height, key) {
        return Ok(0);
    }
    for level in (0..height).rev() {
        let mut entry = [0u8; 8];
        view.read(source, page + index_at(key, level) * 8, &mut entry)?;
        page = directory_offset(u64::from_le_bytes(entry))
            .map_err(|_| corrupt("keyed directory entry"))?;
        if page == 0 {
            return Ok(0);
        }
    }
    Ok(page)
}
