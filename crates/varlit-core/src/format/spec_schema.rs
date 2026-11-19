// `FormatSpec` schema views: the debug dump, the computed schema hash and
// block identities. Included by `format.rs`, whose module it shares.
impl FormatSpec {
    pub fn schema_debug_dump(&self) -> String {
        let mut output = String::new();
        output.push_str("varlit schema\n");
        output.push_str(&format!("version: {}\n", self.version));
        output.push_str(&format!("endian: {:?}\n", self.endian));
        output.push_str(&format!("extension: {:?}\n", self.extension));
        output.push_str(&format!("schema_hash: {}\n", self.schema_hash));
        output.push_str(&format!(
            "computed_schema_hash: {}\n",
            self.computed_schema_hash()
        ));
        output.push_str(&format!("index_policy: {:?}\n", self.index_policy));
        output.push_str(&format!("commit_policy: {:?}\n", self.commit_policy));
        output.push_str(&format!("integrity_policy: {:?}\n", self.integrity_policy));
        output.push_str(&format!("liveness_policy: {:?}\n", self.liveness_policy));
        output.push_str(&format!("recovery_policy: {:?}\n", self.recovery_policy));
        output.push_str(&format!("manifest_policy: {:?}\n", self.manifest_policy));
        output.push_str(&format!(
            "compression_policy: {:?}\n",
            self.compression_policy
        ));
        output.push_str(&format!("layout_preset: {:?}\n", self.layout.preset));
        for part in self.layout.parts {
            output.push_str(&format!("layout_part {} {:?}\n", part.name, part.kind));
        }
        let effective_layout = self.effective_layout();
        output.push_str(&format!(
            "effective_layout_preset: {:?}\n",
            effective_layout.preset
        ));
        for part in &effective_layout.parts {
            output.push_str(&format!(
                "effective_layout_part {} {:?}\n",
                part.name, part.kind
            ));
        }
        for override_policy in self.block_compression {
            output.push_str(&format!(
                "block_compression {} {:?}\n",
                override_policy.block_id, override_policy.compression
            ));
        }
        if !self.matrix_dimensions.is_empty()
            || !self.matrix_commits.is_empty()
            || !self.matrix_blocks.is_empty()
            || !self.matrix_aux.is_empty()
        {
            output.push_str("matrix:\n");
            for dimension in self.matrix_dimensions {
                output.push_str(&format!("  dim {}\n", dimension.name));
            }
            for commit in self.matrix_commits {
                output.push_str(&format!("  commit {} {:?}\n", commit.name, commit.kind));
            }
            for block in self.matrix_blocks {
                output.push_str(&format!(
                    "  block {} dims [{}, {}] category {} stride {} tile {:?}\n",
                    block.block_id,
                    block.dimensions[0],
                    block.dimensions[1],
                    block.category,
                    block.slot_stride,
                    block.effective_tile()
                ));
            }
            for aux in self.matrix_aux {
                output.push_str(&format!("  aux {} bytes {}\n", aux.name, aux.byte_len));
            }
        }
        let mut blocks = self.blocks.to_vec();
        blocks.sort_by_key(|block| block.id);
        for block in blocks {
            output.push_str(&format!(
                "block {} {} v{} {:?}\n",
                block.id, block.name, block.version, block.kind
            ));
            // Declaration order: fixed/matrix payload bytes follow it, and
            // the v2 schema hash covers the encoding ordinal.
            for field in block.fields {
                output.push_str(&format!(
                    "  field {} {} {:?} {:?}\n",
                    field.id, field.name, field.wire_type, field.presence
                ));
            }
        }
        output
    }

    /// Deterministic hash of the wire-relevant schema declaration
    /// (algorithm version [`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`]).
    ///
    /// Version 2  additionally covers, per block: the field
    /// **encoding ordinal** (fixed/matrix payload bytes follow field
    /// declaration order, so two blocks with the same field id/name/type set
    /// in different declaration order must hash differently), and — when
    /// [`FormatSpec::block_identities`] is populated — the block's endian
    /// override, keyedness, and generated codec fingerprint.
    ///
    /// Version 3  inherits transitive custom-codec identity: the
    /// generated fingerprints in [`FormatSpec::block_identities`] resolve each
    /// field through [`crate::VarlitEncode::SCHEMA_ID`], so a nested codec that
    /// changes its emitted bytes changes this hash even when every declared
    /// type name stays the same.
    ///
    /// The computed value is stored in newly created files. Whether it is
    /// *compared* at open is a separate opt-in; see
    /// [`FormatSpec::schema_hash`].
    pub fn computed_schema_hash(&self) -> u64 {
        let mut hash = Fnv1a64::new();
        hash.write_bytes(b"varve-schema-v3");
        hash.write_u16(self.version);
        hash.write_u8(self.endian.to_byte());
        match self.extension {
            Some(extension) => {
                hash.write_u8(1);
                hash.write_str(extension);
            }
            None => hash.write_u8(0),
        }
        hash.write_u8(index_policy_hash_byte(self.index_policy));
        hash.write_u8(commit_policy_hash_byte(self.commit_policy));
        hash.write_u8(
            integrity_policy_hash_byte(self.integrity_policy)
                | liveness_policy_hash_bit(self.liveness_policy),
        );
        hash.write_u8(recovery_policy_hash_byte(self.recovery_policy));
        hash.write_u8(manifest_policy_hash_byte(self.manifest_policy));
        hash_compression_policy(&mut hash, self.compression_policy);
        let mut block_compression = self.block_compression.to_vec();
        block_compression.sort_by_key(|descriptor| descriptor.block_id);
        for descriptor in block_compression {
            hash.write_u32(descriptor.block_id);
            hash_variable_compression(&mut hash, descriptor.compression);
        }
        for dimension in self.matrix_dimensions {
            hash.write_str(dimension.name);
        }
        for commit in self.matrix_commits {
            hash.write_str(commit.name);
            hash.write_u8(matrix_commit_kind_hash_byte(commit.kind));
        }
        let mut matrix_blocks = self.matrix_blocks.to_vec();
        matrix_blocks.sort_by_key(|block| block.block_id);
        for block in matrix_blocks {
            hash.write_u32(block.block_id);
            hash.write_str(block.dimensions[0]);
            hash.write_str(block.dimensions[1]);
            hash.write_str(block.category);
            hash.write_bytes(&block.slot_stride.to_le_bytes());
            // Tiles decide where every cell lives: a reader assuming another
            // shape would read other cells' bytes.
            let tile = block.effective_tile();
            hash.write_bytes(b"matrix-tile-v1");
            hash.write_bytes(&tile[0].to_le_bytes());
            hash.write_bytes(&tile[1].to_le_bytes());
        }
        for aux in self.matrix_aux {
            hash.write_str(aux.name);
            hash.write_bytes(&aux.byte_len.to_le_bytes());
        }
        // Folded only when declared: a growing dimension changes which bytes a
        // writer produces, and a spec that declares none must hash to exactly
        // what it hashed to before this existed.
        if let Some(growing) = self.growing_matrix {
            hash.write_bytes(b"growing-matrix-v1");
            hash.write_str(growing.name);
            hash.write_bytes(&growing.rows_per_chunk.to_le_bytes());
        }
        // Folded only when declared: keyed records then live in the heap,
        // not in the log, and a reader of the other kind finds none.
        if let Some(keyed) = self.keyed_storage {
            hash.write_bytes(b"keyed-storage-v1");
            hash.write_bytes(&keyed.free_space_permille.to_le_bytes());
        }
        if let Some(compression) = self.chunk_compression {
            hash.write_bytes(b"chunk-compression-v1");
            hash.write_u8(compression_algorithm_hash_byte(compression.algorithm));
        }
        // Folded only when declared, like the growing dimension above: the
        // region is bytes in the header, so it moves `append_log_start` and
        // every record offset with it. A build that ignored the declaration
        // would compute a different start and misread the file, which is
        // exactly what a schema hash exists to stop. A spec that declares no
        // region hashes to what it hashed to before this existed.
        if self.header_slots.is_declared() {
            hash.write_bytes(b"header-slots-v1");
            hash.write_u32(self.header_slots.capacity);
            hash.write_u8(header_slot_integrity_hash_byte(self.header_slots.integrity));
            let mut ids = self.header_slots.blocks.to_vec();
            ids.sort_unstable();
            for id in ids {
                hash.write_u32(id);
            }
        }
        if !self.layout.is_varlit_native_default() {
            hash.write_bytes(b"layout-v1");
            hash_layout_spec(&mut hash, self.layout);
        }
        let mut blocks = self.blocks.to_vec();
        blocks.sort_by_key(|block| block.id);
        for block in blocks {
            hash.write_u32(block.id);
            hash.write_str(block.name);
            hash.write_u16(block.version);
            hash.write_u8(block_kind_hash_byte(block.kind));
            // Per-block identity: endian override, keyedness, and generated
            // codec fingerprint, with an explicit absence marker so a spec
            // without identities can never collide with one that has them.
            match self.block_identity(block.id) {
                Some((_, endian, keyed, fingerprint)) => {
                    hash.write_u8(1);
                    hash.write_u8(match endian {
                        Some(endian) => endian.to_byte(),
                        None => 0,
                    });
                    hash.write_u8(u8::from(keyed));
                    hash.write_bytes(&fingerprint.to_le_bytes());
                }
                None => hash.write_u8(0),
            }
            // Fixed/matrix encoding follows field declaration order, so hash
            // the ordinal alongside each field instead of sorting by id.
            hash.write_bytes(&(block.fields.len() as u64).to_le_bytes());
            for (ordinal, field) in block.fields.iter().enumerate() {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a block's field list is a static slice; its ordinals are far below 2^32"
                )]
                hash.write_u32(ordinal as u32);
                hash.write_u32(field.id);
                hash.write_str(field.name);
                hash.write_u16(field.wire_type as u16);
                hash.write_u8(field_presence_hash_byte(field.presence));
            }
        }
        hash.finish()
    }

    /// Returns the `(block_id, endian override, keyedness, fingerprint)`
    /// identity declared for `id`, if any; see
    /// [`FormatSpec::block_identities`].
    pub fn block_identity(&self, id: u32) -> Option<(u32, Option<Endian>, bool, u64)> {
        self.block_identities
            .iter()
            .copied()
            .find(|identity| identity.0 == id)
    }

    pub(crate) const fn read_validation(self) -> ReadValidation {
        self.read_limits
            .effective_read_validation(self.integrity_policy)
    }

    /// Explicit verification, recovery and rewrite admission must never inherit
    /// a caller's trusted payload-read policy.
    pub(crate) const fn with_strict_read_validation(mut self) -> Self {
        self.read_limits.read_validation = Some(ReadValidation {
            data_crc: !matches!(self.integrity_policy, IntegrityPolicy::None),
            structure: true,
        });
        self
    }
}
