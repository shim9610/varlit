// Commit flags and aux regions of a [`TiledMatrix`].

// Included by `api.rs`, whose module it shares.

impl TiledMatrix {
    fn single_at(&self, name: &str) -> Result<u64> {
        self.names
            .singles
            .iter()
            .find(|(single, _)| *single == name)
            .map(|(_, at)| self.meta + at)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))
    }

    pub(crate) fn is_single_committed(&self, source: &dyn TileSource, name: &str) -> Result<bool> {
        let mut unit = [0u8; 8];
        self.read(source, self.single_at(name)?, &mut unit)?;
        Ok(unit[0] != 0)
    }

    pub(crate) fn set_single_committed(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        value: bool,
    ) -> Result<()> {
        let at = self.single_at(name)?;
        let mut unit = [0u8; 8];
        unit[0] = u8::from(value);
        self.writer()?.engine.stage(store, at, &unit)
    }

    fn channel_at(&self, name: &str, channel: u64) -> Result<(u64, u8)> {
        let (_, at, channels) = self
            .names
            .per_channel
            .iter()
            .find(|(commit, ..)| *commit == name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))?;
        if channel >= *channels {
            return Err(Error::MatrixCommitMissing(name.to_string()));
        }
        Ok((self.meta + at + channel / 8, 1 << (channel % 8)))
    }

    pub(crate) fn is_channel_committed(
        &self,
        source: &dyn TileSource,
        name: &str,
        channel: u64,
    ) -> Result<bool> {
        let (at, mask) = self.channel_at(name, channel)?;
        let mut byte = [0u8; 1];
        self.read(source, at, &mut byte)?;
        Ok(byte[0] & mask != 0)
    }

    pub(crate) fn set_channel_committed(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        let (at, mask) = self.channel_at(name, channel)?;
        self.rewrite_extra(store, at, 1, &mut |bytes, _| {
            if value {
                bytes[0] |= mask;
            } else {
                bytes[0] &= !mask;
            }
        })
    }

    fn aux(&self, name: &str) -> Result<(u64, u64)> {
        self.names
            .aux
            .iter()
            .find(|(aux, ..)| *aux == name)
            .map(|(_, at, len)| (self.meta + at, *len))
            .ok_or_else(|| Error::MatrixAuxMissing(name.to_string()))
    }

    pub(crate) fn aux_len(&self, name: &str) -> Result<u64> {
        Ok(self.aux(name)?.1)
    }

    fn aux_range(&self, name: &str, offset: u64, len: u64) -> Result<u64> {
        let (at, aux_len) = self.aux(name)?;
        if offset.checked_add(len).is_none_or(|end| end > aux_len) {
            return Err(Error::MatrixAuxOutOfBounds {
                name: name.to_string(),
                offset,
                len,
                byte_len: aux_len,
            });
        }
        Ok(at + offset)
    }

    pub(crate) fn read_aux(
        &self,
        source: &dyn TileSource,
        name: &str,
        offset: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let at = self.aux_range(name, offset, out.len() as u64)?;
        self.read(source, at, out)
    }

    pub(crate) fn write_aux(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let at = self.aux_range(name, offset, bytes.len() as u64)?;
        self.rewrite_extra(store, at, bytes.len() as u64, &mut |piece, from| {
            piece.copy_from_slice(&bytes[from..from + piece.len()]);
        })
    }

    /// Rewrites `[at, at + len)` of the metadata area page by page: each
    /// touched [`EXTRA_PAGE`] is read, changed through `edit` (given exactly
    /// the requested range once all pages are gathered), and staged whole.
    fn rewrite_extra(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        len: u64,
        edit: &mut dyn FnMut(&mut [u8], usize),
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let base = self.meta + self.geom.extra_at();
        let first = base + (at - base) / EXTRA_PAGE * EXTRA_PAGE;
        let end = base + (at + len - base).div_ceil(EXTRA_PAGE) * EXTRA_PAGE;
        // One page at a time: read, edit the part of the range it holds,
        // stage it whole.
        let mut page = [0u8; crate::width::small(EXTRA_PAGE)];
        let mut page_at = first;
        while page_at < end {
            self.read(store, page_at, &mut page)?;
            let from = at.max(page_at);
            let to = (at + len).min(page_at + EXTRA_PAGE);
            edit(
                &mut page[crate::width::index(from - page_at)?..crate::width::index(to - page_at)?],
                crate::width::index(from - at)?,
            );
            self.writer()?.engine.stage(store, page_at, &page)?;
            page_at += EXTRA_PAGE;
        }
        Ok(())
    }

    /// Checks every tile's header and commit bitmap. A damaged one is a
    /// `Recoverable` finding recommending its category be cleared.
    pub(crate) fn verify(&self, source: &dyn TileSource) -> Result<crate::MatrixRecoveryReport> {
        let mut report = crate::MatrixRecoveryReport {
            findings: Vec::new(),
            recommended_actions: Vec::new(),
        };
        let mut scratch = self.scratch()?;
        for (block, (_, category)) in self.names.blocks.iter().enumerate() {
            let geom = self.geom.blocks[block];
            let mut tiles = Vec::new();
            match &self.engine {
                Engine::Writer(writer) => writer.for_each_tile_ref(source, block, &mut |tile| {
                    push_fallible(&mut tiles, tile, "matrix tile list")
                })?,
                Engine::Reader(reader) => reader
                    .lock()
                    .map_err(|_| Error::InvalidMatrixLayout)?
                    .for_each_tile(source, block, &mut |tile| {
                        push_fallible(&mut tiles, tile, "matrix tile list")
                    })?,
            }
            let mut damaged = false;
            for tile in tiles {
                let mut header = [0u8; crate::width::small(super::geometry::TILE_HEADER_LEN)];
                self.read(source, tile, &mut header)?;
                let row = u64::from_le_bytes(header[16..24].try_into().expect("eight"));
                let col = u64::from_le_bytes(header[24..32].try_into().expect("eight"));
                let ok = geom
                    .check_tile_header(TileCoord { row, col }, &header)
                    .is_ok()
                    && self
                        .bitmap_unit(source, geom, tile, &mut scratch.bitmap)
                        .is_ok();
                if !ok {
                    damaged = true;
                    report
                        .findings
                        .try_reserve(1)
                        .map_err(|_| Error::AllocationFailed {
                            resource: "matrix recovery report",
                            requested: 64,
                        })?;
                    report.findings.push(crate::MatrixRecoveryFinding {
                        kind: crate::MatrixCorruptionKind::CommitMap,
                        severity: crate::MatrixCorruptionSeverity::Recoverable,
                        message: format!("tile at offset {tile} of category {category} is damaged"),
                    });
                }
            }
            if damaged {
                report
                    .recommended_actions
                    .try_reserve(1)
                    .map_err(|_| Error::AllocationFailed {
                        resource: "matrix recovery report",
                        requested: 64,
                    })?;
                report
                    .recommended_actions
                    .push(crate::MatrixRecoveryAction::ClearCategory {
                        category: (*category).to_string(),
                    });
            }
        }
        Ok(report)
    }

    /// Calls `visit(target, at, len)` with every range this handle's
    /// snapshot reads from the ring or an index instead of from its place,
    /// in the order they must be laid.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &self,
        source: &dyn TileSource,
        visit: &mut dyn FnMut(u64, u64, u32) -> Result<()>,
    ) -> Result<()> {
        let mut push = |target: u64, entry: super::view::OverlayEntry| -> Result<()> {
            visit(target, entry.at, entry.len)
        };
        match &self.engine {
            Engine::Writer(writer) => writer.engine.for_each_overlay(source, &mut push),
            Engine::Reader(reader) => reader
                .lock()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .reader
                .for_each_overlay(source, &mut push),
        }
    }
}
