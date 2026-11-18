//! Test access to a native file's published end (`VPUB` header block).
//!
//! Tests that write bytes into a file without a writer, to see what a reader
//! makes of them, have to publish those bytes the way a writer would, or a
//! reader rightly never sees them. These helpers encode the block as
//! `docs/spec.md` describes it, independently of the library.
#![allow(dead_code)]

use std::path::Path;

const MAGIC: &[u8; 4] = b"VPUB";
const PAYLOAD_LEN: usize = 56;
const SLOTS_AT: usize = 8;
const SLOT_LEN: usize = 24;
const ACTIVE: u32 = 1;

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// Offset of the block's two slots, if the file carries the block.
pub fn slots_at(bytes: &[u8]) -> Option<usize> {
    let header = &bytes[..bytes.len().min(64 * 1024)];
    header
        .windows(8)
        .position(|window| {
            &window[..4] == MAGIC
                && u32::from_le_bytes(window[4..8].try_into().unwrap()) == PAYLOAD_LEN as u32
        })
        .map(|at| at + 8 + SLOTS_AT)
}

fn newest_seq(slots: &[u8]) -> u64 {
    slots
        .as_chunks::<SLOT_LEN>()
        .0
        .iter()
        .filter(|slot| {
            u32::from_le_bytes(slot[20..24].try_into().unwrap()) == checksum(&slot[..20])
        })
        .map(|slot| u64::from_le_bytes(slot[8..16].try_into().unwrap()))
        .max()
        .unwrap_or(0)
}

/// Publishes `end` in `bytes`, as a writer's next publication would.
pub fn publish(bytes: &mut [u8], end: u64) {
    let at = slots_at(bytes).expect("the file carries a published end block");
    let seq = newest_seq(&bytes[at..at + 2 * SLOT_LEN]) + 1;
    let mut slot = [0u8; SLOT_LEN];
    slot[..8].copy_from_slice(&end.to_le_bytes());
    slot[8..16].copy_from_slice(&seq.to_le_bytes());
    slot[16..20].copy_from_slice(&ACTIVE.to_le_bytes());
    let sum = checksum(&slot[..20]);
    slot[20..].copy_from_slice(&sum.to_le_bytes());
    let index = (seq % 2) as usize;
    bytes[at + index * SLOT_LEN..at + (index + 1) * SLOT_LEN].copy_from_slice(&slot);
}

/// Publishes the whole file at `path`, in place.
pub fn publish_file_end(path: &Path) -> std::io::Result<()> {
    let mut bytes = std::fs::read(path)?;
    let end = bytes.len() as u64;
    publish(&mut bytes, end);
    let at = slots_at(&bytes).expect("the file carries a published end block");
    use std::os::unix::fs::FileExt;
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all_at(&bytes[at..at + 2 * SLOT_LEN], at as u64)?;
    Ok(())
}

/// `bytes` with the published end's slots zeroed: what stays the same
/// across publications.
pub fn without_published_end(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if let Some(at) = slots_at(bytes) {
        out[at..at + 2 * SLOT_LEN].fill(0);
    }
    out
}
