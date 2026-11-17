//! Keyed storage arithmetic and layouts: the binary search over a page's
//! entries, the separator a split puts between two keys, the varints in
//! entry bodies, which size class an item goes to and how a record splits
//! into items, and that every stored structure decodes to what was encoded.
//! I/O, the ring and checksums are outside these harnesses.
use super::node::{self, put_varint, search, separator_len, varint};
use super::{
    CLASSES, ClassEntry, ITEM_HEADER, Item, MAX_ITEM, chain_bytes, chain_shape, class_of,
    class_size, item_ref, ref_class, ref_index,
};
use core::cmp::Ordering;

/// Over every sorted array of up to eight entries and every target, the
/// search returns the first entry not less than the target, and says
/// whether it is equal: what `tree::find` relies on in each page.
#[kani::proof]
#[kani::unwind(10)]
fn the_entry_search_finds_the_insertion_point() {
    let n: usize = kani::any();
    kani::assume(n <= 8);
    let mut keys = [0u8; 8];
    for index in 0..8 {
        keys[index] = kani::any();
        if index > 0 && index < n {
            kani::assume(keys[index - 1] < keys[index]);
        }
    }
    let target: u8 = kani::any();
    let (at, found) = search(n, &mut |index| Ok(keys[index].cmp(&target))).expect("no errors");
    assert!(at <= n);
    assert!(keys[..at].iter().all(|&key| key < target));
    if at < n {
        assert!(keys[at] >= target);
        assert_eq!(found, keys[at] == target);
    } else {
        assert!(!found);
    }
}

/// For every pair of keys of up to four bytes with `low < high`, the
/// separator a split chooses lies between them: `low < S <= high`.
#[kani::proof]
#[kani::unwind(6)]
fn a_separator_lies_between_its_keys() {
    let low_len: usize = kani::any();
    let high_len: usize = kani::any();
    kani::assume(low_len <= 4 && high_len <= 4);
    let low: [u8; 4] = kani::any();
    let high: [u8; 4] = kani::any();
    let (low, high) = (&low[..low_len], &high[..high_len]);
    kani::assume(low.cmp(high) == Ordering::Less);
    let separator = &high[..separator_len(low, high)];
    assert!(low < separator && separator <= high);
}

/// Every u64 round-trips through a varint, and no other encoding of it
/// reads back.
#[kani::proof]
#[kani::unwind(12)]
fn varints_round_trip() {
    let value: u64 = kani::any();
    let mut bytes = [0u8; 10];
    let len = put_varint(&mut bytes, value);
    let read = varint(&bytes[..len]);
    assert!(matches!(read, Ok((read, used)) if read == value && used == len));
    core::mem::forget(read);
    let _ = node::NODE_BYTES;
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

/// Item headers decode to what was encoded.
#[kani::proof]
fn layouts_round_trip() {
    let item = Item {
        len: kani::any(),
        head: kani::any(),
        separator: kani::any(),
        block_or_route: kani::any(),
        key_len: kani::any(),
        crc: kani::any(),
        dead: kani::any(),
        next: kani::any(),
        total_or_prev: kani::any(),
    };
    kani::assume(item.len < 1 << 30);
    assert_eq!(Item::decode(&item.encode()), item);
}

/// Class entries decode to what was encoded. Apart from the item header
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
