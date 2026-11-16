//! Reading a snapshot: the file as of the last checkpoint, with every range
//! published since laid over it from the ring.

use super::arena::FixedRanges;
use super::geometry::{Geometry, TileCache, TileCoord};
use super::index::{self, Io, PageCache};
use super::store::TileSource;
use super::wal::{self, IndexRoot, Ring, RingHeader, SCAN_BUFFER, ScanEvent};
use super::{DIR_FANOUT, DIR_LEVEL_BITS};
use crate::{Error, Result};

/// Where the newest snapshot-visible bytes of one range are in the ring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayEntry {
    pub generation: u64,
    /// Logical offset of the bytes in the ring.
    pub at: u64,
    pub len: u32,
}

impl super::arena::RangeLen for OverlayEntry {
    fn range_len(&self) -> u64 {
        u64::from(self.len)
    }
}

/// Scans of the whole ring a reader tries before reporting its snapshot
/// expired.
const REBUILD_ATTEMPTS: usize = 32;

/// Ring bytes per overlay entry the writer allows: the overlay of a ring of
/// `len` bytes holds `len / OVERLAY_BYTES_PER_ENTRY` ranges. A writer whose
/// open publication would need more streams the rest to an index, so every
/// snapshot's ring ranges fit an overlay of this size.
pub(crate) const OVERLAY_BYTES_PER_ENTRY: u64 = 256;

/// Published indexes a snapshot may need at once; the writer checkpoints
/// before a publication would make more.
pub(crate) const MAX_INDEXES: usize = 8;

/// The overlay capacity for `ring`.
pub(crate) fn overlay_capacity(ring: Ring) -> usize {
    crate::width::bounded((ring.len / OVERLAY_BYTES_PER_ENTRY).max(256), usize::MAX)
}

/// Scratch for a ring scan, taken once at open.
pub(crate) fn scan_buffer() -> Result<Box<[u8]>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(SCAN_BUFFER)
        .map_err(|_| Error::AllocationFailed {
            resource: "matrix scan buffer",
            requested: SCAN_BUFFER as u64,
        })?;
    bytes.resize(SCAN_BUFFER, 0);
    Ok(bytes.into_boxed_slice())
}

/// What a snapshot lays over the file: ranges in the ring, and the indexes
/// of publications that streamed past it, each kept for as long as its
/// generation is not in place. All of it was allocated at open.
pub(crate) struct Layers {
    /// The newest ring range of each target.
    pub(crate) overlay: FixedRanges<OverlayEntry>,
    /// Published indexes, oldest generation first.
    indexes: [(u64, IndexRoot); MAX_INDEXES],
    index_len: usize,
}

impl Layers {
    pub(crate) fn new(ring: Ring) -> Result<Self> {
        Ok(Self {
            overlay: FixedRanges::with_capacity(overlay_capacity(ring), "matrix overlay")?,
            indexes: [(0, IndexRoot::default()); MAX_INDEXES],
            index_len: 0,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.overlay.clear();
        self.index_len = 0;
    }

    pub(crate) fn indexes(&self) -> &[(u64, IndexRoot)] {
        &self.indexes[..self.index_len]
    }

    /// Records a published index. More than [`MAX_INDEXES`] is a file no
    /// writer wrote.
    pub(crate) fn push_index(&mut self, generation: u64, root: IndexRoot) -> Result<()> {
        if self.index_len == MAX_INDEXES {
            return Err(Error::InvalidMatrixLayout);
        }
        self.indexes[self.index_len] = (generation, root);
        self.index_len += 1;
        Ok(())
    }

    /// Records a ring range. A full overlay first drops what is in place;
    /// `false` when it is still full.
    pub(crate) fn insert(&mut self, target: u64, entry: OverlayEntry, in_place: u64) -> bool {
        if self.overlay.insert(target, entry) {
            return true;
        }
        self.overlay.retain(|_, kept| kept.generation > in_place);
        self.overlay.insert(target, entry)
    }

    /// Forgets every range and index of generations up to `in_place`.
    pub(crate) fn drop_in_place(&mut self, in_place: u64, cache: &mut PageCache) {
        self.overlay.retain(|_, kept| kept.generation > in_place);
        let mut kept = 0;
        for i in 0..self.index_len {
            let (generation, root) = self.indexes[i];
            if generation > in_place {
                self.indexes[kept] = (generation, root);
                kept += 1;
            } else {
                cache.forget(generation);
            }
        }
        self.index_len = kept;
    }

    /// Reads `[at, at + buf.len())` as these layers show it over `source`,
    /// leaving out generations up to `skip` (in place already) and adding
    /// `open`, an index still being built, last. Indexes are laid oldest
    /// first; a ring range is laid over them unless an index of its
    /// generation or a newer one holds the same range. Returns the oldest
    /// generation laid (`u64::MAX` when none).
    pub(crate) fn fill(
        &self,
        cache: &mut PageCache,
        source: &dyn TileSource,
        at: u64,
        buf: &mut [u8],
        skip: u64,
        open: Option<(u64, IndexRoot)>,
    ) -> Result<u64> {
        let len = buf.len() as u64;
        let indexes = self.indexes().iter().copied().filter(|(g, _)| *g > skip);
        let any_index = open.is_some() || indexes.clone().next().is_some();
        let mut oldest = u64::MAX;
        if !any_index {
            // One ring range covering the whole read is read alone, without
            // the bytes it replaces.
            let mut covering = None;
            self.overlay
                .for_each_overlapping(at, len, &mut |target, entry| {
                    if entry.generation > skip
                        && target <= at
                        && target + u64::from(entry.len) >= at + len
                    {
                        covering = Some((target, *entry));
                    }
                    Ok(())
                })?;
            if let Some((target, entry)) = covering {
                lay_entry(source, at, buf, target, entry)?;
                return Ok(entry.generation);
            }
        }
        source.read_at(at, buf)?;
        for (generation, root) in indexes.chain(open) {
            index::for_each_overlapping(
                cache,
                Io::Read(source),
                generation,
                root,
                at,
                len,
                &mut |target, bytes_at, bytes_len| {
                    oldest = oldest.min(generation);
                    lay_entry(
                        source,
                        at,
                        buf,
                        target,
                        OverlayEntry {
                            generation,
                            at: bytes_at,
                            len: bytes_len,
                        },
                    )
                },
            )?;
        }
        self.overlay
            .for_each_overlapping(at, len, &mut |target, entry| {
                if entry.generation <= skip
                    || (any_index
                        && self.shadowed(cache, source, target, entry.generation, skip, open)?)
                {
                    return Ok(());
                }
                oldest = oldest.min(entry.generation);
                lay_entry(source, at, buf, target, *entry)
            })?;
        Ok(oldest)
    }

    /// Whether an index of `generation` or newer holds the range at
    /// `target`.
    fn shadowed(
        &self,
        cache: &mut PageCache,
        source: &dyn TileSource,
        target: u64,
        generation: u64,
        skip: u64,
        open: Option<(u64, IndexRoot)>,
    ) -> Result<bool> {
        let indexes = self.indexes().iter().copied().filter(|(g, _)| *g > skip);
        for (index_gen, root) in indexes.chain(open) {
            if index_gen < generation {
                continue;
            }
            let mut found = false;
            index::for_each_overlapping(
                cache,
                Io::Read(source),
                index_gen,
                root,
                target,
                1,
                &mut |key, _, _| {
                    found |= key == target;
                    Ok(())
                },
            )?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Reads `buf.len()` bytes of the ring entry `entry` overlapping `[at, ..)`
/// onto `buf`.
pub(crate) fn lay_entry(
    source: &dyn TileSource,
    at: u64,
    buf: &mut [u8],
    target: u64,
    entry: OverlayEntry,
) -> Result<()> {
    let start = at.max(target);
    let end = (at + buf.len() as u64).min(target + u64::from(entry.len));
    if start < end {
        source.read_at(
            entry.at + (start - target),
            &mut buf[crate::width::index(start - at)?..crate::width::index(end - at)?],
        )?;
    }
    Ok(())
}

/// Resolves a tile through a block's radix directory, reading every pointer
/// with `read_u64`. `None` when the tile was never allocated.
/// The largest height a directory has: eight levels of nine bits cover a
/// 64-bit Morton key.
pub(crate) const DIR_MAX_HEIGHT: u64 = 8;
/// [`DIR_MAX_HEIGHT`] as a length.
const DIR_DEPTH: usize = crate::width::small(DIR_MAX_HEIGHT);

/// An offset read from a directory page or root. Directory pages carry no
/// checksum, so what they hold is believed only as far as it can be a place
/// in a file: no file offset exceeds `i64::MAX`, and below it adding a
/// tile's or a page's length cannot overflow.
pub(crate) const fn directory_offset(raw: u64) -> Result<u64> {
    if raw > i64::MAX as u64 {
        Err(Error::InvalidMatrixLayout)
    } else {
        Ok(raw)
    }
}

/// A directory root's page and height, as stored.
pub(crate) fn directory_root(root: &[u8; 16]) -> Result<(u64, u64)> {
    let page = directory_offset(u64::from_le_bytes(root[..8].try_into().expect("eight")))?;
    let height = u64::from_le_bytes(root[8..].try_into().expect("eight"));
    if page != 0 && (height == 0 || height > DIR_MAX_HEIGHT) {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok((page, height))
}

pub(crate) fn find_tile(
    root_at: u64,
    coord: TileCoord,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<Option<u64>> {
    let key = coord.morton()?;
    let mut root = [0u8; 16];
    read(root_at, &mut root)?;
    let (mut page, height) = directory_root(&root)?;
    if page == 0 {
        return Ok(None);
    }
    if height < 8 && key >> (u64::from(DIR_LEVEL_BITS) * height) != 0 {
        return Ok(None);
    }
    for level in (0..height).rev() {
        let index = (key >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
        let mut entry = [0u8; 8];
        read(page + index * 8, &mut entry)?;
        page = directory_offset(u64::from_le_bytes(entry))?;
        if page == 0 {
            return Ok(None);
        }
    }
    Ok(Some(page))
}

/// The first allocated tile whose Morton key is `from` or later:
/// `(key, payload offset)`. Walking a block tile by tile with it needs no
/// list of tiles, and a caller may change the matrix between steps.
pub(crate) fn next_tile(
    root_at: u64,
    from: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<Option<(u64, u64)>> {
    let mut root = [0u8; 16];
    read(root_at, &mut root)?;
    let (page, height) = directory_root(&root)?;
    if page == 0 {
        return Ok(None);
    }
    let bits = u64::from(DIR_LEVEL_BITS) * height;
    if bits < 64 && from >> bits != 0 {
        return Ok(None);
    }
    // Depth-first from `from`: at each level, the entry `from` routes to,
    // then the ones after it, each subtree searched from its start.
    let mut stack = [(0u64, 0u64, 0u64); DIR_DEPTH + 1];
    let index_at =
        |key: u64, level: u64| (key >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
    stack[0] = (page, height - 1, index_at(from, height - 1));
    let mut depth = 1usize;
    // Whether the path so far still follows `from`'s digits.
    let mut tight = true;
    let mut prefix = [0u64; DIR_DEPTH + 1];
    let mut entry = [0u8; 8];
    while depth > 0 {
        let (page, level, index) = stack[depth - 1];
        if index == DIR_FANOUT {
            depth -= 1;
            tight = false;
            continue;
        }
        stack[depth - 1].2 = index + 1;
        if tight && index != index_at(from, level) {
            tight = false;
        }
        read(page + index * 8, &mut entry)?;
        let child = directory_offset(u64::from_le_bytes(entry))?;
        if child == 0 {
            tight = false;
            continue;
        }
        prefix[depth - 1] = index;
        if level == 0 {
            let mut key = 0u64;
            for (depth_index, digit) in prefix[..depth].iter().enumerate() {
                let digit_level = height - 1 - depth_index as u64;
                key |= digit << (u64::from(DIR_LEVEL_BITS) * digit_level);
            }
            return Ok(Some((key, child)));
        }
        if depth > DIR_DEPTH {
            return Err(Error::InvalidMatrixLayout);
        }
        let start = if tight { index_at(from, level - 1) } else { 0 };
        stack[depth] = (child, level - 1, start);
        depth += 1;
    }
    Ok(None)
}

/// Every allocated tile of `block`, as payload offsets, in Morton order.
pub(crate) fn for_each_tile(
    root_at: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
    visit: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut root = [0u8; 16];
    read(root_at, &mut root)?;
    let (page, height) = directory_root(&root)?;
    if page == 0 {
        return Ok(());
    }
    // Depth-first with an explicit stack of (page, level, next index): at
    // most one entry per level, and a directory has at most
    // `DIR_MAX_HEIGHT` levels.
    let mut stack = [(0u64, 0u64, 0u64); DIR_DEPTH + 1];
    let mut depth = 1usize;
    stack[0] = (page, height - 1, 0);
    let mut entries = [0u8; 8];
    while depth > 0 {
        let (page, level, index) = stack[depth - 1];
        if index == DIR_FANOUT {
            depth -= 1;
            continue;
        }
        stack[depth - 1].2 = index + 1;
        read(page + index * 8, &mut entries)?;
        let child = directory_offset(u64::from_le_bytes(entries))?;
        if child == 0 {
            continue;
        }
        if level == 0 {
            visit(child)?;
        } else {
            if depth >= DIR_DEPTH {
                return Err(Error::InvalidMatrixLayout);
            }
            stack[depth] = (child, level - 1, 0);
            depth += 1;
        }
    }
    Ok(())
}

/// A read-only snapshot of a tiled matrix. Everything it holds was
/// allocated at open; following, reading and walking allocate nothing.
/// A reader's snapshot of a ring: the ranges published after the last
/// checkpoint it saw, laid over the file. It knows nothing of what the bytes
/// mean; a matrix ([`TileReader`]) and keyed storage both read through one.
pub(crate) struct RingReader {
    pub(crate) ring: Ring,
    snapshot: u64,
    /// Ring offset just past the snapshot's last frame.
    scanned_head: u64,
    layers: Layers,
    cache: PageCache,
    scratch: Box<[u8]>,
    /// The last header a read validated against.
    seen: RingHeader,
}
impl RingReader {
    /// Opens the newest published snapshot of the ring at `ring`
    /// (payload offset, length).
    pub(crate) fn open(source: &dyn TileSource, ring: (u64, u64)) -> Result<Self> {
        let ring = Ring::within(ring.0, ring.1, source)?;
        let mut reader = Self {
            ring,
            snapshot: 0,
            scanned_head: 0,
            layers: Layers::new(ring)?,
            cache: PageCache::with_pages(index::DEFAULT_CACHE_PAGES)?,
            scratch: scan_buffer()?,
            seen: RingHeader::default(),
        };
        reader.rebuild(source)?;
        Ok(reader)
    }

    /// The generation this reader shows.
    pub(crate) const fn snapshot(&self) -> u64 {
        self.snapshot
    }

    /// Adds what a scan reports to `layers`.
    fn adopt(layers: &mut Layers, event: ScanEvent, in_place: u64) -> Result<()> {
        match event {
            ScanEvent::Entry(generation, scanned) => {
                let entry = OverlayEntry {
                    generation,
                    at: scanned.at,
                    len: scanned.len,
                };
                if layers.insert(scanned.target, entry, in_place) {
                    Ok(())
                } else {
                    // A writer never publishes more ring ranges than an
                    // overlay of its ring holds.
                    Err(Error::InvalidMatrixLayout)
                }
            }
            ScanEvent::Index(generation, root) => layers.push_index(generation, root),
            ScanEvent::Committed(..) => Ok(()),
        }
    }

    fn rebuild(&mut self, source: &dyn TileSource) -> Result<()> {
        // A checkpoint that reclaims frames during the scan spoils it; a
        // slow reader (a loaded machine) may need several passes.
        for _ in 0..REBUILD_ATTEMPTS {
            let header = self.ring.read_header(source)?;
            self.layers.clear();
            let layers = &mut self.layers;
            let mut src = source;
            let end = wal::scan(
                &mut src,
                self.ring,
                header.tail,
                header.checkpointed + 1,
                header.published,
                &mut self.scratch,
                &mut |_, event| Self::adopt(layers, event, header.checkpointed),
            )?;
            // The frames were read while a writer may have been reusing their
            // space: they count only if the ring still keeps them now. What
            // they prove is the snapshot, as for a writer's recovery: a frame
            // that does not verify ends it, even if the header names a later
            // generation.
            let after = self.ring.read_header(source)?;
            if after.reclaimed <= header.checkpointed {
                self.snapshot = end.published;
                self.scanned_head = end.head;
                self.seen = after;
                return Ok(());
            }
        }
        Err(Error::MatrixSnapshotExpired {
            snapshot: self.snapshot,
            checkpointed: self.seen.checkpointed,
        })
    }

    /// Adopts the newest published generation. Returns whether it changed.
    pub(crate) fn follow(&mut self, source: &dyn TileSource) -> Result<bool> {
        let header = self.ring.read_header(source)?;
        if header.published == self.snapshot {
            self.seen = header;
            return Ok(false);
        }
        if header.reclaimed > self.snapshot {
            // The frames after this snapshot are gone from the ring: start
            // over from what it keeps.
            self.rebuild(source)?;
            return Ok(true);
        }
        // What the file already holds no longer needs the ring.
        if header.checkpointed > self.seen.checkpointed {
            self.layers
                .drop_in_place(header.checkpointed, &mut self.cache);
        }
        let layers = &mut self.layers;
        let mut src = source;
        let end = wal::scan(
            &mut src,
            self.ring,
            self.scanned_head,
            self.snapshot + 1,
            header.published,
            &mut self.scratch,
            &mut |_, event| Self::adopt(layers, event, header.checkpointed),
        )?;
        let after = self.ring.read_header(source)?;
        // A scan cut short may have adopted part of a generation: only a
        // rebuild tells what the ring still proves.
        if after.reclaimed > self.snapshot || end.published < header.published {
            self.rebuild(source)?;
            return Ok(true);
        }
        self.snapshot = end.published;
        self.scanned_head = end.head;
        self.seen = after;
        Ok(true)
    }

    /// Reads `buf.len()` bytes at logical `offset` as of this snapshot.
    pub(crate) fn read(
        &mut self,
        source: &dyn TileSource,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        for _ in 0..8 {
            let before = self.seen;
            // Frames the ring no longer promises are in place by now.
            let laid =
                self.layers
                    .fill(&mut self.cache, source, offset, buf, before.reclaimed, None);
            let after = self.ring.read_header(source)?;
            self.seen = after;
            if after.mark > self.snapshot {
                return Err(Error::MatrixSnapshotExpired {
                    snapshot: self.snapshot,
                    checkpointed: after.mark,
                });
            }
            match laid {
                Ok(laid_oldest) if laid_oldest > after.reclaimed => return Ok(()),
                Ok(_) => {}
                // An index page read while the writer reused its space does
                // not check out; the header then shows the reclaim.
                Err(_) if after.reclaimed > before.reclaimed => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::MatrixSnapshotExpired {
            snapshot: self.snapshot,
            checkpointed: self.seen.checkpointed,
        })
    }

    /// Reads without validating: the caller validates once for a batch
    /// with [`Self::settle`]. Returns the oldest generation laid
    /// (`u64::MAX` when none).
    pub(crate) fn read_unsettled(
        &mut self,
        source: &dyn TileSource,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<u64> {
        let before = self.seen;
        match self
            .layers
            .fill(&mut self.cache, source, offset, buf, before.reclaimed, None)
        {
            Ok(laid) => Ok(laid),
            // Settled below: a reclaim shows as an expired batch.
            Err(_) if self.ring.read_header(source)?.reclaimed > before.reclaimed => Ok(0),
            Err(error) => Err(error),
        }
    }
    /// Calls `visit` with every range this snapshot reads from the ring or
    /// an index, in the order they must be laid.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &mut self,
        source: &dyn TileSource,
        visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
    ) -> Result<()> {
        let skip = self.seen.reclaimed;
        let upto = self.snapshot;
        for_each_layered(
            &self.layers,
            &mut self.cache,
            source,
            skip,
            upto,
            None,
            visit,
        )
    }

    /// Validates every [`Self::read_unsettled`] since the last settle, whose
    /// oldest laid generation is `laid_oldest`: `Ok(true)` when they all
    /// show this snapshot, `Ok(false)` when the ring moved under them and
    /// the batch must be read again, an error when the snapshot expired.
    pub(crate) fn settle(&mut self, source: &dyn TileSource, laid_oldest: u64) -> Result<bool> {
        let after = self.ring.read_header(source)?;
        let before = self.seen;
        self.seen = after;
        if after.mark > self.snapshot {
            return Err(Error::MatrixSnapshotExpired {
                snapshot: self.snapshot,
                checkpointed: after.mark,
            });
        }
        // A reclaim the reads did not know about may have skipped a range
        // they should have laid, or laid one being overwritten.
        Ok(laid_oldest > after.reclaimed && after.reclaimed == before.reclaimed)
    }
}

/// A matrix reader: tiles and directories over a [`RingReader`] snapshot.
pub(crate) struct TileReader {
    pub(crate) geom: Geometry,
    pub(crate) meta: u64,
    /// Tiles never move once allocated, so a resolved one stays resolved.
    tiles: TileCache,
    pub(crate) reader: RingReader,
}

impl TileReader {
    /// Opens the newest published snapshot of the matrix whose metadata
    /// record payload is at `meta`.
    pub(crate) fn open(source: &dyn TileSource, geom: Geometry, meta: u64) -> Result<Self> {
        let mut fixed = [0u8; crate::width::small(super::geometry::META_FIXED_LEN)];
        source.read_at(meta, &mut fixed)?;
        let ring = geom.decode_meta(&fixed)?;
        let reader = RingReader::open(source, ring)?;
        Ok(Self {
            tiles: TileCache::for_geometry(&geom)?,
            geom,
            meta,
            reader,
        })
    }

    /// A resolved tile from the cache, without reading anything.
    pub(crate) fn cached_tile(&self, block: usize, coord: TileCoord) -> Result<Option<u64>> {
        Ok(self.tiles.get(block, coord.morton()?))
    }

    /// The payload offset of `block`'s tile `coord`, if it exists.
    pub(crate) fn tile(
        &mut self,
        source: &dyn TileSource,
        block: usize,
        coord: TileCoord,
    ) -> Result<Option<u64>> {
        let key = coord.morton()?;
        if let Some(found) = self.tiles.get(block, key) {
            return Ok(Some(found));
        }
        let root_at = self.meta + self.geom.root_at(block);
        let found = find_tile(root_at, coord, &mut |at, buf| {
            self.reader.read(source, at, buf)
        })?;
        if let Some(found) = found {
            self.tiles.insert(block, key, found);
        }
        Ok(found)
    }

    /// Calls `visit` with every allocated tile of `block`.
    pub(crate) fn for_each_tile(
        &mut self,
        source: &dyn TileSource,
        block: usize,
        visit: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        let root_at = self.meta + self.geom.root_at(block);
        for_each_tile(
            root_at,
            &mut |at, buf| self.reader.read(source, at, buf),
            visit,
        )
    }
}

/// Every range `layers` lays over the file, indexes oldest first and then
/// the ring ranges no newer index holds: laying them in this order gives
/// what a read sees.
#[cfg(feature = "mmap")]
pub(crate) fn for_each_layered(
    layers: &Layers,
    cache: &mut PageCache,
    source: &dyn TileSource,
    skip: u64,
    upto: u64,
    open: Option<(u64, IndexRoot)>,
    visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
) -> Result<()> {
    for (generation, root) in layers
        .indexes()
        .iter()
        .copied()
        .filter(|(g, _)| *g > skip && *g <= upto)
        .chain(open)
    {
        index::for_each(
            cache,
            Io::Read(source),
            generation,
            root,
            &mut |_, target, at, len| {
                visit(
                    target,
                    OverlayEntry {
                        generation,
                        at,
                        len,
                    },
                )
            },
        )?;
    }
    let any_index = open.is_some() || layers.indexes().iter().any(|(g, _)| *g > skip);
    layers.overlay.for_each(&mut |target, entry| {
        if entry.generation <= skip
            || entry.generation > upto
            || (any_index
                && layers.shadowed(cache, source, target, entry.generation, skip, open)?)
        {
            return Ok(());
        }
        visit(target, *entry)
    })
}
