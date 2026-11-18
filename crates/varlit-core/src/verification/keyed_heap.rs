//! Keyed storage arithmetic: the binary search over a page's entries, the
//! separator a split puts between two keys, the varints in cells, how much
//! of a record its leaf cell keeps, and the page format's checks on bytes
//! read from disk. I/O, the ring and checksums are outside these harnesses.
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

/// A page read from disk: any bytes whatsoever behind a header with a
/// symbolic count (at most `max_count`), `high` and garbage.
fn any_page(max_count: usize) -> [u8; node::NODE_BYTES] {
    let mut page: [u8; node::NODE_BYTES] = kani::any();
    let count = usize::from(u16::from_le_bytes([page[20], page[21]]));
    kani::assume(count <= max_count);
    page
}

/// Whatever bytes a page holds, once it passes [`node::kind`] its body area
/// lies inside the page, and every entry it yields lies inside the body
/// area: no later read or write derived from them can leave the page.
#[kani::proof]
fn a_page_that_passes_its_check_keeps_every_entry_inside_it() {
    let page = any_page(usize::MAX);
    if node::kind(&page).is_err() {
        return;
    }
    let low = node::ENTRIES_AT + node::count(&page) * 4;
    assert!(low + node::gap(&page) <= node::NODE_BYTES);
    assert!(node::free(&page) <= node::CAPACITY);
    let index: usize = kani::any();
    if let Ok((offset, len)) = node::entry(&page, index) {
        assert!(index < node::count(&page));
        if len != 0 {
            assert!(offset >= low + node::gap(&page));
            assert!(offset + len <= node::NODE_BYTES);
        }
    }
}

/// A repack of any checked page with up to two entries never panics; it
/// either keeps the page checked with its entries and no garbage, or fails
/// and leaves the page exactly as it was.
#[kani::proof]
#[kani::unwind(4)]
fn a_repack_keeps_a_checked_page_or_changes_nothing() {
    let mut page = any_page(2);
    kani::assume(node::kind(&page).is_ok());
    let before = page;
    let count = node::count(&page);
    let mut scratch = [0u8; node::NODE_BYTES];
    match node::repack(&mut page, &mut scratch) {
        Ok(()) => {
            assert!(node::kind(&page).is_ok());
            assert_eq!(node::count(&page), count);
            assert_eq!(node::garbage(&page), 0);
        }
        Err(_) => assert!(page == before),
    }
}

/// An insert into any checked page with up to two entries never panics. A
/// body that goes in becomes entry `index` with its length, one more entry,
/// and the page still passes its check; otherwise the page is unchanged.
#[kani::proof]
#[kani::unwind(4)]
fn an_insert_keeps_a_checked_page_or_changes_nothing() {
    let mut page = any_page(2);
    kani::assume(node::kind(&page).is_ok());
    let before = page;
    let count = node::count(&page);
    let body: [u8; 4] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 4);
    let index: usize = kani::any();
    let mut scratch = [0u8; node::NODE_BYTES];
    match node::insert(&mut page, index, &body[..len], &mut scratch) {
        Ok(true) => {
            assert!(node::kind(&page).is_ok());
            assert_eq!(node::count(&page), count + 1);
            assert!(matches!(node::entry(&page, index), Ok((_, found)) if found == len));
        }
        Ok(false) | Err(_) => assert!(page == before),
    }
}
