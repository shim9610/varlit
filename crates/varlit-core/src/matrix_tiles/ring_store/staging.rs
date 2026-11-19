// Staging into the open frame of a [`RingStore`]: tile writes, buffered
// frames, overflow regions and the coalescing of a publication's pages.

// Included by `ring_store.rs`, whose module it shares.

impl RingStore {
    /// Stages `bytes` to be written at `target` by the next publication. A
    /// later stage of the same range replaces it.
    pub(crate) fn stage(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        bytes: &[u8],
    ) -> Result<()> {
        self.stage_src(store, target, Src::Bytes(bytes))
    }

    fn stage_src(&mut self, store: &mut dyn TileStore, target: u64, src: Src<'_>) -> Result<()> {
        if self.is_fresh(target, src.len()) {
            self.write_src(store, target, src, false)?;
            self.appended = true;
            return Ok(());
        }
        self.spilled = true;
        if self.building.is_none() && !self.stage_in_ring(store, target, src)? {
            self.start_index(store)?;
        }
        if self.building.is_some() {
            self.stage_in_index(store, target, src)?;
        }
        Ok(())
    }

    /// Writes `src` at `at`, feeding the open frame's checksum when `hash`.
    fn write_src(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        src: Src<'_>,
        hash: bool,
    ) -> Result<()> {
        match src {
            Src::Bytes(bytes) => {
                store.write_at(at, bytes)?;
                if hash && let Some(frame) = self.frame.as_mut() {
                    frame.hasher.update(bytes);
                }
            }
        }
        Ok(())
    }

    /// Ring bytes the open generation may still take before the rest of it
    /// streams into an index appended to the file.
    pub(crate) fn ring_room(&self) -> u64 {
        (self.ring.area() / 2).saturating_sub(self.open_ring_bytes)
    }

    /// The bytes of the ring's frame area.
    pub(crate) const fn ring_area(&self) -> u64 {
        self.ring.area()
    }

    /// Ring bytes the published generations not yet in place take.
    pub(crate) fn published_ring_bytes(&self) -> u64 {
        self.ring.used(self.header.tail, self.header.head)
    }

    /// The newest generation in place: ranges staged in it or before are no
    /// longer in any overlay.
    pub(crate) const fn checkpointed(&self) -> u64 {
        self.header.checkpointed
    }

    /// The oldest generation a reader may still show: a reader whose
    /// snapshot is older expires. A checkpoint raises it to the generation it
    /// put in place; [`Self::raise_mark`] raises it further.
    pub(crate) const fn mark(&self) -> u64 {
        self.header.mark
    }

    /// Expires every reader older than the last publication. Afterwards no
    /// reader and no recovery shows a generation older than it, so space
    /// that generation no longer points at may be written in place.
    pub(crate) fn raise_mark(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.header.mark < self.header.published {
            let mut marked = self.header;
            marked.mark = marked.published;
            self.write_header(store, marked)?;
        }
        Ok(())
    }

    /// Writes `bytes` at `target` straight to the file instead of through
    /// the ring, when nothing the ring holds covers any of them: no range
    /// staged since the last checkpoint (published or not) and no index.
    /// Returns `false`, writing nothing, otherwise.
    ///
    /// Only for space the caller knows no generation from the mark on points
    /// at, such as space freed by a generation the mark has reached. A
    /// reader whose snapshot is older than the mark expires, and newer
    /// snapshots do not point at the space, so no reader can see these bytes
    /// before the publication that points at them; that publication makes
    /// them durable before its commit frame, as it does appended records. A
    /// recovery shows the last publication, which does not point at them
    /// either.
    pub(crate) fn write_unreferenced(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        bytes: &[u8],
    ) -> Result<bool> {
        let len = bytes.len() as u64;
        if !self.is_fresh(target, len) && self.covers(&*store, target, len)? {
            return Ok(false);
        }
        store.write_at(target, bytes)?;
        self.appended = true;
        Ok(true)
    }

    /// Whether anything staged since the last checkpoint, in the ring or in
    /// an index, published or not, covers any of `[target, target + len)`.
    fn covers(&self, store: &dyn TileSource, target: u64, len: u64) -> Result<bool> {
        let mut covered = false;
        self.layers
            .overlay
            .for_each_overlapping(target, len, &mut |_, _| {
                covered = true;
                Ok(())
            })?;
        if covered {
            return Ok(true);
        }
        let checkpointed = self.header.checkpointed;
        let open = self.building.map(|b| (b.generation, b.root));
        let indexes = self.layers.indexes().iter().copied();
        let mut cache = self.cache()?;
        for (generation, root) in indexes.filter(|(g, _)| *g > checkpointed).chain(open) {
            index::for_each_overlapping(
                &mut cache,
                Io::Read(store),
                generation,
                root,
                target,
                len,
                &mut |_, _, _| {
                    covered = true;
                    Ok(())
                },
            )?;
            if covered {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `[target, target + len)` lies inside a record appended, or
    /// space carved, since the last publication.
    fn is_fresh(&self, target: u64, len: u64) -> bool {
        target >= self.fresh_floor || self.space.is_carved_fresh(target, len)
    }

    /// The largest frame: a quarter of the ring, so a checkpoint can always
    /// make room for the next one.
    fn frame_limit(&self) -> u64 {
        (self.ring.area() / 4).max(FRAME_MIN * 2)
    }

    /// Writes the range into the open ring frame. `false` when it belongs in
    /// an index instead: it does not fit a frame, the open generation would
    /// take more than half the ring, or the overlay has no room for it.
    fn stage_in_ring(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        src: Src<'_>,
    ) -> Result<bool> {
        let len = src.len();
        let need = ENTRY_HEADER + len;
        // Room for the entry, a header, padding and the checksum.
        let whole = FRAME_HEADER + need + 12;
        if whole > self.frame_limit() || self.open_ring_bytes + whole > self.ring.area() / 2 {
            return Ok(false);
        }
        let overlay = &mut self.layers.overlay;
        if overlay.len() == overlay.capacity() && overlay.get(target).is_none() {
            let in_place = self.header.checkpointed;
            overlay.retain(|_, entry| entry.generation > in_place);
            if overlay.len() == overlay.capacity() {
                return Ok(false);
            }
        }
        if let Some(frame) = &self.frame
            && frame.len + need + 12 > frame.reserved
        {
            self.close_frame(store, false)?;
        }
        if self.frame.is_none() {
            self.open_frame(store, whole)?;
        }
        let frame = self.frame.as_mut().expect("opened above");
        let at = self.ring.at(frame.at + frame.len);
        let mut entry = [0u8; crate::width::small(ENTRY_HEADER)];
        entry[..8].copy_from_slice(&target.to_le_bytes());
        let len32 = crate::width::field32(crate::width::small(len))?;
        entry[8..].copy_from_slice(&len32.to_le_bytes());
        let header_len = entry.len();
        frame.hasher.update(&entry);
        frame.len += need;
        frame.count += 1;
        debug_assert_eq!(header_len as u64, ENTRY_HEADER);
        let Src::Bytes(bytes) = src;
        frame.hasher.update(bytes);
        self.hold(store, at, &entry)?;
        self.hold(store, at + ENTRY_HEADER, bytes)?;
        self.open_ring_bytes += need;
        let inserted = self.layers.overlay.insert(
            target,
            OverlayEntry {
                generation: self.header.published + 1,
                at: at + ENTRY_HEADER,
                len: len32,
            },
        );
        debug_assert!(inserted, "room was made above");
        Ok(true)
    }

    /// Buffers `bytes` for `at`, after what is held when it follows on;
    /// writes what is held first when it does not, and whenever the buffer
    /// fills.
    fn hold(&mut self, store: &mut dyn TileStore, at: u64, bytes: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < bytes.len() {
            let to = at + done as u64;
            if self.held > 0 && self.held_at + self.held as u64 != to {
                self.write_held(store)?;
            }
            if self.held == 0 {
                self.held_at = to;
            }
            let n = (bytes.len() - done).min(self.copy.len() - self.held);
            self.copy[self.held..self.held + n].copy_from_slice(&bytes[done..done + n]);
            self.held += n;
            done += n;
            if self.held == self.copy.len() {
                self.write_held(store)?;
            }
        }
        Ok(())
    }

    /// Writes what the open frame holds in the buffer.
    fn write_held(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.held > 0 {
            store.write_at(self.held_at, &self.copy[..self.held])?;
            self.held_at += self.held as u64;
            self.held = 0;
        }
        Ok(())
    }

    /// Opens a frame of at least `least` bytes at the next ring room.
    fn open_frame(&mut self, store: &mut dyn TileStore, least: u64) -> Result<()> {
        let at = self.place(store, least)?;
        let (room, _) = self.ring.room(self.header.tail, at);
        let reserved = room.min(self.frame_limit()).max(least);
        self.frame = Some(OpenFrame {
            at,
            len: FRAME_HEADER,
            reserved,
            count: 0,
            hasher: crc32fast::Hasher::new(),
        });
        self.open_ring_bytes += FRAME_HEADER;
        Ok(())
    }

    /// Seals the open frame: padding, header and checksum. The commit frame
    /// when `commit`.
    fn close_frame(&mut self, store: &mut dyn TileStore, commit: bool) -> Result<()> {
        let Some(mut frame) = self.frame.take() else {
            return Ok(());
        };
        let len = (frame.len + 4).next_multiple_of(8);
        let pad = crate::width::small(len - 4 - frame.len);
        frame.hasher.update(&ZEROS[..pad]);
        let header = frame_header(
            if commit { FRAME_COMMIT } else { 0 },
            self.header.published + 1,
            len,
            frame.count,
            self.space.requires,
        );
        let mut crc = crc32fast::Hasher::new();
        crc.update(&header);
        crc.combine(&frame.hasher);
        let mut tail = [0u8; 12];
        tail[pad..pad + 4].copy_from_slice(&crc.finalize().to_le_bytes());
        self.hold(store, self.ring.at(frame.at + frame.len), &tail[..pad + 4])?;
        // The header goes in front of what is held when the whole frame is
        // still in the buffer, so a small frame is one write.
        let header_at = self.ring.at(frame.at);
        if self.held > 0
            && self.held_at == header_at + FRAME_HEADER
            && self.held + header.len() <= self.copy.len()
        {
            self.held_at = header_at;
            let hl = header.len();
            self.copy.copy_within(..self.held, hl);
            self.copy[..hl].copy_from_slice(&header);
            self.held += hl;
            self.write_held(store)?;
        } else {
            self.write_held(store)?;
            store.write_at(header_at, &header)?;
        }
        self.stats.frames += 1;
        self.open_ring_bytes += len - frame.len;
        self.cursor = frame.at + len;
        if self.cursor == self.ring.area() {
            self.cursor = 0;
        }
        Ok(())
    }

    /// Streams the rest of the open generation into an index. Everything
    /// published is put in place first: the open generation's index is then
    /// the only one a writer adds, and the regions earlier ones used are free.
    fn start_index(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.close_frame(store, false)?;
        if self.header.published > self.header.checkpointed {
            self.checkpoint(store, u64::MAX)?;
        }
        debug_assert!(self.layers.indexes().len() < MAX_INDEXES);
        let generation = self.header.published + 1;
        let reclaimed = self.header.reclaimed;
        let space = &mut self.space;
        let cache = self
            .cache
            .get_mut()
            .map_err(|_| Error::InvalidMatrixLayout)?;
        let building = Building::new(cache, store, generation, &mut |store, len| {
            space.reserve(store, len, generation, reclaimed)
        })?;
        self.building = Some(building);
        self.stats.indexed += 1;
        self.note_space();
        Ok(())
    }

    /// Writes the range to overflow space and records it in the open index.
    fn stage_in_index(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        src: Src<'_>,
    ) -> Result<()> {
        let generation = self.header.published + 1;
        let reclaimed = self.header.reclaimed;
        let mut pending = Some((target, src));
        let mut head: [u8; 8];
        while let Some((target, src)) = pending.take() {
            let len = src.len();
            let at = self.space.reserve(store, len, generation, reclaimed)?;
            self.write_src(store, at, src, false)?;
            self.appended = true;
            let space = &mut self.space;
            let cache = self
                .cache
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?;
            let building = self.building.as_mut().ok_or(Error::InvalidMatrixLayout)?;
            building.insert(
                cache,
                store,
                &mut |store, len| space.reserve(store, len, generation, reclaimed),
                target,
                at,
                crate::width::field32(crate::width::small(len))?,
            )?;
            // Pages written back now keep room for clean pages a read can
            // evict.
            if cache.dirty() > cache.pages() / 2 {
                cache.flush(store)?;
            }
            // A new region becomes the head of the list in the same
            // publication.
            if self.space.head_moved {
                self.space.head_moved = false;
                head = self.space.head.to_le_bytes();
                pending = Some((self.head_at, Src::Bytes(&head)));
            }
        }
        self.note_space();
        Ok(())
    }

    fn note_space(&mut self) {
        self.stats.overflow_appended = self.space.appended;
        self.stats.overflow_reused = self.space.reused;
    }
    /// Writes a new tile or directory page of `len` bytes, `prefix` and then
    /// zeros: into free overflow space when a region has room, else as a new
    /// record. Either way nothing published points at it yet, so it is
    /// fresh until the next publication.
    pub(crate) fn place_record(
        &mut self,
        store: &mut dyn TileStore,
        kind: RecordKind,
        prefix: &[u8],
        len: u64,
    ) -> Result<u64> {
        let at = match self.carve(store, len)? {
            Some(at) => {
                // The space held older data: zero all of it first.
                let mut done = 0u64;
                while done < len {
                    let n = crate::width::bounded(len - done, ZEROS.len());
                    store.write_at(at + done, &ZEROS[..n])?;
                    done += n as u64;
                }
                store.write_at(at, prefix)?;
                self.stats.carved += 1;
                at
            }
            None => {
                let at = store.append_zeroed(kind, prefix, len)?;
                self.space.requires = self.space.requires.max(at + len);
                at
            }
        };
        self.appended = true;
        Ok(at)
    }
    /// Calls `visit` with every published range not yet in place, in the
    /// order they are laid.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &self,
        store: &dyn TileSource,
        visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
    ) -> Result<()> {
        let mut cache = self.cache()?;
        super::view::for_each_layered(
            &self.layers,
            &mut cache,
            store,
            self.header.checkpointed,
            self.header.published,
            None,
            visit,
        )
    }

    /// Takes `len` bytes (rounded up to 8) from the end of the free space of
    /// the reclaimed overflow region with the least room that holds them, and
    /// stages the region's new carved count. `None` when no region can.
    fn carve(&mut self, store: &mut dyn TileStore, len: u64) -> Result<Option<u64>> {
        let len = len.next_multiple_of(8);
        let reclaimed = self.header.reclaimed;
        let Some(index) = self
            .space
            .regions()
            .iter()
            .enumerate()
            .filter(|(_, region)| region.generation <= reclaimed && region.free_room() >= len)
            .min_by_key(|(_, region)| region.free_room())
            .map(|(index, _)| index)
        else {
            return Ok(None);
        };
        let region = &mut self.space.regions[index];
        region.carved += len;
        let (region_at, next, region_len, carved) =
            (region.at, region.next, region.len, region.carved);
        self.stage(
            store,
            region_at,
            &encode_overflow_header(next, region_len, carved),
        )?;
        Ok(Some(region_at + region_len - carved))
    }

    /// Forgets everything staged since the last publication.
    #[cfg(test)]
    pub(crate) fn discard(&mut self) {
        let open = self.header.published + 1;
        self.layers
            .overlay
            .retain(|_, entry| entry.generation != open);
        if let Ok(cache) = self.cache.get_mut() {
            cache.forget(open);
        }
        self.frame = None;
        self.held = 0;
        self.building = None;
        self.spilled = false;
        self.cursor = self.header.head;
        self.open_ring_bytes = 0;
        self.space.filling = None;
        self.space.streamed = 0;
    }
}
