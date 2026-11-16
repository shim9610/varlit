//! Keyed heap arithmetic and layouts: where linear hashing routes a key and
//! that a split moves exactly the keys `heap::split` moves, chunk lengths,
//! and that every stored structure decodes to what was encoded. I/O, the
//! ring and checksums are outside these harnesses.
use super::{
    CHUNK_ALIGN, CHUNK_HEADER, Chunk, FREE_BLOCK, Region, SLOT_LEN, Slot, State, heap_chunk_len,
};

/// A table shape a writer can reach: up to 2^31 primary buckets, the split
/// pointer inside the current level.
fn any_table() -> State {
    let level: u64 = kani::any();
    let split: u64 = kani::any();
    kani::assume(level < 32);
    kani::assume(split < 1u64 << level);
    State {
        level,
        split,
        ..State::default()
    }
}

/// Every key routes to a bucket the table has.
#[kani::proof]
fn every_route_lands_in_a_bucket_of_the_table() {
    let table = any_table();
    let route: u32 = kani::any();
    assert!(table.bucket_of(route) < table.buckets());
}

/// A split changes the bucket of no key outside the split bucket, and sends
/// each key of the split bucket to the old bucket or to `split + 2^level`
/// by routing bit `level` alone: what `heap::split` does with the slots.
#[kani::proof]
fn a_split_moves_exactly_the_keys_whose_routing_bit_is_set() {
    let table = any_table();
    kani::assume(!(table.level == 31 && table.split + 1 == 1u64 << 31));
    let next = table.after_split();
    assert_eq!(next.buckets(), table.buckets() + 1);
    let route: u32 = kani::any();
    let before = table.bucket_of(route);
    let after = next.bucket_of(route);
    if before == table.split {
        let moves = u64::from(route) & (1u64 << table.level) != 0;
        let target = if moves {
            table.split + (1u64 << table.level)
        } else {
            table.split
        };
        assert_eq!(after, target);
    } else {
        assert_eq!(after, before);
    }
}

/// A chunk is whole alignment units, holds its header, key and payload, and
/// wastes less than one unit; its header gives the same length back.
#[kani::proof]
fn chunk_lengths_are_aligned_and_hold_the_record() {
    let key_len: u32 = kani::any();
    let payload_len: u32 = kani::any();
    let block_id: u32 = kani::any();
    kani::assume(block_id != FREE_BLOCK);
    let needed = CHUNK_HEADER + u64::from(key_len) + u64::from(payload_len);
    let len = heap_chunk_len(u64::from(key_len), u64::from(payload_len));
    assert!(len.is_multiple_of(CHUNK_ALIGN));
    assert!(len >= needed);
    assert!(len < needed + CHUNK_ALIGN);
    let chunk = Chunk {
        payload_len,
        block_id,
        key_len,
        crc: kani::any(),
    };
    assert_eq!(Chunk::decode(&chunk.encode()), chunk);
    assert_eq!(chunk.len(), len);
}

/// A free chunk of any whole-unit length reads back as that length.
#[kani::proof]
fn a_free_chunk_reads_back_as_its_length() {
    let len: u64 = kani::any();
    kani::assume(len >= CHUNK_HEADER && len - CHUNK_HEADER <= u64::from(u32::MAX));
    kani::assume(len.is_multiple_of(CHUNK_ALIGN));
    let free = Chunk::free(len).expect("fits");
    let read = Chunk::decode(&free.encode());
    assert_eq!(read.block_id, FREE_BLOCK);
    assert_eq!(read.len(), len);
}

/// Slots decode to what was encoded.
#[kani::proof]
fn slots_round_trip() {
    let slot = Slot {
        route: kani::any(),
        offset: kani::any(),
        region: kani::any(),
    };
    let bytes = slot.encode();
    assert_eq!(bytes.len() as u64, SLOT_LEN);
    assert_eq!(Slot::decode(&bytes), slot);
}

/// Region headers decode to what was encoded.
///
/// The result is forgotten, not dropped: dropping a `Result` whose variant
/// the verifier cannot fix makes it model the drop of every `Error` variant,
/// which takes more than 14 GB. Decoding itself takes seconds.
#[kani::proof]
fn region_headers_round_trip() {
    let region = Region {
        len: kani::any(),
        live: kani::any(),
        next_all: kani::any(),
        next_free: kani::any(),
        freed: kani::any(),
    };
    kani::assume(region.live <= region.len);
    let read = Region::decode(&region.encode());
    assert!(matches!(&read, Ok(read) if *read == region));
    core::mem::forget(read);
}

/// Hole accounting never wraps: the holes are never more than the heap.
#[kani::proof]
fn holes_are_never_more_than_the_heap() {
    let state = State {
        heap: kani::any(),
        live: kani::any(),
        free: kani::any(),
        ..State::default()
    };
    let tail: u64 = kani::any();
    assert!(state.holes(tail) <= state.heap);
}
