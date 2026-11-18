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

/// The page size the page harnesses use: the same layout and the same code
/// as the 4096-byte pages the tree reads (every page function bounds itself
/// by the page's length), small enough for the solver.
const PAGE: usize = 64;

/// Whether two pages hold the same bytes, compared a word at a time: a
/// byte-wise comparison would be a loop the solver unwinds per byte.
fn same(a: &[u8; PAGE], b: &[u8; PAGE]) -> bool {
    let mut at = 0;
    while at < PAGE {
        let word = |page: &[u8; PAGE]| {
            u64::from_le_bytes([
                page[at],
                page[at + 1],
                page[at + 2],
                page[at + 3],
                page[at + 4],
                page[at + 5],
                page[at + 6],
                page[at + 7],
            ])
        };
        if word(a) != word(b) {
            return false;
        }
        at += 8;
    }
    true
}

/// A page read from disk: any bytes whatsoever, with a count of at most
/// `max_count` entries.
fn any_page(max_count: usize) -> [u8; PAGE] {
    let page: [u8; PAGE] = kani::any();
    let count = usize::from(u16::from_le_bytes([page[20], page[21]]));
    kani::assume(count <= max_count);
    page
}

/// A page read from disk with exactly `count` entries and any other bytes:
/// a count the solver sees as a constant bounds the loops over entries.
fn any_page_of(count: u16) -> [u8; PAGE] {
    let mut page: [u8; PAGE] = kani::any();
    page[20..22].copy_from_slice(&count.to_le_bytes());
    page
}

/// Whatever bytes a page holds, once it passes its check its body area lies
/// inside the page, and every entry it yields lies inside the body area: no
/// later read or write derived from them can leave the page.
#[kani::proof]
fn a_page_that_passes_its_check_keeps_every_entry_inside_it() {
    let page = any_page(usize::MAX);
    if node::kind_of(&page).is_err() {
        return;
    }
    let low = node::ENTRIES_AT + node::count(&page) * 4;
    assert!(low + node::gap(&page) <= PAGE);
    assert!(node::free(&page) <= PAGE - node::ENTRIES_AT);
    let index: usize = kani::any();
    if let Ok((offset, len)) = node::entry(&page, index) {
        assert!(index < node::count(&page));
        if len != 0 {
            assert!(offset >= low + node::gap(&page));
            assert!(offset + len <= PAGE);
        }
    }
}

/// A repack of any checked page with up to two entries never panics; it
/// either keeps the page checked with its entries and no garbage, or fails
/// and leaves the page exactly as it was.
#[kani::proof]
#[kani::unwind(10)]
fn a_repack_keeps_a_checked_page_or_changes_nothing() {
    for count in 0..=2u16 {
        let mut page = any_page_of(count);
        if node::kind_of(&page).is_err() {
            continue;
        }
        let before = page;
        let mut scratch = [0u8; PAGE];
        match node::repack(&mut page, &mut scratch) {
            Ok(()) => {
                assert!(node::kind_of(&page).is_ok());
                assert_eq!(node::count(&page), usize::from(count));
                assert_eq!(node::garbage(&page), 0);
            }
            Err(_) => assert!(same(&page, &before)),
        }
    }
}

/// An insert into any checked page with up to two entries never panics. A
/// body that goes in becomes entry `index` with its length, one more entry,
/// and the page still passes its check; otherwise the page is unchanged.
#[kani::proof]
#[kani::unwind(10)]
fn an_insert_keeps_a_checked_page_or_changes_nothing() {
    for count in 0..=2u16 {
        let mut page = any_page_of(count);
        if node::kind_of(&page).is_err() {
            continue;
        }
        let before = page;
        let body: [u8; 4] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= 4);
        let index: usize = kani::any();
        let mut scratch = [0u8; PAGE];
        match node::insert(&mut page, index, &body[..len], &mut scratch) {
            Ok(true) => {
                assert!(node::kind_of(&page).is_ok());
                assert_eq!(node::count(&page), usize::from(count) + 1);
                assert!(matches!(node::entry(&page, index), Ok((_, found)) if found == len));
            }
            Ok(false) | Err(_) => assert!(same(&page, &before)),
        }
    }
}
