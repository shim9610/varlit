//! Publishing raw bytes the way a writer does.
//!
//! A native file of a transaction-marker format carries a published end (the
//! `VPUB` header block, `docs/spec.md`): readers treat the file as ending
//! there, so bytes appended without a writer are invisible to them until the
//! end is moved over them. A target that appends bytes to see what a reader
//! makes of them publishes them with [`publish`]. The block is encoded here as
//! the specification describes it, independently of the library.

const MAGIC: &[u8; 4] = b"VPUB";
const PAYLOAD_LEN: usize = 56;
const SLOTS_AT: usize = 8;
const SLOT_LEN: usize = 24;
const ACTIVE: u32 = 1;
/// The block lives in the file header, which is far smaller than this.
const HEADER_WINDOW: usize = 64 * 1024;

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

fn word(bytes: &[u8]) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(out)
}

fn half(bytes: &[u8]) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(out)
}

/// Bytes of the block's two slots.
pub const SLOTS_LEN: usize = 2 * SLOT_LEN;

/// Offset of the block's two slots, if the bytes carry the block.
pub fn slots_at(bytes: &[u8]) -> Option<usize> {
    let header = &bytes[..bytes.len().min(HEADER_WINDOW)];
    header
        .windows(8)
        .position(|window| &window[..4] == MAGIC && half(&window[4..]) == PAYLOAD_LEN as u32)
        .map(|at| at + 8 + SLOTS_AT)
        .filter(|at| at + 2 * SLOT_LEN <= bytes.len())
}

/// Moves the published end to the end of `bytes`, as a writer's next
/// publication would. Does nothing to bytes without the block.
pub fn publish(bytes: &mut [u8]) {
    let Some(at) = slots_at(bytes) else {
        return;
    };
    let newest = bytes[at..at + 2 * SLOT_LEN]
        .chunks_exact(SLOT_LEN)
        .filter(|slot| half(&slot[20..]) == checksum(&slot[..20]))
        .map(|slot| word(&slot[8..]))
        .max()
        .unwrap_or(0);
    let seq = newest.wrapping_add(1);
    let mut slot = [0u8; SLOT_LEN];
    slot[..8].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    slot[8..16].copy_from_slice(&seq.to_le_bytes());
    slot[16..20].copy_from_slice(&ACTIVE.to_le_bytes());
    let sum = checksum(&slot[..20]);
    slot[20..].copy_from_slice(&sum.to_le_bytes());
    let index = (seq % 2) as usize;
    bytes[at + index * SLOT_LEN..at + (index + 1) * SLOT_LEN].copy_from_slice(&slot);
}
