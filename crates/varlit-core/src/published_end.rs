//! The published end: how far readers may adopt a native file's append log.
//!
//! A writer appends records with positional writes, so their bytes reach the
//! file (and every reader's view of it) before the barrier that makes them
//! durable. The published end is what keeps a reader from adopting them early:
//! a `VPUB` header block the writer rewrites in place once a publication is
//! complete, after the barrier of a durable one. A reader treats the file as
//! ending there.
//!
//! The block holds two slots, written alternately, each
//! `end u64 | seq u64 | flags u32 | checksum u32`. The slot with the larger
//! sequence among those whose checksum holds is current. A write torn by a
//! crash fails its checksum and leaves the other slot, which names an earlier
//! (smaller or equal) end, so a torn write can only hide, never reveal. A slot
//! that was never written is all zeros; a block of two such slots does not
//! bound readers, which is also what a file without the block means.
//!
//! The checksum is FNV-1a 32 like the other in-place header blocks: it guards
//! against torn and stale bytes, not against an adversary, and it is not
//! gated on the `integrity` feature.
//!
//! Everything here is pure; the file code does the I/O.

/// The block's magic.
pub(crate) const MAGIC: &[u8; 4] = b"VPUB";
/// Payload version.
pub(crate) const VERSION: u16 = 1;
/// `end u64 | seq u64 | flags u32 | checksum u32`.
pub(crate) const SLOT_LEN: usize = 24;
/// The slots follow `version u16 | reserved u16 | reserved u32`.
pub(crate) const SLOTS_AT: usize = 8;
/// The payload: the version word and two slots.
pub(crate) const PAYLOAD_LEN: usize = SLOTS_AT + 2 * SLOT_LEN;
/// A slot with this flag bounds readers at its end.
pub(crate) const ACTIVE: u32 = 0x0000_0001;
/// Bytes of a slot the checksum covers.
const CHECKED_LEN: usize = SLOT_LEN - 4;

/// The whole block, framing (`magic[4] | len u32`) included.
pub(crate) const fn region_len() -> u64 {
    (8 + PAYLOAD_LEN) as u64
}

/// Whether files of `spec` carry the block: formats whose records become
/// visible at commit markers. Under the other commit policies a record is
/// visible once written, so no visibility waits on a barrier.
pub(crate) const fn applies_to(spec: crate::FormatSpec) -> bool {
    spec.commit_policy.is_transaction_marker()
}

/// [`region_len`] for a spec that carries the block, zero otherwise.
pub(crate) const fn region_len_for(spec: crate::FormatSpec) -> u64 {
    if applies_to(spec) { region_len() } else { 0 }
}

/// The payload of a block whose slots were never written.
pub(crate) fn empty_payload() -> [u8; PAYLOAD_LEN] {
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[..2].copy_from_slice(&VERSION.to_le_bytes());
    payload
}

/// One decoded slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    pub end: u64,
    pub seq: u64,
    pub flags: u32,
}

/// What a block says about readers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Published {
    /// No slot was ever written, or the newest one is inactive: readers read
    /// to the end of the file.
    Unbounded,
    /// Readers stop at this logical offset.
    Bounded(u64),
    /// Neither slot holds, and they are not blank: torn or damaged. A reader
    /// keeps the bound it last read.
    Unreadable,
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// Encodes one slot.
pub(crate) fn encode_slot(slot: Slot) -> [u8; SLOT_LEN] {
    let mut bytes = [0u8; SLOT_LEN];
    bytes[..8].copy_from_slice(&slot.end.to_le_bytes());
    bytes[8..16].copy_from_slice(&slot.seq.to_le_bytes());
    bytes[16..20].copy_from_slice(&slot.flags.to_le_bytes());
    let sum = checksum(&bytes[..CHECKED_LEN]);
    bytes[CHECKED_LEN..].copy_from_slice(&sum.to_le_bytes());
    bytes
}

/// Decodes one slot, `None` when its checksum does not hold.
pub(crate) fn decode_slot(bytes: &[u8; SLOT_LEN]) -> Option<Slot> {
    let mut sum = [0u8; 4];
    sum.copy_from_slice(&bytes[CHECKED_LEN..]);
    if u32::from_le_bytes(sum) != checksum(&bytes[..CHECKED_LEN]) {
        return None;
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    let end = u64::from_le_bytes(word);
    word.copy_from_slice(&bytes[8..16]);
    let seq = u64::from_le_bytes(word);
    let mut flags = [0u8; 4];
    flags.copy_from_slice(&bytes[16..20]);
    Some(Slot {
        end,
        seq,
        flags: u32::from_le_bytes(flags),
    })
}

/// The slot sequence number `seq` is written to, as an index (0 or 1).
pub(crate) const fn slot_index(seq: u64) -> usize {
    (seq % 2) as usize
}

/// What the two slots `slots` (the payload from [`SLOTS_AT`]) say.
pub(crate) fn read(slots: &[u8; 2 * SLOT_LEN]) -> Published {
    let mut first = [0u8; SLOT_LEN];
    let mut second = [0u8; SLOT_LEN];
    first.copy_from_slice(&slots[..SLOT_LEN]);
    second.copy_from_slice(&slots[SLOT_LEN..]);
    choose(
        decode_slot(&first),
        decode_slot(&second),
        slots.iter().all(|byte| *byte == 0),
    )
}

/// The choice [`read`] makes from the two decoded slots (`None` where the
/// checksum failed) and whether every slot byte is zero.
pub(crate) fn choose(first: Option<Slot>, second: Option<Slot>, blank: bool) -> Published {
    let newest = match (first, second) {
        (Some(a), Some(b)) => Some(if b.seq > a.seq { b } else { a }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match newest {
        Some(slot) if slot.flags & ACTIVE != 0 => Published::Bounded(slot.end),
        Some(_) => Published::Unbounded,
        None if blank => Published::Unbounded,
        None => Published::Unreadable,
    }
}

/// The logical length a reader may use, and the bound to remember for the
/// next read.
///
/// `file_len` is the file's logical length, `floor` the start of the append
/// log (nothing before it is ever hidden), and `last` the bound remembered
/// from the previous read (`floor` before the first). A bounded result never
/// exceeds the file nor the published end; an unreadable block keeps the
/// remembered bound, so a torn read can hide, never reveal.
pub(crate) fn visible_len(
    file_len: u64,
    floor: u64,
    published: Published,
    last: u64,
) -> (u64, u64) {
    match published {
        Published::Unbounded => (file_len, last),
        Published::Bounded(end) => {
            let bound = end.max(floor);
            (file_len.min(bound), bound)
        }
        Published::Unreadable => {
            let bound = last.max(floor);
            (file_len.min(bound), bound)
        }
    }
}

#[cfg(kani)]
mod proofs;

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(a: [u8; SLOT_LEN], b: [u8; SLOT_LEN]) -> [u8; 2 * SLOT_LEN] {
        let mut out = [0u8; 2 * SLOT_LEN];
        out[..SLOT_LEN].copy_from_slice(&a);
        out[SLOT_LEN..].copy_from_slice(&b);
        out
    }

    #[test]
    fn a_blank_block_does_not_bound_readers() {
        assert_eq!(read(&[0; 2 * SLOT_LEN]), Published::Unbounded);
    }

    #[test]
    fn the_newer_valid_slot_wins_and_a_torn_one_falls_back() {
        let old = encode_slot(Slot {
            end: 100,
            seq: 1,
            flags: ACTIVE,
        });
        let new = encode_slot(Slot {
            end: 200,
            seq: 2,
            flags: ACTIVE,
        });
        assert_eq!(read(&slots(new, old)), Published::Bounded(200));
        let mut torn = new;
        torn[3] ^= 0xff;
        assert_eq!(read(&slots(torn, old)), Published::Bounded(100));
        assert_eq!(read(&slots(torn, [0xaa; SLOT_LEN])), Published::Unreadable);
    }

    #[test]
    fn visible_length_never_exceeds_the_file_or_the_bound() {
        assert_eq!(
            visible_len(500, 64, Published::Bounded(300), 64),
            (300, 300)
        );
        assert_eq!(
            visible_len(200, 64, Published::Bounded(300), 64),
            (200, 300)
        );
        assert_eq!(visible_len(500, 64, Published::Bounded(10), 64), (64, 64));
        assert_eq!(visible_len(500, 64, Published::Unreadable, 300), (300, 300));
        assert_eq!(visible_len(500, 64, Published::Unbounded, 300), (500, 300));
    }
}
