//! Finding keys in the B-tree, the same for the writer and a reader: only
//! the [`View`](super::lookup::View) its bytes come through differs. Each
//! page of a path is read whole (4 KiB) and searched in memory.
//!
//! A key is the user's key bytes; each block type has its own tree. A
//! leaf cell holds a record (key then value):
//! whole when it is short, else its first bytes, the rest in a chain of
//! overflow pages. An interior cell holds a child and a separator, whose
//! bytes past [`SEP_LOCAL`] are in an overflow chain too.

use super::lookup::Ctx;
use super::node::{self, NODE_BYTES};
use super::{NO_PAGE, corrupt};
use crate::Result;
use core::cmp::Ordering;

/// The deepest tree: an interior page holds at least a dozen entries.
pub(crate) const MAX_HEIGHT: usize = 32;
/// A cell's first byte: its record or separator continues in overflow pages.
pub(crate) const FLAG_OVERFLOW: u8 = 1;
/// Record bytes a leaf cell holds whole; past it, a cell keeps between
/// `MIN_LOCAL` and `MAX_LOCAL` bytes, chosen so the overflow pages are full.
pub(crate) const MAX_LOCAL: u64 = 1024;
pub(crate) const MIN_LOCAL: u64 = 256;
/// Separator bytes an interior cell holds.
pub(crate) const SEP_LOCAL: u64 = 256;

/// An overflow page: link (16) | magic | used u16 | zero u16 | next u64 |
/// data.
pub(crate) const OVERFLOW_MAGIC: &[u8; 4] = b"VKOV";
pub(crate) const OVERFLOW_AT: usize = 32;
pub(crate) const OVERFLOW_DATA: u64 = (NODE_BYTES - OVERFLOW_AT) as u64;
/// Bytes compared per step.
const CHUNK: usize = 512;

/// Bytes of a record of `total` bytes its leaf cell holds.
pub(crate) const fn local_len(total: u64) -> u64 {
    if total <= MAX_LOCAL {
        return total;
    }
    let local = MIN_LOCAL + (total - MIN_LOCAL) % OVERFLOW_DATA;
    if local <= MAX_LOCAL { local } else { MIN_LOCAL }
}

/// Bytes of a separator of `len` bytes its interior cell holds.
pub(crate) const fn sep_local_len(len: u64) -> u64 {
    if len <= SEP_LOCAL { len } else { SEP_LOCAL }
}

/// Where a key's bytes are: the cell's `local` record bytes at file offset
/// `at`, then the overflow chain from `overflow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyLoc {
    pub at: u64,
    pub local: u64,
    pub len: u64,
    pub overflow: u64,
}

/// A key to compare: key bytes in memory, where a cell
/// keeps it, or a cell held in memory.
#[derive(Clone, Copy, Debug)]
pub(crate) enum KeySrc<'a> {
    Parts(&'a [u8]),
    Loc(KeyLoc),
    Held {
        local: &'a [u8],
        len: u64,
        overflow: u64,
    },
}

impl KeySrc<'_> {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Parts(key) => key.len() as u64,
            Self::Loc(loc) => loc.len,
            Self::Held { len, .. } => *len,
        }
    }

    /// Reads key bytes `[from, from + buf.len())`.
    pub(crate) fn read(&self, ctx: &mut Ctx<'_>, from: u64, buf: &mut [u8]) -> Result<()> {
        if from + buf.len() as u64 > self.len() {
            return Err(corrupt("keyed key range"));
        }
        match self {
            Self::Parts(key) => {
                let start = crate::width::index(from)?;
                buf.copy_from_slice(&key[start..start + buf.len()]);
                Ok(())
            }
            Self::Loc(loc) => read_record(ctx, loc.at, loc.local, loc.overflow, from, buf),
            Self::Held {
                local, overflow, ..
            } => {
                let start = crate::width::index(from)?;
                let n = local.len().saturating_sub(start).min(buf.len());
                if n > 0 {
                    buf[..n].copy_from_slice(&local[start..start + n]);
                }
                if n < buf.len() {
                    let at = (start + n - local.len()) as u64;
                    read_overflow(ctx, *overflow, at, &mut buf[n..])?;
                }
                Ok(())
            }
        }
    }
}

impl<'a> KeySrc<'a> {
    /// The whole key, when it is all in memory.
    fn whole(&self) -> Option<&'a [u8]> {
        match *self {
            Self::Parts(key) => Some(key),
            Self::Held { local, len, .. } => local.get(..crate::width::index(len).ok()?),
            Self::Loc(_) => None,
        }
    }
}

/// Compares two keys byte by byte, streaming what is not in memory.
pub(crate) fn compare(ctx: &mut Ctx<'_>, a: &KeySrc<'_>, b: &KeySrc<'_>) -> Result<Ordering> {
    if let (Some(a), Some(b)) = (a.whole(), b.whole()) {
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
    if let (Some(a), Some(b)) = (a.whole(), b.whole()) {
        return Ok(a.iter().zip(b).take_while(|(x, y)| x == y).count() as u64);
    }
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

/// Reads record bytes `[from, from + buf.len())` of a cell whose `local`
/// bytes are at `at` and whose rest is the overflow chain from `overflow`.
pub(crate) fn read_record(
    ctx: &mut Ctx<'_>,
    at: u64,
    local: u64,
    overflow: u64,
    from: u64,
    buf: &mut [u8],
) -> Result<()> {
    let mut done = 0usize;
    if from < local {
        let n = crate::width::bounded(local - from, buf.len());
        ctx.read(at + from, &mut buf[..n])?;
        done = n;
    }
    if done < buf.len() {
        read_overflow(ctx, overflow, from + done as u64 - local, &mut buf[done..])?;
    }
    Ok(())
}

/// An overflow page's header: its used bytes and the next page.
fn overflow_header(ctx: &mut Ctx<'_>, page: u64) -> Result<(u64, u64)> {
    if page == NO_PAGE {
        return Err(corrupt("keyed overflow chain"));
    }
    let mut header = [0u8; OVERFLOW_AT - 16];
    ctx.read(page + 16, &mut header)?;
    let used = u64::from(u16::from_le_bytes([header[4], header[5]]));
    if &header[..4] != OVERFLOW_MAGIC || used == 0 || used > OVERFLOW_DATA {
        return Err(corrupt("keyed overflow page"));
    }
    Ok((
        used,
        u64::from_le_bytes(header[8..16].try_into().expect("eight")),
    ))
}

/// Reads bytes `[from, from + buf.len())` of the overflow chain from
/// `head`. Every page but the last is full.
pub(crate) fn read_overflow(ctx: &mut Ctx<'_>, head: u64, from: u64, buf: &mut [u8]) -> Result<()> {
    let mut page = head;
    let mut skipped = 0u64;
    let mut done = 0usize;
    // The chain is no longer than the record: bounded by `from + len`.
    while done < buf.len() {
        let (used, next) = overflow_header(ctx, page)?;
        let at = from + done as u64;
        if at < skipped + used {
            let within = at - skipped;
            let n = crate::width::bounded(used - within, buf.len() - done);
            ctx.read(page + OVERFLOW_AT as u64 + within, &mut buf[done..done + n])?;
            done += n;
        }
        skipped += used;
        if done < buf.len() {
            page = next;
        }
    }
    Ok(())
}

// ---- cells ----

/// A cell as read: a leaf's record or an interior page's child and
/// separator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Body {
    pub child: u64,
    pub key: KeyLoc,
    pub value_len: u64,
}

impl Body {
    pub(crate) const fn overflowed(&self) -> bool {
        self.key.overflow != NO_PAGE
    }
}

/// Parses a cell of `len` bytes at file offset `at` whose bytes are `cell`.
pub(crate) fn parse(cell: &[u8], at: u64, interior: bool) -> Result<Body> {
    let bad = || corrupt("keyed tree cell");
    let len = cell.len();
    let mut pos = 0usize;
    let mut child = 0u64;
    if interior {
        child = u64::from_le_bytes(cell.get(..8).ok_or_else(bad)?.try_into().expect("eight"));
        pos = 8;
    }
    let flags = *cell.get(pos).ok_or_else(bad)?;
    pos += 1;
    let (key_len, n) = node::read_varint(cell.get(pos..).ok_or_else(bad)?)?;
    pos += n;
    let mut value_len = 0u64;
    if !interior {
        let (value, n) = node::read_varint(cell.get(pos..).ok_or_else(bad)?)?;
        pos += n;
        value_len = value;
    }
    let total = key_len.checked_add(value_len).ok_or_else(bad)?;
    let local = if interior {
        sep_local_len(key_len)
    } else {
        local_len(total)
    };
    let overflowed = local < total;
    if flags & !FLAG_OVERFLOW != 0 || (flags & FLAG_OVERFLOW != 0) != overflowed {
        return Err(bad());
    }
    let local_bytes = crate::width::index(local)?;
    let end = pos + local_bytes + if overflowed { 8 } else { 0 };
    if end != len {
        return Err(bad());
    }
    let overflow = if overflowed {
        let page = u64::from_le_bytes(cell[end - 8..end].try_into().expect("eight"));
        if page == NO_PAGE {
            return Err(bad());
        }
        page
    } else {
        NO_PAGE
    };
    Ok(Body {
        child,
        key: KeyLoc {
            at: at + pos as u64,
            local,
            len: key_len,
            overflow,
        },
        value_len,
    })
}

/// Cell `index` of a page held in memory at file offset `at`.
pub(crate) fn held_body(page: &[u8], at: u64, index: usize, interior: bool) -> Result<Body> {
    let (offset, len) = node::entry(page, index)?;
    parse(&page[offset..offset + len], at + offset as u64, interior)
}

/// The key of cell `index` of a page held in memory, when the cell keeps
/// it whole (no overflow); `None` sends the caller to [`held_body`], which
/// checks the cell.
fn inline_key(page: &[u8], index: usize, interior: bool) -> Option<&[u8]> {
    let (offset, len) = node::entry(page, index).ok()?;
    let cell = page.get(offset..offset + len)?;
    let mut pos = if interior { 8 } else { 0 };
    if *cell.get(pos)? != 0 {
        return None;
    }
    pos += 1;
    let (key_len, n) = node::read_varint(cell.get(pos..)?).ok()?;
    pos += n;
    if !interior {
        pos += node::read_varint(cell.get(pos..)?).ok()?.1;
    }
    cell.get(pos..pos + usize::try_from(key_len).ok()?)
}

/// Searches a page held in memory (at file offset `at`) for `key`:
/// `(index, found)` as [`node::search_entries`] gives them.
pub(crate) fn search_held(
    ctx: &mut Ctx<'_>,
    page: &[u8],
    at: u64,
    key: &KeySrc<'_>,
    interior: bool,
) -> Result<(usize, bool)> {
    let count = node::count(page);
    node::search_entries(count, &mut |index| {
        if interior && index == 0 {
            // The first child has no lower bound.
            return Ok(Ordering::Less);
        }
        // A cell with no overflow keeps its key whole right after its
        // lengths: compare it in place without parsing the rest.
        if let Some(wanted) = key.whole()
            && let Some(stored) = inline_key(page, index, interior)
        {
            return Ok(stored.cmp(wanted));
        }
        let body = held_body(page, at, index, interior)?;
        let start = crate::width::index(body.key.at - at)?;
        let local = crate::width::index(body.key.local.min(body.key.len))?;
        let held = KeySrc::Held {
            local: &page[start..start + local],
            len: body.key.len,
            overflow: body.key.overflow,
        };
        compare(ctx, &held, key)
    })
}

// ---- paths ----

/// The pages from the root to a key's leaf: `pages[level]`, the entry taken
/// at each interior level, and `index[0]`, the key's entry or insertion
/// point in the leaf.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Path {
    pub pages: [u64; MAX_HEIGHT],
    pub index: [usize; MAX_HEIGHT],
    pub counts: [usize; MAX_HEIGHT],
    pub height: usize,
    pub found: bool,
}

impl Path {
    pub(crate) fn new(height: usize) -> Self {
        Self {
            pages: [0; MAX_HEIGHT],
            index: [0; MAX_HEIGHT],
            counts: [0; MAX_HEIGHT],
            height,
            found: false,
        }
    }
}

/// Finds `key` in the tree with root `root` (`(page, height)`), reading
/// each page of the path whole into `page` (a page-sized buffer). Returns
/// the path and, when found, the leaf cell.
pub(crate) fn find(
    ctx: &mut Ctx<'_>,
    root: (u64, u64),
    key: &KeySrc<'_>,
    page: &mut [u8],
) -> Result<(Path, Option<Body>)> {
    let height = crate::width::index(root.1)?;
    if root.0 == NO_PAGE || height == 0 || height > MAX_HEIGHT {
        return Err(corrupt("keyed tree root"));
    }
    let page = page
        .get_mut(..NODE_BYTES)
        .ok_or(corrupt("keyed tree page buffer"))?;
    let mut path = Path::new(height);
    let mut at = root.0;
    for level in (0..height).rev() {
        ctx.read(at, page)?;
        let found = step(ctx, page, at, key, level, &mut path)?;
        if level == 0 {
            return Ok((path, found));
        }
        at = path.pages[level - 1];
    }
    Ok((path, None))
}

/// One level of a search: checks the page held in `page` (at file offset
/// `at`, at `level` of `path`), records it, and returns the leaf cell found
/// (level 0), or puts the child to follow in `path.pages[level - 1]`.
pub(crate) fn step(
    ctx: &mut Ctx<'_>,
    page: &[u8],
    at: u64,
    key: &KeySrc<'_>,
    level: usize,
    path: &mut Path,
) -> Result<Option<Body>> {
    let kind = node::kind(page)?;
    let interior = level > 0;
    let count = node::count(page);
    if (kind == node::Kind::Interior) != interior || (interior && count == 0) {
        return Err(corrupt("keyed tree page"));
    }
    path.pages[level] = at;
    path.counts[level] = count;
    let (index, found) = search_held(ctx, page, at, key, interior)?;
    if interior {
        let child = if found { index } else { index - 1 };
        path.index[level] = child;
        let next = held_body(page, at, child, true)?.child;
        if next == NO_PAGE {
            return Err(corrupt("keyed tree child"));
        }
        path.pages[level - 1] = next;
        return Ok(None);
    }
    path.index[0] = index;
    path.found = found;
    if found {
        return Ok(Some(held_body(page, at, index, false)?));
    }
    Ok(None)
}

/// Reads the value of the record `body` describes into `out`, which must be
/// its length.
pub(crate) fn read_value(ctx: &mut Ctx<'_>, body: &Body, out: &mut [u8]) -> Result<()> {
    if out.len() as u64 != body.value_len {
        return Err(corrupt("keyed record length"));
    }
    read_record(
        ctx,
        body.key.at,
        body.key.local,
        body.key.overflow,
        body.key.len,
        out,
    )
}

// ---- range scans ----

/// What a scan calls with each record: its leaf page, the page's file
/// offset and the record's cell; `false` stops the scan.
pub(crate) type Visit<'v> = dyn FnMut(&mut Ctx<'_>, &[u8], u64, &Body) -> Result<bool> + 'v;

/// Where a scan stands: the path to its leaf and the next entry there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    path: Path,
}

/// Positions a scan of the tree with root `root` at the first key at or
/// past `from` (past it when `inclusive` is false), or at the first key.
pub(crate) fn seek(
    ctx: &mut Ctx<'_>,
    root: (u64, u64),
    from: Option<(&[u8], bool)>,
    page: &mut [u8],
) -> Result<Cursor> {
    let height = crate::width::index(root.1)?;
    if root.0 == NO_PAGE || height == 0 || height > MAX_HEIGHT {
        return Err(corrupt("keyed tree root"));
    }
    let page = page
        .get_mut(..NODE_BYTES)
        .ok_or(corrupt("keyed tree page buffer"))?;
    let mut path = Path::new(height);
    let mut at = root.0;
    for level in (0..height).rev() {
        ctx.read(at, page)?;
        match from {
            Some((key, _)) => {
                step(ctx, page, at, &KeySrc::Parts(key), level, &mut path)?;
            }
            None => leftmost(page, at, level, &mut path)?,
        }
        if level == 0 {
            break;
        }
        at = path.pages[level - 1];
    }
    if let Some((_, false)) = from
        && path.found
    {
        path.index[0] += 1;
    }
    Ok(Cursor { path })
}

/// Records the page held in `page` at `level` of `path` and takes its first
/// entry.
fn leftmost(page: &[u8], at: u64, level: usize, path: &mut Path) -> Result<()> {
    let kind = node::kind(page)?;
    let interior = level > 0;
    let count = node::count(page);
    if (kind == node::Kind::Interior) != interior || (interior && count == 0) {
        return Err(corrupt("keyed tree page"));
    }
    path.pages[level] = at;
    path.counts[level] = count;
    path.index[level] = 0;
    if interior {
        let child = held_body(page, at, 0, true)?.child;
        if child == NO_PAGE {
            return Err(corrupt("keyed tree child"));
        }
        path.pages[level - 1] = child;
    }
    Ok(())
}

/// Visits records in key order from `cursor`, each as its leaf page (held
/// in `page`, at file offset `at`) and cell, until `visit` returns `false`
/// or the tree ends. Pages are read one at a time into `page`.
pub(crate) fn scan(
    ctx: &mut Ctx<'_>,
    mut cursor: Cursor,
    page: &mut [u8],
    visit: &mut Visit<'_>,
) -> Result<()> {
    let page = page
        .get_mut(..NODE_BYTES)
        .ok_or(corrupt("keyed tree page buffer"))?;
    let path = &mut cursor.path;
    let height = path.height;
    let mut at = path.pages[0];
    ctx.read(at, page)?;
    leaf_check(page, path)?;
    loop {
        while path.index[0] < node::count(page) {
            let body = held_body(page, at, path.index[0], false)?;
            if !visit(ctx, page, at, &body)? {
                return Ok(());
            }
            path.index[0] += 1;
        }
        // The next leaf: up to the lowest level with an entry to the
        // right, then down its leftmost edge.
        let Some(mut level) = (1..height).find(|&level| path.index[level] + 1 < path.counts[level])
        else {
            return Ok(());
        };
        path.index[level] += 1;
        loop {
            let parent = path.pages[level];
            ctx.read(parent, page)?;
            if node::kind(page)? != node::Kind::Interior || node::count(page) != path.counts[level]
            {
                return Err(corrupt("keyed tree page"));
            }
            let child = held_body(page, parent, path.index[level], true)?.child;
            if child == NO_PAGE {
                return Err(corrupt("keyed tree child"));
            }
            level -= 1;
            ctx.read(child, page)?;
            leftmost(page, child, level, path)?;
            if level == 0 {
                break;
            }
        }
        at = path.pages[0];
        leaf_check(page, path)?;
    }
}

fn leaf_check(page: &[u8], path: &Path) -> Result<()> {
    if node::kind(page)? != node::Kind::Leaf || path.index[0] > node::count(page) {
        return Err(corrupt("keyed tree page"));
    }
    Ok(())
}

/// Reads record bytes `[from, from + buf.len())` of the cell `body` of the
/// page held in `page` at file offset `at`: what the cell keeps from the
/// page, the rest from its overflow chain.
pub(crate) fn read_held(
    ctx: &mut Ctx<'_>,
    page: &[u8],
    at: u64,
    body: &Body,
    from: u64,
    buf: &mut [u8],
) -> Result<()> {
    let start = crate::width::index(body.key.at - at)?;
    let local = crate::width::index(body.key.local.min(body.key.len + body.value_len))?;
    let held = KeySrc::Held {
        local: page
            .get(start..start + local)
            .ok_or(corrupt("keyed tree cell"))?,
        len: body.key.len + body.value_len,
        overflow: body.key.overflow,
    };
    held.read(ctx, from, buf)
}
