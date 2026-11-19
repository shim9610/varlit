// Checkpoints of a [`RingStore`]: copying published frames into place and
// raising the mark, so the ring has room again.

// Included by `ring_store.rs`, whose module it shares.

impl RingStore {
    /// The fewest, oldest published generations whose checkpoint leaves
    /// room for a frame of `need` bytes and half the ring: the newest of
    /// them and where its frames end.
    fn checkpoint_choice(&mut self, store: &mut dyn TileStore, need: u64) -> Result<(u64, u64)> {
        let header = self.header;
        // Which generations to put in place: the fewest, oldest ones that
        // leave room. Readers whose snapshots are newer keep reading them.
        let area = self.ring.area();
        let ring = self.ring;
        let cursor = self.cursor;
        let mut chosen: Option<(u64, u64)> = None;
        let mut last: Option<(u64, u64)> = None;
        wal::scan(
            store,
            ring,
            header.tail,
            header.checkpointed + 1,
            header.published,
            &mut self.scratch,
            &mut |_, event| {
                if let ScanEvent::Committed(generation, end) = event {
                    last = Some((generation, end));
                    if chosen.is_none() {
                        // The largest frame that fits once frames before
                        // `end` are gone, by the rule `place` then applies.
                        // Free half the ring at least, not just this frame's
                        // room: a ring kept just full would checkpoint (two
                        // more syncs) on every publish.
                        let (at_cursor, at_start) = ring.room(end, cursor);
                        let free_total = area - ring.used(end, cursor);
                        if (at_cursor.max(at_start) >= need && free_total >= area / 2)
                            || end == cursor
                        {
                            chosen = Some((generation, end));
                        }
                    }
                }
                Ok(())
            },
        )?;
        let Some(last) = last else {
            return Err(Error::InvalidMatrixLayout);
        };
        if last.0 != header.published {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(chosen.unwrap_or(last))
    }

    /// Puts the oldest published generations in place until at least `need`
    /// contiguous ring bytes and half the ring are free, or all of them when
    /// `need` is `u64::MAX`. Ranges stream from the ring and from indexes
    /// through the copy buffer; a range a newer generation of the same
    /// checkpoint rewrites whole is skipped.
    pub(crate) fn checkpoint(&mut self, store: &mut dyn TileStore, need: u64) -> Result<()> {
        // The copy buffer is about to coalesce the checkpoint's writes.
        self.write_held(store)?;
        let header = self.header;
        let ring = self.ring;
        if header.published == header.checkpointed {
            return Ok(());
        }
        // Everything published: the header says where it ends, so the ring
        // is read once, to copy it.
        let (target, new_tail) = if need == u64::MAX {
            (header.published, header.head)
        } else {
            self.checkpoint_choice(store, need)?
        };

        // Readers must see the mark before anything in place changes.
        let mut marked = self.header;
        marked.mark = marked.mark.max(target);
        self.write_header(store, marked)?;
        {
            let cache = self
                .cache
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?;
            // Only ring ranges, newest per target in the overlay: with an
            // index layer the overlay is not the whole story, and every
            // range is copied, checked, in order.
            let dedup = self.layers.indexes().is_empty();
            let overlay = &self.layers.overlay;
            let mut out = Coalescer {
                buf: &mut self.copy,
                at: 0,
                len: 0,
                writes: 0,
            };
            let superseded = |generation: u64, at: u64, len: u32| {
                // A newer generation put in place by this same checkpoint
                // rewrites all of these bytes: copying them would be
                // overwritten in the same pass.
                overlay.get(at).is_some_and(|newest| {
                    newest.generation > generation
                        && newest.generation <= target
                        && newest.len >= len
                })
            };
            if dedup {
                // Only ring ranges, all published by this writer or checked
                // when it opened: one pass over the ring, in ring order
                // (generation after generation, so the newest bytes land
                // last), straight from the frames to their places.
                wal::copy_committed(
                    store,
                    ring,
                    header.tail,
                    new_tail,
                    &mut self.scratch,
                    &mut |generation, at, len| superseded(generation, at, len),
                    &mut |store, at, bytes| out.write(store, at, bytes),
                )?;
            } else {
                // In ring order: generation after generation, and within one its
                // ring ranges before its index, so the newest bytes land last.
                wal::scan(
                    store,
                    ring,
                    header.tail,
                    header.checkpointed + 1,
                    target,
                    &mut self.scratch,
                    &mut |store, event| match event {
                        ScanEvent::Entry(_, entry) => {
                            out.copy(store, entry.target, entry.at, u64::from(entry.len))
                        }
                        ScanEvent::Index(generation, root) => index::for_each(
                            cache,
                            Io::Write(store),
                            generation,
                            root,
                            &mut |io, target, at, len| match io {
                                Io::Write(store) => {
                                    out.copy(&mut **store, target, at, u64::from(len))
                                }
                                Io::Read(_) => Err(Error::InvalidMatrixLayout),
                            },
                        ),
                        ScanEvent::Committed(..) => Ok(()),
                    },
                )?;
            }
            out.flush(store)?;
            self.stats.in_place_writes += out.writes;
        }
        store.sync()?;
        self.stats.barriers += 1;

        let mut done = self.header;
        done.checkpointed = target;
        done.reclaimed = target;
        done.tail = new_tail;
        done.tail_gen = target + 1;
        self.write_header(store, done)?;
        // The new tail must be durable before its old space is reused, or a
        // crash could leave a header pointing at overwritten frames.
        store.sync()?;
        self.stats.barriers += 1;
        self.stats.checkpoints += 1;
        let cache = self
            .cache
            .get_mut()
            .map_err(|_| Error::InvalidMatrixLayout)?;
        self.layers.drop_in_place(target, cache);
        Ok(())
    }

    /// Puts every published generation in place and empties the ring.
    #[cfg(test)]
    pub(crate) fn checkpoint_all(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.checkpoint(store, u64::MAX)
    }

    /// Finds what the ring holds: every committed publication after the
    /// last checkpoint becomes this writer's overlay, as a reader's would be.
    /// Nothing is written in place, so a reader's snapshot stays readable
    /// across a writer reopen; frames of a publication that never committed
    /// are simply overwritten by the next one.
    fn recover(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let header = self.header;
        self.layers.clear();
        let layers = &mut self.layers;
        let end = wal::scan(
            store,
            self.ring,
            header.tail,
            header.checkpointed + 1,
            u64::MAX,
            &mut self.scratch,
            &mut |_, event| match event {
                ScanEvent::Entry(generation, entry) => {
                    let kept = layers.insert(
                        entry.target,
                        OverlayEntry {
                            generation,
                            at: entry.at,
                            len: entry.len,
                        },
                        header.checkpointed,
                    );
                    if kept {
                        Ok(())
                    } else {
                        Err(Error::InvalidMatrixLayout)
                    }
                }
                ScanEvent::Index(generation, root) => layers.push_index(generation, root),
                ScanEvent::Committed(..) => Ok(()),
            },
        )?;
        // The frames are the authority on what was published; a header
        // written after them may not have reached the disk.
        let mut found = header;
        found.published = end.published.max(header.checkpointed);
        found.head = if end.published > header.checkpointed {
            end.head
        } else {
            header.tail
        };
        self.cursor = found.head;
        if found != header {
            self.write_header(store, found)?;
        }
        Ok(())
    }

    /// Finds the overflow regions a reopened matrix already has, walking the
    /// list from the head the metadata record points at. Their data may
    /// belong to a publication not yet in place, so each is in use until
    /// everything published now is reclaimed. A header that does not check
    /// out ends the walk: the regions past it are left alone, never reused.
    fn adopt_overflow(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let mut head = [0u8; 8];
        self.read(store, self.head_at, &mut head)?;
        self.space.head = u64::from_le_bytes(head);
        let end = store.len()?;
        let mut at = self.space.head;
        // Each region points at an older, earlier one, so the walk ends.
        let mut before = u64::MAX;
        while at != 0 && at < before && at.checked_add(OVERFLOW_HEADER).is_some_and(|e| e <= end) {
            let mut bytes = [0u8; crate::width::small(OVERFLOW_HEADER)];
            self.read(store, at, &mut bytes)?;
            let Some((next, len, carved)) = decode_overflow_header(&bytes) else {
                break;
            };
            if at.checked_add(len).is_none_or(|e| e > end) {
                break;
            }
            let tracked = self.space.track_oldest(OverflowRegion {
                at,
                len,
                generation: self.header.published,
                carved,
                carved_published: carved,
                next,
            });
            if !tracked {
                break;
            }
            before = at;
            at = next;
        }
        Ok(())
    }

    fn write_header(&mut self, store: &mut dyn TileStore, mut header: RingHeader) -> Result<()> {
        header.seq = self.header.seq + 1;
        store.write_at(self.ring.slot_for(header.seq), &header.encode())?;
        self.header = header;
        Ok(())
    }
}
