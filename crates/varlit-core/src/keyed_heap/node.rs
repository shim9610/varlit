//! Tree pages: leaves of records in key order and interior pages of
//! separators, each a 64 KiB pool page (see `docs/spec.md`, Keyed Storage).
//!
//! A page is `link (16 bytes) | header (16 bytes) | entries | free | bodies`:
//! `count` four-byte entries (`offset u16 | length u16`, sorted by key) grow
//! up from byte 32; bodies grow down from the end. Removing a record leaves
//! its body as garbage until the page is repacked. Everything here works on
//! a page's bytes in memory and checks every offset it reads, so a damaged
//! page is an error, never a panic.

use super::corrupt;
use crate::Result;
use core::cmp::Ordering;

/// The page size: a pool page.
pub(crate) const NODE_BYTES: usize = 64 << 10;
/// Where the header starts: past the pool link.
const HEADER_AT: usize = 16;
pub(crate) const ENTRIES_AT: usize = 32;
const ENTRY_LEN: usize = 4;

pub(crate) const LEAF_MAGIC: &[u8; 4] = b"VKLF";
pub(crate) const INTERIOR_MAGIC: &[u8; 4] = b"VKIN";

/// What a page holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Leaf,
    Interior,
}

/// Lays out an empty page of `kind`.
pub(crate) fn init(page: &mut [u8], kind: Kind) {
    page[HEADER_AT..].fill(0);
    page[HEADER_AT..HEADER_AT + 4].copy_from_slice(match kind {
        Kind::Leaf => LEAF_MAGIC,
        Kind::Interior => INTERIOR_MAGIC,
    });
    set_u16(page, HEADER_AT + 4, 0);
    set_high(page, NODE_BYTES);
    set_u16(page, HEADER_AT + 8, 0);
}

fn u16_at(page: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([page[at], page[at + 1]]))
}

/// Every value a page stores in 16 bits is below `NODE_BYTES`.
fn set_u16(page: &mut [u8], at: usize, value: usize) {
    debug_assert!(value < NODE_BYTES);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "offsets, lengths and counts are below the 64 KiB page"
    )]
    let value = value as u16;
    page[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// The start of the bodies; `NODE_BYTES` is stored as 0.
fn high(page: &[u8]) -> usize {
    match u16_at(page, HEADER_AT + 6) {
        0 => NODE_BYTES,
        high => high,
    }
}

fn set_high(page: &mut [u8], high: usize) {
    set_u16(
        page,
        HEADER_AT + 6,
        if high == NODE_BYTES { 0 } else { high },
    );
}

/// Checks a page's header and returns its kind.
#[cfg(test)]
pub(crate) fn kind(page: &[u8]) -> Result<Kind> {
    if page.len() != NODE_BYTES {
        return Err(corrupt("keyed tree page"));
    }
    let kind = match &page[HEADER_AT..HEADER_AT + 4] {
        magic if magic == LEAF_MAGIC => Kind::Leaf,
        magic if magic == INTERIOR_MAGIC => Kind::Interior,
        _ => return Err(corrupt("keyed tree page")),
    };
    let low = ENTRIES_AT + count(page) * ENTRY_LEN;
    if low > high(page) || garbage(page) > NODE_BYTES - high(page) {
        return Err(corrupt("keyed tree page"));
    }
    Ok(kind)
}

pub(crate) fn count(page: &[u8]) -> usize {
    u16_at(page, HEADER_AT + 4)
}

/// Bytes of removed bodies not yet reclaimed.
pub(crate) fn garbage(page: &[u8]) -> usize {
    u16_at(page, HEADER_AT + 8)
}

/// Bytes between the entries and the bodies.
pub(crate) fn gap(page: &[u8]) -> usize {
    high(page) - (ENTRIES_AT + count(page) * ENTRY_LEN)
}

/// Bytes a repack would leave free: the gap and the garbage.
pub(crate) fn free(page: &[u8]) -> usize {
    gap(page) + garbage(page)
}

/// The most a page can hold: everything past the header.
pub(crate) const CAPACITY: usize = NODE_BYTES - ENTRIES_AT;

/// Bytes an entry with a body of `len` bytes takes.
pub(crate) const fn cost(len: usize) -> usize {
    ENTRY_LEN + len
}

/// Entry `index`: its body's offset and length, checked to lie in the
/// page's body area.
pub(crate) fn entry(page: &[u8], index: usize) -> Result<(usize, usize)> {
    if index >= count(page) {
        return Err(corrupt("keyed tree entry"));
    }
    let at = ENTRIES_AT + index * ENTRY_LEN;
    let offset = u16_at(page, at);
    let len = u16_at(page, at + 2);
    // An empty body has no place; its offset is stored as 0.
    if len == 0 {
        return Ok((NODE_BYTES, 0));
    }
    if offset < high(page) || offset + len > NODE_BYTES {
        return Err(corrupt("keyed tree entry"));
    }
    Ok((offset, len))
}

pub(crate) fn body(page: &[u8], index: usize) -> Result<&[u8]> {
    let (offset, len) = entry(page, index)?;
    Ok(&page[offset..offset + len])
}

/// Inserts `body` as entry `index`, repacking first when the gap is too
/// small. `false`, changing nothing, when the page cannot hold it.
pub(crate) fn insert(page: &mut [u8], index: usize, body: &[u8], scratch: &mut [u8]) -> bool {
    let n = count(page);
    if index > n || cost(body.len()) > free(page) {
        return false;
    }
    if cost(body.len()) > gap(page) {
        repack(page, scratch);
    }
    let offset = high(page) - body.len();
    page[offset..offset + body.len()].copy_from_slice(body);
    set_high(page, offset);
    let at = ENTRIES_AT + index * ENTRY_LEN;
    page.copy_within(at..ENTRIES_AT + n * ENTRY_LEN, at + ENTRY_LEN);
    set_u16(page, at, if body.is_empty() { 0 } else { offset });
    set_u16(page, at + 2, body.len());
    set_u16(page, HEADER_AT + 4, n + 1);
    true
}

/// Removes entry `index`; its body becomes garbage.
pub(crate) fn remove(page: &mut [u8], index: usize) -> Result<()> {
    let (offset, len) = entry(page, index)?;
    let n = count(page);
    let at = ENTRIES_AT + index * ENTRY_LEN;
    page.copy_within(at + ENTRY_LEN..ENTRIES_AT + n * ENTRY_LEN, at);
    set_u16(page, HEADER_AT + 4, n - 1);
    if len == 0 {
    } else if offset == high(page) {
        set_high(page, offset + len);
    } else {
        set_u16(page, HEADER_AT + 8, garbage(page) + len);
    }
    Ok(())
}

/// Replaces entry `index`'s body. `false`, changing nothing, when the page
/// cannot hold the new one.
pub(crate) fn replace(
    page: &mut [u8],
    index: usize,
    body: &[u8],
    scratch: &mut [u8],
) -> Result<bool> {
    let (offset, len) = entry(page, index)?;
    if body.len() == len {
        page[offset..offset + len].copy_from_slice(body);
        return Ok(true);
    }
    if body.len() > free(page) + len {
        return Ok(false);
    }
    remove(page, index)?;
    let inserted = insert(page, index, body, scratch);
    debug_assert!(inserted, "room was checked");
    Ok(inserted)
}

/// Moves every body to the end of the page, in entry order, and clears the
/// garbage. `scratch` is a page-sized buffer.
pub(crate) fn repack(page: &mut [u8], scratch: &mut [u8]) {
    let n = count(page);
    scratch[..NODE_BYTES].copy_from_slice(&page[..NODE_BYTES]);
    let mut high = NODE_BYTES;
    for index in 0..n {
        let at = ENTRIES_AT + index * ENTRY_LEN;
        let offset = u16_at(scratch, at);
        let len = u16_at(scratch, at + 2);
        if len == 0 {
            continue;
        }
        high -= len;
        page[high..high + len].copy_from_slice(&scratch[offset..offset + len]);
        set_u16(page, at, high);
    }
    set_high(page, high);
    set_u16(page, HEADER_AT + 8, 0);
}

/// The first entry for which `probe` is not `Less`: the insertion point of a
/// key `probe` compares entries against (`probe(i)` is entry `i` compared
/// with the key). Entries are sorted, so this is a binary search.
pub(crate) fn search_entries(
    n: usize,
    probe: &mut dyn FnMut(usize) -> Result<Ordering>,
) -> Result<(usize, bool)> {
    let (mut low, mut high) = (0usize, n);
    while low < high {
        let mid = low + (high - low) / 2;
        match probe(mid)? {
            Ordering::Less => low = mid + 1,
            Ordering::Equal => return Ok((mid, true)),
            Ordering::Greater => high = mid,
        }
    }
    Ok((low, false))
}

/// The length of the shortest prefix of `right` that is greater than
/// `left`, given `left < right`: a separator `S` with `left < S <= right`.
/// The writer finds the same length streaming keys (`tree::common_prefix`).
#[cfg(any(test, kani))]
pub(crate) fn separator_len(left: &[u8], right: &[u8]) -> usize {
    let common = left.iter().zip(right).take_while(|(a, b)| a == b).count();
    (common + 1).min(right.len())
}

// ---- varints ----

/// Writes `value` as a LEB128 varint at the start of `out`; returns its
/// length (at most ten bytes).
pub(crate) fn write_varint(out: &mut [u8], mut value: u64) -> usize {
    let mut at = 0;
    loop {
        #[allow(clippy::cast_possible_truncation, reason = "seven bits")]
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out[at] = byte;
            return at + 1;
        }
        out[at] = byte | 0x80;
        at += 1;
    }
}

/// Reads a LEB128 varint: `(value, length)`. Refuses overlong encodings and
/// values past 64 bits.
pub(crate) fn read_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut value = 0u64;
    for (at, &byte) in bytes.iter().enumerate().take(10) {
        let bits = u64::from(byte & 0x7f);
        if at == 9 && byte > 1 {
            return Err(corrupt("keyed varint"));
        }
        value |= bits << (7 * at);
        if byte & 0x80 == 0 {
            if at > 0 && byte == 0 {
                return Err(corrupt("keyed varint"));
            }
            return Ok((value, at + 1));
        }
    }
    Err(corrupt("keyed varint"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Random inserts, removals and replacements against a list model: the
    /// page shows the model after every step, and its free bytes add up.
    #[test]
    fn a_page_matches_a_list_model() {
        let mut page = vec![0u8; NODE_BYTES];
        let mut scratch = vec![0u8; NODE_BYTES];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..20 {
            init(&mut page, Kind::Leaf);
            let mut model: Vec<Vec<u8>> = Vec::new();
            for step in 0..3000 {
                let roll = rng(&mut state) % 10;
                let len = (rng(&mut state) % if round % 2 == 0 { 64 } else { 3000 }) as usize;
                let body: Vec<u8> = (0..len).map(|i| (i as u64 ^ step) as u8).collect();
                if roll < 5 || model.is_empty() {
                    let index = (rng(&mut state) % (model.len() as u64 + 1)) as usize;
                    let fits =
                        cost(len) <= CAPACITY - model.iter().map(|b| cost(b.len())).sum::<usize>();
                    assert_eq!(insert(&mut page, index, &body, &mut scratch), fits);
                    if fits {
                        model.insert(index, body);
                    }
                } else if roll < 8 {
                    let index = (rng(&mut state) % model.len() as u64) as usize;
                    remove(&mut page, index).unwrap();
                    model.remove(index);
                } else {
                    let index = (rng(&mut state) % model.len() as u64) as usize;
                    let used: usize = model.iter().map(|b| cost(b.len())).sum();
                    let fits = used - model[index].len() + len <= CAPACITY;
                    assert_eq!(
                        replace(&mut page, index, &body, &mut scratch).unwrap(),
                        fits
                    );
                    if fits {
                        model[index] = body;
                    }
                }
                assert_eq!(kind(&page).unwrap(), Kind::Leaf);
                assert_eq!(count(&page), model.len());
                for (index, expected) in model.iter().enumerate() {
                    assert_eq!(super::body(&page, index).unwrap(), &expected[..]);
                }
                let used: usize = model.iter().map(|b| cost(b.len())).sum();
                assert_eq!(free(&page), CAPACITY - used, "round {round} step {step}");
            }
        }
    }

    #[test]
    fn search_finds_the_insertion_point() {
        let keys: Vec<u32> = (0..100).map(|k| k * 2).collect();
        for target in 0..201u32 {
            let (at, found) =
                search_entries(keys.len(), &mut |i| Ok(keys[i].cmp(&target))).unwrap();
            assert_eq!(found, target % 2 == 0 && target < 200);
            assert_eq!(at, keys.partition_point(|&k| k < target));
        }
    }

    #[test]
    fn separators_lie_between_their_keys() {
        let pairs: [(&[u8], &[u8]); 5] = [
            (b"a", b"b"),
            (b"abc", b"abd"),
            (b"ab", b"abc"),
            (b"", b"x"),
            (b"zz\xff", b"zz\xff\x00"),
        ];
        for (left, right) in pairs {
            let s = &right[..separator_len(left, right)];
            assert!(left < s && s <= right, "{left:?} {s:?} {right:?}");
        }
    }

    #[test]
    fn varints_round_trip_and_refuse_overlong_forms() {
        let mut buf = [0u8; 10];
        for value in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let len = write_varint(&mut buf, value);
            assert_eq!(read_varint(&buf[..len]).unwrap(), (value, len));
        }
        assert!(read_varint(&[0x80, 0x00]).is_err());
        assert!(read_varint(&[0xff; 10]).is_err());
        assert!(read_varint(&[0x80]).is_err());
    }

    #[test]
    fn damaged_pages_are_errors() {
        let mut page = vec![0u8; NODE_BYTES];
        assert!(kind(&page).is_err());
        init(&mut page, Kind::Interior);
        let mut scratch = vec![0u8; NODE_BYTES];
        assert!(insert(&mut page, 0, b"abc", &mut scratch));
        page[ENTRIES_AT] = 0;
        page[ENTRIES_AT + 1] = 0;
        assert!(entry(&page, 0).is_err());
        assert!(entry(&page, 1).is_err());
    }
}
