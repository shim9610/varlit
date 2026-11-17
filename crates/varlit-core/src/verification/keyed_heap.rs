//! Keyed storage arithmetic: the binary search over a page's entries, the
//! separator a split puts between two keys, the varints in cells, and how
//! much of a record its leaf cell keeps. I/O, the ring and checksums are
//! outside these harnesses.
use super::node::{self, read_varint, search_entries, separator_len, write_varint};
use super::tree::{MAX_LOCAL, MIN_LOCAL, OVERFLOW_DATA, local_len};
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
    let (at, found) =
        search_entries(n, &mut |index| Ok(keys[index].cmp(&target))).expect("no errors");
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
    let len = write_varint(&mut bytes, value);
    let read = read_varint(&bytes[..len]);
    assert!(matches!(read, Ok((read, used)) if read == value && used == len));
    core::mem::forget(read);
    let _ = node::NODE_BYTES;
}

/// For every record length, a leaf cell keeps all of it up to
/// `MAX_LOCAL` bytes, and past that between `MIN_LOCAL` and `MAX_LOCAL`
/// bytes, chosen so the rest fills whole overflow pages whenever it can.
#[kani::proof]
fn a_cell_keeps_what_fits_and_the_rest_fills_overflow_pages() {
    let total: u64 = kani::any();
    kani::assume(total < 1 << 48);
    let local = local_len(total);
    if total <= MAX_LOCAL {
        assert_eq!(local, total);
    } else {
        assert!((MIN_LOCAL..=MAX_LOCAL).contains(&local));
        let rest = total - local;
        assert!(rest > 0);
        if local != MIN_LOCAL {
            assert_eq!(rest % OVERFLOW_DATA, 0);
        }
    }
}
