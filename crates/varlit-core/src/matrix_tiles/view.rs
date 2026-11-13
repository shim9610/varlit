//! Reading a snapshot: the file as of the last checkpoint, with every range
//! published since laid over it from the ring.

use std::collections::HashMap;

use super::geometry::{Geometry, TileCoord};
use super::store::TileSource;
use super::wal::{self, Ring, RingHeader, ScanEvent};
use super::{DIR_FANOUT, DIR_LEVEL_BITS};
use crate::{Error, Result};

/// Where the newest snapshot-visible bytes of one range are in the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlayEntry {
    pub generation: u64,
    /// Logical offset of the bytes in the ring.
    pub at: u64,
    pub len: u32,
}

impl super::ranges::RangeLen for OverlayEntry {
    fn range_len(&self) -> u64 {
        u64::from(self.len)
    }
}

/// Scans of the whole ring a reader tries before reporting its snapshot
/// expired.
const REBUILD_ATTEMPTS: usize = 32;

/// The ranges a snapshot reads from the ring, keyed by where they belong.
/// Ranges are whole units (a slot, a bitmap, a pointer, a root), so two
/// never overlap partially; a read may still span several.
pub(crate) struct Overlay {
    pub(crate) ranges: super::ranges::Ranges<OverlayEntry>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            ranges: super::ranges::Ranges::new("matrix overlay"),
        }
    }
}

impl Overlay {
    pub(crate) fn insert(&mut self, target: u64, entry: OverlayEntry) -> Result<()> {
        self.ranges.insert(target, entry).map(drop)
    }

    /// Lays the ring bytes of every entry overlapping `[at, at + len)` onto
    /// `buf`, skipping entries `skip` names. Returns the oldest generation
    /// laid (`u64::MAX` when none).
    pub(crate) fn lay(
        &self,
        source: &dyn TileSource,
        at: u64,
        buf: &mut [u8],
        skip: &dyn Fn(&OverlayEntry) -> bool,
    ) -> Result<u64> {
        let mut oldest = u64::MAX;
        self.ranges
            .for_each_overlapping(at, buf.len() as u64, &mut |target, entry| {
                if skip(entry) {
                    return Ok(());
                }
                lay_entry(source, at, buf, target, *entry)?;
                oldest = oldest.min(entry.generation);
                Ok(())
            })?;
        Ok(oldest)
    }

    /// Reads `[at, at + buf.len())` as this overlay shows it: the main
    /// bytes from `source` with every entry not `skip`ped laid over them.
    /// When one entry covers the whole range it is read alone, without the
    /// main bytes it would replace anyway (entries never overlap partially,
    /// so a covering entry is the only one there). Returns the oldest
    /// generation laid (`u64::MAX` when none).
    pub(crate) fn fill(
        &self,
        source: &dyn TileSource,
        at: u64,
        buf: &mut [u8],
        skip: &dyn Fn(&OverlayEntry) -> bool,
    ) -> Result<u64> {
        let len = buf.len() as u64;
        let mut covering = None;
        self.ranges
            .for_each_overlapping(at, len, &mut |target, entry| {
                if !skip(entry) && target <= at && target + u64::from(entry.len) >= at + len {
                    covering = Some((target, *entry));
                }
                Ok(())
            })?;
        if let Some((target, entry)) = covering {
            lay_entry(source, at, buf, target, entry)?;
            return Ok(entry.generation);
        }
        source.read_at(at, buf)?;
        self.lay(source, at, buf, skip)
    }
}

/// Copies the part of `src` (which belongs at `src_at`) that falls inside
/// `out` (which belongs at `out_at`).
pub(crate) fn lay_over(out_at: u64, out: &mut [u8], src_at: u64, src: &[u8]) {
    let start = out_at.max(src_at);
    let end = (out_at + out.len() as u64).min(src_at + src.len() as u64);
    if start < end {
        out[(start - out_at) as usize..(end - out_at) as usize]
            .copy_from_slice(&src[(start - src_at) as usize..(end - src_at) as usize]);
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
            &mut buf[(start - at) as usize..(end - at) as usize],
        )?;
    }
    Ok(())
}

/// Resolves a tile through a block's radix directory, reading every pointer
/// with `read_u64`. `None` when the tile was never allocated.
pub(crate) fn find_tile(
    root_at: u64,
    coord: TileCoord,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<Option<u64>> {
    let key = coord.morton()?;
    let mut root = [0u8; 16];
    read(root_at, &mut root)?;
    let mut page = u64::from_le_bytes(root[..8].try_into().expect("eight"));
    let height = u64::from_le_bytes(root[8..].try_into().expect("eight"));
    if page == 0 {
        return Ok(None);
    }
    if height == 0 || height > 8 {
        return Err(Error::InvalidMatrixLayout);
    }
    if height < 8 && key >> (DIR_LEVEL_BITS * height as u32) != 0 {
        return Ok(None);
    }
    for level in (0..height).rev() {
        let index = (key >> (DIR_LEVEL_BITS * level as u32)) & (DIR_FANOUT - 1);
        let mut entry = [0u8; 8];
        read(page + index * 8, &mut entry)?;
        page = u64::from_le_bytes(entry);
        if page == 0 {
            return Ok(None);
        }
    }
    Ok(Some(page))
}

/// Every allocated tile of `block`, as payload offsets, in Morton order.
pub(crate) fn for_each_tile(
    root_at: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> Result<()>,
    visit: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut root = [0u8; 16];
    read(root_at, &mut root)?;
    let page = u64::from_le_bytes(root[..8].try_into().expect("eight"));
    let height = u64::from_le_bytes(root[8..].try_into().expect("eight"));
    if page == 0 {
        return Ok(());
    }
    if height == 0 || height > 8 {
        return Err(Error::InvalidMatrixLayout);
    }
    // Depth-first with an explicit stack of (page, level): at most
    // `height` pages of pointers are held at once.
    let mut stack: Vec<(u64, u64, u64)> = Vec::new();
    stack.try_reserve(9).map_err(|_| Error::AllocationFailed {
        resource: "matrix directory walk",
        requested: 8 * 24,
    })?;
    stack.push((page, height - 1, 0));
    let mut entries = [0u8; 8];
    while let Some((page, level, index)) = stack.pop() {
        if index == DIR_FANOUT {
            continue;
        }
        stack.push((page, level, index + 1));
        read(page + index * 8, &mut entries)?;
        let child = u64::from_le_bytes(entries);
        if child == 0 {
            continue;
        }
        if level == 0 {
            visit(child)?;
        } else {
            if stack.len() >= 8 {
                return Err(Error::InvalidMatrixLayout);
            }
            stack.push((child, level - 1, 0));
        }
    }
    Ok(())
}

/// A read-only snapshot of a tiled matrix.
pub(crate) struct TileReader {
    pub(crate) geom: Geometry,
    pub(crate) meta: u64,
    pub(crate) ring: Ring,
    snapshot: u64,
    /// Ring offset just past the snapshot's last frame.
    scanned_head: u64,
    overlay: Overlay,
    /// The last header a read validated against.
    seen: RingHeader,
    /// Tiles never move once allocated, so a resolved one stays resolved.
    tiles: HashMap<(usize, u64), u64>,
}

impl TileReader {
    /// Opens the newest published snapshot of the matrix whose metadata
    /// record payload is at `meta`.
    pub(crate) fn open(source: &dyn TileSource, geom: Geometry, meta: u64) -> Result<Self> {
        let mut fixed = [0u8; super::geometry::META_FIXED_LEN as usize];
        source.read_at(meta, &mut fixed)?;
        let (base, len) = geom.decode_meta(&fixed)?;
        let ring = Ring { base, len };
        let mut reader = Self {
            geom,
            meta,
            ring,
            snapshot: 0,
            scanned_head: 0,
            overlay: Overlay::default(),
            seen: RingHeader::default(),
            tiles: HashMap::new(),
        };
        reader.rebuild(source)?;
        Ok(reader)
    }

    /// The generation this reader shows.
    pub(crate) const fn snapshot(&self) -> u64 {
        self.snapshot
    }

    fn rebuild(&mut self, source: &dyn TileSource) -> Result<()> {
        // A checkpoint that reclaims frames during the scan spoils it; a
        // slow reader (a loaded machine) may need several passes.
        for _ in 0..REBUILD_ATTEMPTS {
            let header = self.ring.read_header(source)?;
            let mut overlay = Overlay::default();
            let end = wal::scan(
                source,
                self.ring,
                header.tail,
                header.checkpointed + 1,
                header.published,
                &mut |event| match event {
                    ScanEvent::Entry(generation, entry) => overlay.insert(
                        entry.target,
                        OverlayEntry {
                            generation,
                            at: entry.at,
                            len: entry.len,
                        },
                    ),
                    ScanEvent::Committed(..) => Ok(()),
                },
            )?;
            // The frames were read while a writer may have been reusing their
            // space: they count only if the ring still keeps them now. What
            // they prove is the snapshot, as for a writer's recovery: a frame
            // that does not verify ends it, even if the header names a later
            // generation.
            let after = self.ring.read_header(source)?;
            if after.reclaimed <= header.checkpointed {
                self.overlay = overlay;
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
        // Collected first and adopted only once the scan is known good; in
        // ring order, so a later write of the same range replaces an
        // earlier one when they are adopted.
        let mut added: Vec<(u64, OverlayEntry)> = Vec::new();
        let end = wal::scan(
            source,
            self.ring,
            self.scanned_head,
            self.snapshot + 1,
            header.published,
            &mut |event| match event {
                ScanEvent::Entry(generation, entry) => {
                    added.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "matrix overlay follow",
                        requested: (added.len() as u64 + 1) * 32,
                    })?;
                    added.push((
                        entry.target,
                        OverlayEntry {
                            generation,
                            at: entry.at,
                            len: entry.len,
                        },
                    ));
                    Ok(())
                }
                ScanEvent::Committed(..) => Ok(()),
            },
        )?;
        let after = self.ring.read_header(source)?;
        if after.reclaimed > self.snapshot || end.published < header.published {
            self.rebuild(source)?;
            return Ok(true);
        }
        for (target, entry) in added {
            self.overlay.insert(target, entry)?;
        }
        // What the file already holds no longer needs the ring.
        // Only a checkpoint since the last look frees anything: sweeping
        // the overlay on every follow would cost its whole size each time.
        let checkpointed = after.checkpointed;
        if checkpointed > self.seen.checkpointed {
            self.overlay
                .ranges
                .retain(|_, entry| entry.generation > checkpointed);
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
            let laid_oldest = self.overlay.fill(source, offset, buf, &|entry| {
                entry.generation <= before.reclaimed
            })?;
            let after = self.ring.read_header(source)?;
            self.seen = after;
            if after.mark > self.snapshot {
                return Err(Error::MatrixSnapshotExpired {
                    snapshot: self.snapshot,
                    checkpointed: after.mark,
                });
            }
            if laid_oldest > after.reclaimed {
                return Ok(());
            }
        }
        Err(Error::MatrixSnapshotExpired {
            snapshot: self.snapshot,
            checkpointed: self.seen.checkpointed,
        })
    }

    /// Reads without validating: the caller validates once for a batch
    /// with [`Self::settle`]. Returns the oldest generation laid from the
    /// ring (`u64::MAX` when none).
    pub(crate) fn read_unsettled(
        &self,
        source: &dyn TileSource,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<u64> {
        let before = self.seen;
        self.overlay.fill(source, offset, buf, &|entry| {
            entry.generation <= before.reclaimed
        })
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

    /// A resolved tile from the cache, without reading anything.
    pub(crate) fn cached_tile(&self, block: usize, coord: TileCoord) -> Result<Option<u64>> {
        Ok(self.tiles.get(&(block, coord.morton()?)).copied())
    }

    /// The payload offset of `block`'s tile `coord`, if it exists.
    pub(crate) fn tile(
        &mut self,
        source: &dyn TileSource,
        block: usize,
        coord: TileCoord,
    ) -> Result<Option<u64>> {
        let key = coord.morton()?;
        if let Some(found) = self.tiles.get(&(block, key)) {
            return Ok(Some(*found));
        }
        let root_at = self.meta + self.geom.root_at(block);
        let found = find_tile(root_at, coord, &mut |at, buf| self.read(source, at, buf))?;
        if let Some(found) = found
            && self.tiles.try_reserve(1).is_ok()
        {
            // A cache: when it cannot grow, the next lookup walks again.
            self.tiles.insert((block, key), found);
        }
        Ok(found)
    }

    /// Calls `visit` with every range this snapshot reads from the ring.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &self,
        visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
    ) -> Result<()> {
        for (target, entry) in self.overlay.ranges.iter() {
            if entry.generation > self.seen.reclaimed {
                visit(target, *entry)?;
            }
        }
        Ok(())
    }

    /// Calls `visit` with every allocated tile of `block`.
    pub(crate) fn for_each_tile(
        &mut self,
        source: &dyn TileSource,
        block: usize,
        visit: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        let root_at = self.meta + self.geom.root_at(block);
        for_each_tile(root_at, &mut |at, buf| self.read(source, at, buf), visit)
    }
}
