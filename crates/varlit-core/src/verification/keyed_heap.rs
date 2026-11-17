//! Keyed storage arithmetic and layouts: where linear hashing routes a key
//! and that a split moves exactly the keys `heap::split` moves, which size
//! class an item goes to and how a record splits into items, and that every
//! stored structure decodes to what was encoded. I/O, the ring and checksums
//! are outside these harnesses.
use super::{
    CLASSES, ClassEntry, ITEM_HEADER, Item, MAX_ITEM, Slot, State, chain_bytes, chain_shape,
    class_of, class_size, item_ref, ref_class, ref_index,
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

/// A merge undoes a split exactly, and changes the bucket of no key outside
/// the merged bucket, which all go to the bucket it was split from: what
/// `heap::merge` does with the slots.
#[kani::proof]
fn a_merge_undoes_a_split_and_moves_only_the_merged_keys() {
    let table = any_table();
    kani::assume(table.buckets() > 1);
    let next = table.after_merge();
    assert_eq!(next.buckets() + 1, table.buckets());
    assert_eq!(next.after_split(), table);
    let from = next.split + (1u64 << next.level);
    let route: u32 = kani::any();
    let before = table.bucket_of(route);
    let after = next.bucket_of(route);
    if before == from {
        assert_eq!(after, next.split);
    } else {
        assert_eq!(after, before);
    }
}

/// An item of any length up to the largest goes to the smallest class that
/// holds it, which wastes at most a sixteenth (or 16 bytes).
#[kani::proof]
#[kani::unwind(9)]
fn an_item_goes_to_the_smallest_class_that_holds_it() {
    let len: u64 = kani::any();
    kani::assume(len >= 1 && len <= MAX_ITEM);
    let class = class_of(len).expect("fits");
    assert!(class < CLASSES);
    assert!(class_size(class) >= len);
    if class > 0 {
        assert!(class_size(class - 1) < len);
    }
    // Past the smallest class, at most a sixteenth (or 15 bytes) larger.
    if len > class_size(0) {
        assert!(class_size(class) <= (len * 17 / 16).next_multiple_of(16).max(len + 15));
    }
}

/// A record of any length below 16 MiB splits into full items and one that
/// holds the rest; the items hold the record and its headers. (Longer
/// records take the same arithmetic; the verifier's division circuit is
/// what bounds the length here, and the native tests check longer ones.)
#[kani::proof]
#[kani::unwind(9)]
fn a_record_splits_into_items_that_hold_it() {
    let total: u64 = kani::any();
    kani::assume(total < 1 << 24);
    let (full, last) = chain_shape(total);
    let per_full = MAX_ITEM - ITEM_HEADER;
    assert!(last < CLASSES);
    let rest = total - full * per_full;
    assert!(rest + ITEM_HEADER <= class_size(last));
    assert!(full == 0 || rest >= 1);
    assert_eq!(chain_bytes(total), full * MAX_ITEM + class_size(last));
}

/// Item references keep their class and index.
#[kani::proof]
fn item_references_round_trip() {
    let class: usize = kani::any();
    let index: u64 = kani::any();
    kani::assume(class < CLASSES);
    kani::assume(index < 1 << 56);
    let item = item_ref(class, index);
    assert_eq!(ref_class(item), class);
    assert_eq!(ref_index(item), index);
}

/// Slots, item headers and class entries decode to what was encoded.
#[kani::proof]
fn layouts_round_trip() {
    let slot = Slot {
        route: kani::any(),
        item: kani::any(),
    };
    assert_eq!(Slot::decode(&slot.encode()), slot);
    let item = Item {
        len: kani::any(),
        head: kani::any(),
        block_or_route: kani::any(),
        key_len: kani::any(),
        crc: kani::any(),
        dead: kani::any(),
        next: kani::any(),
        total_or_prev: kani::any(),
    };
    kani::assume(item.len < 1 << 31);
    assert_eq!(Item::decode(&item.encode()), item);
}

/// Class entries decode to what was encoded. Apart from the slot and item
/// harness: together they take more of the verifier's memory than needed.
#[kani::proof]
fn class_entries_round_trip() {
    let entry = ClassEntry {
        end: kani::any(),
        dir: (kani::any(), kani::any()),
    };
    kani::assume(entry.end < 1 << 56);
    kani::assume(entry.dir.1 <= 8 && (entry.dir.0 == 0) == (entry.dir.1 == 0));
    let read = ClassEntry::decode(&entry.encode());
    assert!(matches!(&read, Ok(read) if *read == entry));
    core::mem::forget(read);
}
