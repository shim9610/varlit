//! The matrix writer: tiles and their directories over the in-place storage
//! [`RingStore`], which stages, publishes and checkpoints the bytes.

use super::geometry::{Geometry, META_FIXED_LEN, META_OVERFLOW_HEAD_AT, TileCache, TileCoord};
use super::ring_store::{RingStore, ZEROS};
use super::store::{TileSource, TileStore};
use super::view::{directory_offset, directory_root, find_tile, for_each_tile};
use super::{DIR_FANOUT, DIR_LEVEL_BITS, DIR_PAGE_BYTES, RecordKind};
use crate::Result;

pub(crate) struct TileWriter {
    pub(crate) geom: Geometry,
    pub(crate) meta: u64,
    tiles: TileCache,
    pub(crate) engine: RingStore,
}

impl TileWriter {
    /// Lays out a new matrix: the metadata record, then a ring of `ring_len`
    /// bytes. Returns the writer; the metadata payload offset is `meta`.
    pub(crate) fn create(
        store: &mut dyn TileStore,
        geom: Geometry,
        ring_len: u64,
        creation_id: [u8; 16],
    ) -> Result<Self> {
        let mut fixed = [0u8; crate::width::small(META_FIXED_LEN)];
        // The ring record follows the metadata record; its payload offset is
        // only known once appended, so the metadata is appended with a
        // placeholder and its fixed part rewritten.
        geom.encode_meta((0, ring_len), creation_id, &mut fixed);
        let meta = store.append_zeroed(RecordKind::Meta, &fixed, geom.meta_len())?;
        let engine = RingStore::create(store, ring_len, meta + META_OVERFLOW_HEAD_AT)?;
        geom.encode_meta((engine.ring.base, engine.ring.len), creation_id, &mut fixed);
        store.write_at(meta, &fixed)?;
        store.sync()?;
        Self::with_engine(geom, meta, engine)
    }

    /// Opens an existing matrix for writing; see [`RingStore::open`].
    pub(crate) fn open(store: &mut dyn TileStore, geom: Geometry, meta: u64) -> Result<Self> {
        let mut fixed = [0u8; crate::width::small(META_FIXED_LEN)];
        store.read_at(meta, &mut fixed)?;
        let ring = geom.decode_meta(&fixed)?;
        let engine = RingStore::open(store, ring, meta + META_OVERFLOW_HEAD_AT)?;
        Self::with_engine(geom, meta, engine)
    }

    fn with_engine(geom: Geometry, meta: u64, engine: RingStore) -> Result<Self> {
        Ok(Self {
            tiles: TileCache::for_geometry(&geom)?,
            geom,
            meta,
            engine,
        })
    }

    /// Forgets everything staged since the last publication.
    #[cfg(test)]
    pub(crate) fn discard(&mut self) {
        self.tiles.clear();
        self.engine.discard();
    }

    /// The payload offset of `block`'s tile `coord`, appending it (and the
    /// directory pages it needs) when `create` and it does not exist yet.
    pub(crate) fn tile(
        &mut self,
        store: &mut dyn TileStore,
        block: usize,
        coord: TileCoord,
        create: bool,
    ) -> Result<Option<u64>> {
        let key = coord.morton()?;
        if let Some(found) = self.tiles.get(block, key) {
            return Ok(Some(found));
        }
        let root_at = self.meta + self.geom.root_at(block);
        let found = find_tile(root_at, coord, &mut |at, buf| {
            self.engine.read(store, at, buf)
        })?;
        let found = match found {
            Some(found) => found,
            None if !create => return Ok(None),
            None => self.allocate_tile(store, block, coord, key)?,
        };
        self.tiles.insert(block, key, found);
        Ok(Some(found))
    }

    // ---- tiles ----
    fn allocate_tile(
        &mut self,
        store: &mut dyn TileStore,
        block: usize,
        coord: TileCoord,
        key: u64,
    ) -> Result<u64> {
        let root_at = self.meta + self.geom.root_at(block);
        let mut root = [0u8; 16];
        self.engine.read(store, root_at, &mut root)?;
        let (mut page, mut height) = directory_root(&root)?;
        let needed = levels_for(key);
        if page == 0 {
            page = self.append_page(store)?;
            height = needed;
        }
        while height < needed {
            // A taller root whose first entry is the old one: every existing
            // key keeps its path.
            let taller = self.append_page(store)?;
            self.engine.stage(store, taller, &page.to_le_bytes())?;
            page = taller;
            height += 1;
        }
        let mut new_root = [0u8; 16];
        new_root[..8].copy_from_slice(&page.to_le_bytes());
        new_root[8..].copy_from_slice(&height.to_le_bytes());
        if new_root != root {
            self.engine.stage(store, root_at, &new_root)?;
        }
        for level in (1..height).rev() {
            let index = (key >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
            let at = page + index * 8;
            let mut entry = [0u8; 8];
            self.engine.read(store, at, &mut entry)?;
            let mut child = directory_offset(u64::from_le_bytes(entry))?;
            if child == 0 {
                child = self.append_page(store)?;
                self.engine.stage(store, at, &child.to_le_bytes())?;
            }
            page = child;
        }
        let geom = self.geom.blocks[block];
        let header = geom.empty_tile_header(coord);
        let tile_at =
            self.engine
                .place_record(store, RecordKind::Tile, &header, geom.tile_bytes())?;
        // The empty bitmap's checksum: of zeros, hashed from the zero page.
        let mut hasher = crc32fast::Hasher::new();
        let mut left = geom.bitmap_len();
        while left > 0 {
            let n = crate::width::bounded(left, ZEROS.len());
            hasher.update(&ZEROS[..n]);
            left -= n as u64;
        }
        store.write_at(
            tile_at + geom.bitmap_at() + geom.bitmap_len(),
            &hasher.finalize().to_le_bytes(),
        )?;
        let at = page + (key & (DIR_FANOUT - 1)) * 8;
        self.engine.stage(store, at, &tile_at.to_le_bytes())?;
        Ok(tile_at)
    }

    fn append_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.engine
            .place_record(store, RecordKind::Directory, &[], DIR_PAGE_BYTES)
    }

    /// The payload offset of `block`'s tile `coord` if it exists, without
    /// caching the answer (for `&self` reads).
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block: usize,
        coord: TileCoord,
    ) -> Result<Option<u64>> {
        if let Some(found) = self.tiles.get(block, coord.morton()?) {
            return Ok(Some(found));
        }
        find_tile(
            self.meta + self.geom.root_at(block),
            coord,
            &mut |at, buf| self.engine.read(store, at, buf),
        )
    }

    /// [`Self::for_each_tile`] for `&self` reads.
    pub(crate) fn for_each_tile_ref(
        &self,
        store: &dyn TileSource,
        block: usize,
        visit: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        for_each_tile(
            self.meta + self.geom.root_at(block),
            &mut |at, buf| self.engine.read(store, at, buf),
            visit,
        )
    }
}

/// Directory levels a Morton key needs.
fn levels_for(key: u64) -> u64 {
    let bits = 64 - key.leading_zeros();
    u64::from(bits.div_ceil(DIR_LEVEL_BITS).max(1))
}
