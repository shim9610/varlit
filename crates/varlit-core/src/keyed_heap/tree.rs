//! Finding keys in the tree of sorted pages, the same for the writer and a
//! reader: only the [`View`](super::lookup::View) its bytes come through
//! differs. Pages are probed with small reads: a page header, the entries a
//! binary search visits, and the key bytes each comparison needs.
//!
//! A key is the block id (big-endian) then the user's key bytes, so byte
//! order is (block, key) order. A body keeps at most [`KEY_PREFIX`] key
//! bytes in the page; a longer key continues in the item chain the body
//! names, and comparisons stream the rest from there.

use super::lookup::Ctx;
use super::node::{self, ENTRIES_AT, NODE_BYTES};
use super::{ITEM_HEADER, NO_ITEM, corrupt};
use crate::Result;
use core::cmp::Ordering;

/// Key bytes a body keeps in its page.
pub(crate) const KEY_PREFIX: usize = 256;
/// The deepest tree: a fan-out of at least 128 entries (separators of at
/// most `KEY_PREFIX` bytes) reaches every u64 count of leaves well before.
pub(crate) const MAX_HEIGHT: usize = 16;
/// A body's first byte: its record (or separator) continues in a chain.
pub(crate) const FLAG_CHAIN: u8 = 1;
/// Bytes compared per step.
const CHUNK: usize = 512;
/// The most a body's head can take: a child, the flags, a varint length, a
/// key prefix, a total length and a head item.
pub(crate) const HEAD_MAX: usize = 8 + 1 + 10 + KEY_PREFIX + 8 + 8;

/// Where a key's bytes are: `inline` bytes in a page at `at`, then, when
/// the key is longer, the rest in an item chain whose record bytes hold the
/// key from byte `chain_from`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyLoc {
    pub at: u64,
    pub inline: usize,
    pub len: u64,
    pub head: u64,
    pub chain_from: u64,
}

/// A key to compare: in memory, or where an entry keeps it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum KeySrc<'a> {
    Mem(&'a [u8]),
    Loc(KeyLoc),
}

impl KeySrc<'_> {
    fn len(&self) -> u64 {
        match self {
            Self::Mem(bytes) => bytes.len() as u64,
            Self::Loc(loc) => loc.len,
        }
    }

    /// Reads key bytes `[from, from + buf.len())`.
    fn read(&self, ctx: &mut Ctx<'_>, from: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Mem(bytes) => {
                let from = crate::width::index(from)?;
                buf.copy_from_slice(&bytes[from..from + buf.len()]);
                Ok(())
            }
            Self::Loc(loc) => {
                let mut done = 0usize;
                let inline = loc.inline as u64;
                if from < inline {
                    let n = crate::width::bounded(inline - from, buf.len());
                    ctx.read(loc.at + from, &mut buf[..n])?;
                    done = n;
                }
                if done < buf.len() {
                    let start = from + done as u64 - inline + loc.chain_from;
                    read_record(ctx, loc.head, start, &mut buf[done..])?;
                }
                Ok(())
            }
        }
    }
}

/// Compares two keys byte by byte, streaming what is not in memory.
pub(crate) fn compare(ctx: &mut Ctx<'_>, a: &KeySrc<'_>, b: &KeySrc<'_>) -> Result<Ordering> {
    if let (KeySrc::Mem(a), KeySrc::Mem(b)) = (a, b) {
        return Ok(a.cmp(b));
    }
    let common = a.len().min(b.len());
    let mut at = 0u64;
    let mut left = [0u8; CHUNK];
    let mut right = [0u8; CHUNK];
    while at < common {
        let n = crate::width::bounded(common - at, CHUNK);
        a.read(ctx, at, &mut left[..n])?;
        b.read(ctx, at, &mut right[..n])?;
        match left[..n].cmp(&right[..n]) {
            Ordering::Equal => at += n as u64,
            other => return Ok(other),
        }
    }
    Ok(a.len().cmp(&b.len()))
}

/// The length of the common prefix of two keys.
pub(crate) fn common_prefix(ctx: &mut Ctx<'_>, a: &KeySrc<'_>, b: &KeySrc<'_>) -> Result<u64> {
    let common = a.len().min(b.len());
    let mut at = 0u64;
    let mut left = [0u8; CHUNK];
    let mut right = [0u8; CHUNK];
    while at < common {
        let n = crate::width::bounded(common - at, CHUNK);
        a.read(ctx, at, &mut left[..n])?;
        b.read(ctx, at, &mut right[..n])?;
        let same = left[..n]
            .iter()
            .zip(&right[..n])
            .take_while(|(x, y)| x == y)
            .count();
        at += same as u64;
        if same < n {
            break;
        }
    }
    Ok(at)
}

/// Reads `buf.len()` record bytes of the chain headed by `head`, from byte
/// `from` of its record (key then payload), without checking checksums:
/// for comparing keys. Values are read with [`super::lookup::stream`].
pub(crate) fn read_record(ctx: &mut Ctx<'_>, head: u64, from: u64, buf: &mut [u8]) -> Result<()> {
    let mut item = head;
    let mut skipped = 0u64;
    let mut done = 0usize;
    // A chain is no longer than its record: bounded by `from + len`.
    while done < buf.len() {
        if item == NO_ITEM {
            return Err(corrupt("keyed record chain"));
        }
        let header = ctx.item(item)?;
        let len = u64::from(header.len);
        let at = from + done as u64;
        if at < skipped + len {
            let within = at - skipped;
            let n = crate::width::bounded(len - within, buf.len() - done);
            ctx.read_item(item, ITEM_HEADER + within, &mut buf[done..done + n])?;
            done += n;
        }
        skipped += len;
        item = header.next;
    }
    Ok(())
}

// ---- bodies ----

/// A leaf body's record, or an interior body's separator, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Body {
    /// An interior body's child page.
    pub child: u64,
    pub key: KeyLoc,
    /// An inline record's value: offset and length in the page.
    pub value_at: u64,
    pub value_len: u64,
    /// A chained record's total length (key then payload) and head; a
    /// chained separator's head is `key.head`.
    pub total: u64,
}

impl Body {
    pub(crate) const fn chained(&self) -> bool {
        self.key.head != NO_ITEM
    }
}

/// Parses a body whose first bytes are `head` (at most [`HEAD_MAX`]),
/// starting at file offset `at` and `len` bytes long.
pub(crate) fn parse(head: &[u8], at: u64, len: usize, interior: bool) -> Result<Body> {
    let bad = || corrupt("keyed tree body");
    let mut pos = 0usize;
    let mut child = 0u64;
    if interior {
        child = u64::from_le_bytes(head.get(..8).ok_or_else(bad)?.try_into().expect("eight"));
        pos = 8;
    }
    let flags = *head.get(pos).ok_or_else(bad)?;
    pos += 1;
    let (key_len, n) = node::varint(head.get(pos..).ok_or_else(bad)?)?;
    pos += n;
    if flags & !FLAG_CHAIN != 0 {
        return Err(bad());
    }
    if flags & FLAG_CHAIN == 0 {
        let key_len = crate::width::index(key_len)?;
        let end = pos.checked_add(key_len).ok_or_else(bad)?;
        if end > len {
            return Err(bad());
        }
        return Ok(Body {
            child,
            key: KeyLoc {
                at: at + pos as u64,
                inline: key_len,
                len: key_len as u64,
                head: NO_ITEM,
                chain_from: 0,
            },
            value_at: at + end as u64,
            value_len: (len - end) as u64,
            total: 0,
        });
    }
    let inline = crate::width::bounded(key_len, KEY_PREFIX);
    if key_len < 4 || pos + inline + 16 != len {
        return Err(bad());
    }
    let word = |from: usize| -> Result<u64> {
        Ok(u64::from_le_bytes(
            head.get(from..from + 8).ok_or_else(bad)?.try_into().expect("eight"),
        ))
    };
    let total = word(pos + inline)?;
    let item = super::checked_ref(word(pos + inline + 8)?)?;
    Ok(Body {
        child,
        key: KeyLoc {
            at: at + pos as u64,
            inline,
            len: key_len,
            head: item,
            // The chain's record holds the key without the block id.
            chain_from: inline as u64 - 4,
        },
        value_at: 0,
        value_len: 0,
        total,
    })
}

/// Reads and parses entry `index` of the page at `page`.
pub(crate) fn read_body(
    ctx: &mut Ctx<'_>,
    page: u64,
    count: usize,
    index: usize,
    interior: bool,
) -> Result<Body> {
    if index >= count {
        return Err(corrupt("keyed tree entry"));
    }
    let mut entry = [0u8; 4];
    ctx.read(page + (ENTRIES_AT + index * 4) as u64, &mut entry)?;
    let offset = usize::from(u16::from_le_bytes([entry[0], entry[1]]));
    let len = usize::from(u16::from_le_bytes([entry[2], entry[3]]));
    if len == 0 || offset < ENTRIES_AT + count * 4 || offset + len > NODE_BYTES {
        return Err(corrupt("keyed tree entry"));
    }
    let mut head = [0u8; HEAD_MAX];
    let n = len.min(HEAD_MAX);
    ctx.read(page + offset as u64, &mut head[..n])?;
    parse(&head[..n], page + offset as u64, len, interior)
}

/// A page's kind and entry count.
pub(crate) fn read_header(ctx: &mut Ctx<'_>, page: u64) -> Result<(node::Kind, usize)> {
    let mut header = [0u8; ENTRIES_AT];
    ctx.read(page, &mut header)?;
    let kind = match &header[16..20] {
        magic if magic == node::LEAF_MAGIC => node::Kind::Leaf,
        magic if magic == node::INTERIOR_MAGIC => node::Kind::Interior,
        _ => return Err(corrupt("keyed tree page")),
    };
    let count = usize::from(u16::from_le_bytes([header[20], header[21]]));
    if ENTRIES_AT + count * 4 > NODE_BYTES {
        return Err(corrupt("keyed tree page"));
    }
    Ok((kind, count))
}

// ---- paths ----

/// The pages from the root to a key's leaf: `pages[level]` and, below the
/// leaf, the entry taken at each level (`index[0]` is the key's entry or
/// insertion point in the leaf).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Path {
    pub pages: [u64; MAX_HEIGHT],
    pub index: [usize; MAX_HEIGHT],
    pub counts: [usize; MAX_HEIGHT],
    pub height: usize,
    pub found: bool,
}

/// Searches the page at `page` for `key`: `(index, found)` as
/// [`node::search`] gives them.
fn search_page(
    ctx: &mut Ctx<'_>,
    page: u64,
    count: usize,
    key: &KeySrc<'_>,
    interior: bool,
) -> Result<(usize, bool)> {
    node::search(count, &mut |index| {
        if interior && index == 0 {
            // The first child has no lower bound.
            return Ok(Ordering::Less);
        }
        let body = read_body(ctx, page, count, index, interior)?;
        compare(ctx, &KeySrc::Loc(body.key), key)
    })
}

/// Finds `key` in the tree with root `root` (`(page, height)`).
pub(crate) fn find(ctx: &mut Ctx<'_>, root: (u64, u64), key: &KeySrc<'_>) -> Result<Path> {
    let height = crate::width::index(root.1)?;
    if root.0 == 0 || height == 0 || height > MAX_HEIGHT {
        return Err(corrupt("keyed tree root"));
    }
    let mut path = Path {
        pages: [0; MAX_HEIGHT],
        index: [0; MAX_HEIGHT],
        counts: [0; MAX_HEIGHT],
        height,
        found: false,
    };
    let mut page = root.0;
    for level in (0..height).rev() {
        let (kind, count) = read_header(ctx, page)?;
        let interior = level > 0;
        if (kind == node::Kind::Interior) != interior || (interior && count == 0) {
            return Err(corrupt("keyed tree page"));
        }
        path.pages[level] = page;
        path.counts[level] = count;
        let (index, found) = search_page(ctx, page, count, key, interior)?;
        if interior {
            let child = if found { index } else { index - 1 };
            path.index[level] = child;
            page = read_body(ctx, page, count, child, true)?.child;
            if page == 0 {
                return Err(corrupt("keyed tree child"));
            }
        } else {
            path.index[0] = index;
            path.found = found;
        }
    }
    Ok(path)
}

/// Reads the payload of the record `body` describes into `out`, which must
/// be its length. A chained record is checked item by item.
pub(crate) fn read_value(ctx: &mut Ctx<'_>, body: &Body, out: &mut [u8]) -> Result<()> {
    if !body.chained() {
        if out.len() as u64 != body.value_len {
            return Err(corrupt("keyed record length"));
        }
        return ctx.read(body.value_at, out);
    }
    let user_key = body.key.len - 4;
    let mut skip = user_key;
    let mut written = 0usize;
    super::lookup::stream(ctx, body.key.head, &mut |bytes| {
        let mut bytes = bytes;
        if skip > 0 {
            let n = crate::width::bounded(skip, bytes.len());
            skip -= n as u64;
            bytes = &bytes[n..];
        }
        let end = written + bytes.len();
        let target = out
            .get_mut(written..end)
            .ok_or(corrupt("keyed record length"))?;
        target.copy_from_slice(bytes);
        written = end;
        Ok(true)
    })?;
    if written != out.len() {
        return Err(corrupt("keyed record length"));
    }
    Ok(())
}

/// The payload length of the record `body` describes.
pub(crate) const fn value_len(body: &Body) -> u64 {
    if body.chained() {
        body.total - (body.key.len - 4)
    } else {
        body.value_len
    }
}
