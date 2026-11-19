// `FormatSpec::validate` and the checks it runs on layouts, segments,
// matrices and compression. Included by `format.rs`, whose module it shares.
impl FormatSpec {
    pub fn validate(self) -> Result<()> {
        if self.read_limits.read_validation.is_some_and(|p| p.data_crc)
            && self.integrity_policy == IntegrityPolicy::None
        {
            return Err(Error::InvalidFormatSpec(
                "data CRC verification requires stored checksums",
            ));
        }
        if self.magic.is_empty() {
            return Err(Error::InvalidFormatSpec("magic must not be empty"));
        }
        if let Some(extension) = self.extension {
            if extension.is_empty() {
                return Err(Error::InvalidFormatSpec("extension must not be empty"));
            }
            if extension.contains(['/', '\\']) {
                return Err(Error::InvalidFormatSpec(
                    "extension must not contain path separators",
                ));
            }
        }
        if self.index_policy.keyed_offset_chain && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "keyed_offset_chain requires block_offset_chain",
            ));
        }
        // A full checkpoint answers "what is the whole index?" by serialising
        // every entry from scratch; a segment answers it incrementally, and
        // unlike the checkpoint it is the thing open actually reads - open
        // validates a checkpoint it walks past and discards its entries. With
        // the chain on, `checkpoint_on_flush` writes a periodic full copy of
        // the index that nothing will ever read, and stops fitting in a record
        // at all past `(max_record_payload_len - 22) / 73` entries. Refusing is
        // louder than quietly clearing it: a format that declared the
        // checkpoint asked for something, and silently not delivering it is how
        // a policy becomes folklore.
        // A non-resident block is reachable only through its footer chain -
        // nothing else in varlit can find its records once they are out of the
        // resident index - so the combination without a chain is not a
        // trade-off to offer, it is a block written into a hole.
        if self.has_non_resident_blocks() && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "a non-resident block requires block_offset_chain to stay reachable",
            ));
        }
        for descriptor in self.block_residency {
            if self.block(descriptor.block_id).is_none() {
                return Err(Error::InvalidFormatSpec(
                    "residency declared for a block the format does not declare",
                ));
            }
        }
        if self.index_policy.segment_on_flush && self.index_policy.checkpoint_on_flush {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush supersedes checkpoint_on_flush; declare one",
            ));
        }
        if self.index_policy.segment_on_flush && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush requires block_offset_chain",
            ));
        }
        // The digest hands out each block's tail offset, and the only way to
        // use one is to walk back through `prev_same_block_offset`. Without the
        // chain those offsets name a record with no predecessor link, so the
        // digest would be answering "where does this block end" with something
        // nothing can follow.
        if self.index_policy.open_digest_on_flush && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "open_digest_on_flush requires block_offset_chain",
            ));
        }
        // `MarkTail` writes its verdict into the footer word and needs a
        // commit marker to name the boundary it stops at. Both are refusals
        // rather than silent degradations to `TruncateTail`, because the
        // difference between the two is whether a crashed writer's records
        // survive — not a detail to discover later.
        if matches!(self.recovery_policy, RecoveryPolicy::MarkTail) {
            if !matches!(self.liveness_policy, LivenessPolicy::FooterFlags) {
                return Err(Error::InvalidFormatSpec(
                    "recovery: mark_tail requires liveness: footer_flags",
                ));
            }
            if !self.commit_policy.is_transaction_marker() {
                return Err(Error::InvalidFormatSpec(
                    "recovery: mark_tail requires a transaction_marker commit policy",
                ));
            }
        }
        // The mutable word IS the footer's trailing field, so a format that
        // writes no footer has nowhere to put it. Refused rather than silently
        // inert: an option that does nothing is the shape this project treats
        // as a defect, not a convenience.
        if matches!(self.liveness_policy, LivenessPolicy::FooterFlags)
            && !self.spec_needs_record_footer()
        {
            return Err(Error::InvalidFormatSpec(
                "liveness: footer_flags requires a record footer (declare a commit policy or an offset chain)",
            ));
        }
        // Same reason, and the region is the same three facts as the digest.
        if self.index_policy.header_tails && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "header_tails requires block_offset_chain",
            ));
        }
        // The editable header region, validated. Everything here is refused
        // rather than approximated, because each one is a way for the region to
        // exist while meaning nothing.
        if self.header_slots.is_declared() {
            // A custom physical layout does not have varlit's file header at
            // all, so there is nothing to reserve the region inside.
            if !self.layout.is_varlit_native_default() {
                return Err(Error::InvalidFormatSpec(
                    "header_slots requires the varlit_native layout",
                ));
            }
            // The region shares the file-header extension budget with every
            // other block. Checked against the framed length, not the capacity,
            // because the framing is what occupies the budget. The budget is
            // 64 KiB unless this format declared `limits { header_extension }`,
            // and the *reader* enforces the same number, so a format that
            // reserves more than it declares is refused here rather than
            // written and then rejected at open.
            if header_slots_region_len(self)
                > self.read_limits.effective_max_file_header_extension_len()
            {
                return Err(Error::InvalidFormatSpec(
                    "header_slots capacity exceeds the file-header extension limit",
                ));
            }
            // A region nothing may be written into is a region that costs its
            // bytes and its schema hash to do nothing, which is the shape
            // "capabilities are options" exists to refuse.
            if self.header_slots.blocks.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "header_slots declares no blocks; declare at least one or drop the region",
                ));
            }
            let mut seen = 0usize;
            while seen < self.header_slots.blocks.len() {
                let id = self.header_slots.blocks[seen];
                // The id has to name a block this format declares, or a write
                // of it could never be encoded and a read could never be typed.
                if !self.blocks.iter().any(|block| block.id == id) {
                    return Err(Error::InvalidFormatSpec(
                        "header_slots names a block the format does not declare",
                    ));
                }
                // A matrix block is not a value with an encoded payload, and a
                // header entry is exactly that.
                if let Some(block) = self.blocks.iter().find(|block| block.id == id)
                    && matches!(block.kind, BlockKind::Matrix)
                {
                    return Err(Error::InvalidFormatSpec(
                        "header_slots cannot hold a matrix block",
                    ));
                }
                if self.header_slots.blocks[..seen].contains(&id) {
                    return Err(Error::InvalidFormatSpec(
                        "header_slots names the same block twice",
                    ));
                }
                seen += 1;
            }
        }
        // A record scan reads each record's own header, so damage to one record
        // misindexes that record. A segment chain reads one payload that
        // describes many records, so damage to it misindexes records whose own
        // bytes are intact - and open would have no way to notice. The record
        // checksum is what closes that, and the chain walk verifies it on every
        // link, so the chain is available exactly where there is a checksum to
        // verify.
        if self.index_policy.segment_on_flush
            && !matches!(
                self.integrity_policy,
                IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
            )
        {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush requires a crc32 integrity policy",
            ));
        }
        // The region is written *in place* over a live file, so unlike every
        // other structure here its failure mode is a half-written slot rather
        // than a short file. A per-slot checksum is what distinguishes the two,
        // and it is the only thing that does: the slot's length never changes,
        // so a torn write leaves a region that frames perfectly and describes
        // records that are not there.
        if self.index_policy.header_tails
            && !matches!(
                self.integrity_policy,
                IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
            )
        {
            return Err(Error::InvalidFormatSpec(
                "header_tails requires a crc32 integrity policy",
            ));
        }
        // Two persisted answers to the same question, with different safety
        // properties and no precedence rule that is safe in both directions.
        // The digest is bound to the file by ending exactly at EOF, which the
        // header table has to *earn* by walking forward from the commit marker
        // it names; "whichever is newer" would sometimes prefer the weaker one.
        // Rather than invent an order, declare one.
        if self.index_policy.header_tails && self.index_policy.open_digest_on_flush {
            return Err(Error::InvalidFormatSpec(
                "header_tails and open_digest_on_flush are two answers to the same question; declare one",
            ));
        }
        // The slot is written for a *commit marker*, and a marker is the one
        // thing these policies never produce: `commit_durable` refuses them
        // outright and `flush` writes a marker only under `marker_on_flush`.
        // With no marker there is nothing for the slot to name, so the region
        // stays cold for the life of the file — measured, both slots at
        // `count = 0` after five flushes — and the option costs its bytes and
        // its schema hash while doing nothing. Refusing it is what keeps a
        // capability a capability.
        if self.commit_policy.single_barrier()
            && self.integrity_policy != IntegrityPolicy::Crc32WithHeader
        {
            return Err(Error::InvalidFormatSpec(
                "a single-barrier commit requires crc32_with_header integrity",
            ));
        }
        // `MarkTail` keeps a crashed writer's tail by committing it. Under a
        // single barrier that tail may be torn, so it is truncated instead.
        if self.commit_policy.single_barrier()
            && matches!(self.recovery_policy, RecoveryPolicy::MarkTail)
        {
            return Err(Error::InvalidFormatSpec(
                "a single-barrier commit cannot use mark_tail recovery",
            ));
        }
        if self.index_policy.header_tails && !self.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "header_tails requires a transaction_marker commit policy",
            ));
        }
        if let Some(keyed) = self.keyed_storage {
            keyed.validate()?;
            if self.index_policy.keyed_offset_chain {
                return Err(Error::InvalidFormatSpec(
                    "keyed storage keeps one version per key: it cannot keep a keyed_offset_chain",
                ));
            }
            if self.has_matrix_blocks() {
                return Err(Error::InvalidFormatSpec(
                    "keyed storage and matrix blocks in one format are not supported yet",
                ));
            }
        }
        for (index, block) in self.blocks.iter().enumerate() {
            if block.id >= RESERVED_BLOCK_ID_START {
                return Err(Error::InvalidFormatSpec("user block id is reserved"));
            }
            if block.version == 0 {
                return Err(Error::InvalidFormatSpec("block version must be non-zero"));
            }
            if block.name.is_empty() {
                return Err(Error::InvalidFormatSpec("block name must not be empty"));
            }
            if block.kind == BlockKind::Matrix
                && !self
                    .matrix_blocks
                    .iter()
                    .any(|matrix| matrix.block_id == block.id)
            {
                return Err(Error::InvalidFormatSpec(
                    "matrix block missing matrix descriptor",
                ));
            }
            for (field_index, field) in block.fields.iter().enumerate() {
                if field.id == 0 {
                    return Err(Error::InvalidFormatSpec("field id must be non-zero"));
                }
                if field.name.is_empty() {
                    return Err(Error::InvalidFormatSpec("field name must not be empty"));
                }
                for other in &block.fields[(field_index + 1)..] {
                    if field.id == other.id {
                        return Err(Error::InvalidFormatSpec("duplicate field id"));
                    }
                }
            }
            for other in &self.blocks[(index + 1)..] {
                if block.id == other.id {
                    return Err(Error::InvalidFormatSpec("duplicate block id"));
                }
            }
        }
        for (index, identity) in self.block_identities.iter().enumerate() {
            if !self.blocks.iter().any(|block| block.id == identity.0) {
                return Err(Error::InvalidFormatSpec(
                    "block identity references an unregistered block",
                ));
            }
            for other in &self.block_identities[(index + 1)..] {
                if identity.0 == other.0 {
                    return Err(Error::InvalidFormatSpec("duplicate block identity"));
                }
            }
        }
        self.validate_matrix_spec()?;
        self.validate_layout_spec()?;
        if let CompressionPolicy::VariableBlocks(compression) = self.compression_policy {
            self.validate_variable_compression(compression)?;
        }
        for (index, descriptor) in self.block_compression.iter().enumerate() {
            let block = self
                .block(descriptor.block_id)
                .ok_or(Error::InvalidFormatSpec(
                    "block compression id is not registered",
                ))?;
            if block.kind != BlockKind::Variable {
                return Err(Error::InvalidFormatSpec(
                    "block compression requires a variable block",
                ));
            }
            if descriptor.compression.header_mode != CompressionHeaderMode::RecordExplicit {
                return Err(Error::InvalidFormatSpec(
                    "block compression requires record_explicit header",
                ));
            }
            self.validate_variable_compression(descriptor.compression)?;
            for other in &self.block_compression[(index + 1)..] {
                if descriptor.block_id == other.block_id {
                    return Err(Error::InvalidFormatSpec("duplicate block compression"));
                }
            }
        }
        Ok(())
    }

    pub const fn spec_needs_record_footer(self) -> bool {
        self.commit_policy.requires_record_footer() || self.index_policy.requires_record_footer()
    }

    pub const fn has_matrix_blocks(self) -> bool {
        !self.matrix_blocks.is_empty()
    }

    fn validate_layout_spec(self) -> Result<()> {
        match self.layout.preset {
            LayoutPreset::VarlitNative => {
                if !self.layout.parts.is_empty() {
                    return Err(Error::InvalidFormatSpec(
                        "varlit_native layout must not declare custom parts",
                    ));
                }
                crate::native_layout::ensure_native_file_header_layout_contract(self)?;
                crate::native_layout::ensure_native_record_layout_contract()?;
            }
            LayoutPreset::None | LayoutPreset::Custom => {
                if self.has_matrix_blocks() {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support matrix blocks in this release",
                    ));
                }
                if self.commit_policy != CommitPolicy::None {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native commit policy in this release",
                    ));
                }
                if self.index_policy.checkpoint_on_flush
                    || self.index_policy.block_offset_chain
                    || self.index_policy.keyed_offset_chain
                {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native index records in this release",
                    ));
                }
                if self.manifest_policy != ManifestPolicy::None {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support embedded native manifests in this release",
                    ));
                }
                if self.compression_policy != CompressionPolicy::None
                    || !self.block_compression.is_empty()
                {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native record compression in this release",
                    ));
                }
                if self.layout.parts.is_empty() {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout requires at least one layout part",
                    ));
                }
            }
        }

        for (index, part) in self.layout.parts.iter().enumerate() {
            if part.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout part name must not be empty",
                ));
            }
            for other in &self.layout.parts[(index + 1)..] {
                if part.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate layout part name"));
                }
            }
            match part.kind {
                LayoutPartKind::FileHeader(header) => {
                    self.validate_layout_fields(header.fields)?;
                    if header
                        .fields
                        .iter()
                        .any(|field| matches!(field.source, LayoutFieldSource::Finalize(_)))
                    {
                        return Err(Error::InvalidFormatSpec(
                            "layout file_header does not support finalized fields in this release",
                        ));
                    }
                }
                LayoutPartKind::Segment(segment) => self.validate_segment_layout(segment)?,
                LayoutPartKind::LeadIn(lead_in) => self.validate_layout_fields(lead_in.fields)?,
                LayoutPartKind::Metadata(metadata) => {
                    if metadata.name.is_empty() {
                        return Err(Error::InvalidFormatSpec(
                            "layout metadata name must not be empty",
                        ));
                    }
                }
                LayoutPartKind::RawRegion(raw) => {
                    if raw.name.is_empty() {
                        return Err(Error::InvalidFormatSpec(
                            "layout raw region name must not be empty",
                        ));
                    }
                }
                LayoutPartKind::Footer(footer) => self.validate_layout_fields(footer.fields)?,
            }
        }
        if !matches!(self.layout.preset, LayoutPreset::VarlitNative) {
            crate::layout::validate_layout_segment_dispatch(self)?;
        }
        Ok(())
    }

    fn validate_segment_layout(self, segment: SegmentDescriptor) -> Result<()> {
        if segment.name.is_empty()
            || segment.lead_in.name.is_empty()
            || segment.metadata.name.is_empty()
            || segment.raw_region.name.is_empty()
        {
            return Err(Error::InvalidFormatSpec(
                "layout segment names must not be empty",
            ));
        }
        self.validate_layout_fields(segment.lead_in.fields)?;
        if let Some(footer) = segment.footer {
            if footer.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout footer name must not be empty",
                ));
            }
            self.validate_layout_fields(footer.fields)?;
        }
        let has_segment_end = segment.lead_in.fields.iter().any(|field| {
            matches!(
                field.source,
                LayoutFieldSource::Finalize(LayoutFinalize {
                    target: LayoutAnchor::SegmentEnd,
                    relative_to: LayoutAnchor::SegmentStart
                        | LayoutAnchor::AfterLeadIn
                        | LayoutAnchor::MetadataStart
                        | LayoutAnchor::RawRegionStart
                })
            )
        });
        let has_raw_start = segment.lead_in.fields.iter().any(|field| {
            matches!(
                field.source,
                LayoutFieldSource::Finalize(LayoutFinalize {
                    target: LayoutAnchor::RawRegionStart,
                    relative_to: LayoutAnchor::SegmentStart
                        | LayoutAnchor::AfterLeadIn
                        | LayoutAnchor::MetadataStart
                })
            )
        });
        if !has_segment_end || !has_raw_start {
            return Err(Error::InvalidFormatSpec(
                "layout segment requires scan-resolvable finalized segment_end and raw_region_start fields",
            ));
        }
        Ok(())
    }

    fn validate_layout_fields(self, fields: &[LayoutFieldDescriptor]) -> Result<()> {
        if fields.is_empty() {
            return Err(Error::InvalidFormatSpec("layout fields must not be empty"));
        }
        let mut total_len = 0u64;
        for (index, field) in fields.iter().enumerate() {
            if field.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout field name must not be empty",
                ));
            }
            let len = field.ty.byte_len();
            if len == 0 {
                return Err(Error::InvalidFormatSpec(
                    "layout field length must be non-zero",
                ));
            }
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::InvalidFormatSpec("layout fields overflow"))?;
            if let LayoutFieldSource::LiteralBytes(bytes) = field.source
                && !matches!(field.ty, LayoutFieldType::Bytes { len } if len == bytes.len() as u64)
            {
                return Err(Error::InvalidFormatSpec(
                    "literal bytes length must match layout field length",
                ));
            }
            if matches!(field.source, LayoutFieldSource::LiteralBytes(_))
                && !matches!(field.ty, LayoutFieldType::Bytes { .. })
            {
                return Err(Error::InvalidFormatSpec(
                    "literal bytes require a bytes layout field",
                ));
            }
            if matches!(field.source, LayoutFieldSource::LiteralI64(_))
                && !matches!(field.ty, LayoutFieldType::I64)
            {
                return Err(Error::InvalidFormatSpec(
                    "literal i64 requires an i64 layout field",
                ));
            }
            if matches!(field.source, LayoutFieldSource::Finalize(_))
                && matches!(field.ty, LayoutFieldType::Bytes { .. })
            {
                return Err(Error::InvalidFormatSpec(
                    "finalized layout fields must be numeric",
                ));
            }
            for other in &fields[(index + 1)..] {
                if field.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate layout field name"));
                }
            }
        }
        Ok(())
    }

    fn validate_matrix_spec(self) -> Result<()> {
        for (index, dimension) in self.matrix_dimensions.iter().enumerate() {
            if dimension.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix dimension name must not be empty",
                ));
            }
            for other in &self.matrix_dimensions[(index + 1)..] {
                if dimension.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix dimension"));
                }
            }
        }
        for (index, commit) in self.matrix_commits.iter().enumerate() {
            if commit.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix commit category name must not be empty",
                ));
            }
            for other in &self.matrix_commits[(index + 1)..] {
                if commit.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix commit category"));
                }
            }
        }
        for (index, block) in self.matrix_blocks.iter().enumerate() {
            if block.slot_stride == 0 {
                return Err(Error::InvalidFormatSpec(
                    "matrix slot_stride must be non-zero",
                ));
            }
            let [tile_rows, tile_cols] = block.effective_tile();
            let tile_cells = tile_rows.checked_mul(tile_cols);
            if tile_rows == 0
                || tile_cols == 0
                || tile_cells.is_none_or(|cells| cells > MATRIX_MAX_TILE_CELLS)
                || tile_cells
                    .and_then(|cells| cells.checked_mul(block.slot_stride.saturating_add(4)))
                    .is_none_or(|bytes| bytes > MATRIX_MAX_TILE_BYTES)
            {
                return Err(Error::InvalidFormatSpec(
                    "matrix tile must be non-empty and at most 2^20 cells and 64 MiB",
                ));
            }
            let descriptor = self.block(block.block_id).ok_or(Error::InvalidFormatSpec(
                "matrix block id is not registered",
            ))?;
            if descriptor.kind != BlockKind::Matrix {
                return Err(Error::InvalidFormatSpec(
                    "matrix descriptor points at a non-matrix block",
                ));
            }
            for dimension in block.dimensions {
                if !self
                    .matrix_dimensions
                    .iter()
                    .any(|candidate| candidate.name == dimension)
                {
                    return Err(Error::InvalidFormatSpec(
                        "matrix block references unknown dimension",
                    ));
                }
            }
            if !self.matrix_commits.iter().any(|commit| {
                commit.name == block.category && commit.kind == MatrixCommitKind::Cell
            }) {
                return Err(Error::InvalidFormatSpec(
                    "matrix block references unknown cell commit category",
                ));
            }
            for other in &self.matrix_blocks[(index + 1)..] {
                if block.block_id == other.block_id {
                    return Err(Error::InvalidFormatSpec(
                        "duplicate matrix block descriptor",
                    ));
                }
                if block.category == other.category {
                    return Err(Error::InvalidFormatSpec(
                        "matrix cell commit category must be unique per block",
                    ));
                }
            }
        }
        if let Some(growing) = self.growing_matrix {
            // Every refusal here exists so that "chunk k has the matrix
            // region's layout" is true by construction rather than by care.
            if self.matrix_blocks.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "a growing matrix dimension requires at least one matrix block",
                ));
            }
            if growing.rows_per_chunk == 0 {
                return Err(Error::InvalidFormatSpec(
                    "growing matrix rows_per_chunk must be non-zero",
                ));
            }
            if !self
                .matrix_dimensions
                .iter()
                .any(|candidate| candidate.name == growing.name)
            {
                return Err(Error::InvalidFormatSpec(
                    "growing matrix dimension is not declared",
                ));
            }
            for block in self.matrix_blocks {
                // `ordinal = key.scan * dim1 + key.ch`, so only dimension 0 can
                // grow without renumbering every cell already written.
                if block.dimensions[0] != growing.name {
                    return Err(Error::InvalidFormatSpec(
                        "the growing dimension must be dimension 0 of every matrix block",
                    ));
                }
            }
            // A chunk is a record: its payload has to be materialised whole to
            // be written, and a reader charges it against the same ceiling.
            if matches!(self.read_limits.max_record_payload_len, ReadLimit::Missing) {
                return Err(Error::InvalidFormatSpec(
                    "a growing matrix dimension requires a record payload limit",
                ));
            }
        }
        if self.chunk_compression.is_some() && self.growing_matrix.is_none() {
            return Err(Error::InvalidFormatSpec(
                "chunk compression requires a growing matrix dimension",
            ));
        }
        // Tiles are rewritten in place, which a compressed tile cannot be.
        if self.chunk_compression.is_some() {
            return Err(Error::InvalidFormatSpec(
                "matrix tiles are rewritten in place and cannot be compressed",
            ));
        }
        if !self.matrix_aux.is_empty() && self.matrix_blocks.is_empty() {
            return Err(Error::InvalidFormatSpec(
                "matrix aux requires at least one matrix block",
            ));
        }
        for (index, aux) in self.matrix_aux.iter().enumerate() {
            if aux.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix aux name must not be empty",
                ));
            }
            if aux.byte_len == 0 {
                return Err(Error::InvalidFormatSpec(
                    "matrix aux byte_len must be non-zero",
                ));
            }
            for other in &self.matrix_aux[(index + 1)..] {
                if aux.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix aux"));
                }
            }
        }
        Ok(())
    }

    fn validate_variable_compression(self, compression: VariableCompression) -> Result<()> {
        if compression.max_uncompressed_len == 0 {
            return Err(Error::InvalidFormatSpec(
                "compression max_uncompressed_len must be non-zero",
            ));
        }
        if compression.min_uncompressed_len > compression.max_uncompressed_len {
            return Err(Error::InvalidFormatSpec(
                "compression min_uncompressed_len exceeds max_uncompressed_len",
            ));
        }
        if compression.header_mode != CompressionHeaderMode::RecordExplicit
            && compression.max_uncompressed_len > u64::from(u32::MAX)
        {
            return Err(Error::InvalidFormatSpec(
                "file/contract compression max_uncompressed_len must fit u32",
            ));
        }
        if compression.header_mode == CompressionHeaderMode::FormatContract && self.schema_hash == 0
        {
            return Err(Error::InvalidFormatSpec(
                "format contract compression requires a non-zero schema_hash",
            ));
        }
        if compression.header_mode == CompressionHeaderMode::FormatContract
            && self.schema_hash != self.computed_schema_hash()
        {
            return Err(Error::InvalidFormatSpec(
                "format contract compression requires schema_hash to equal computed_schema_hash",
            ));
        }
        Ok(())
    }
}
