// The `FormatSpecBuilder` setters and `build`. Included by `format.rs`, whose
// module it shares.
impl FormatSpecBuilder {
    pub const fn new() -> Self {
        Self {
            magic: None,
            version: 1,
            endian: Endian::Little,
            schema_hash: 0,
            extension: None,
            index_policy: IndexPolicy::ScanOnOpen,
            commit_policy: CommitPolicy::None,
            integrity_policy: IntegrityPolicy::None,
            liveness_policy: LivenessPolicy::None,
            header_slots: HeaderSlots::NONE,
            recovery_policy: RecoveryPolicy::Strict,
            manifest_policy: ManifestPolicy::None,
            compression_policy: CompressionPolicy::None,
            block_compression: &[],
            block_residency: &[],
            blocks: &[],
            matrix_dimensions: &[],
            matrix_commits: &[],
            matrix_blocks: &[],
            matrix_aux: &[],
            growing_matrix: None,
            chunk_compression: None,
            block_identities: &[],
            layout: LayoutSpec::varlit_native(),
            read_limits: ReadLimits::MISSING,
        }
    }

    pub const fn magic(mut self, magic: &'static [u8]) -> Self {
        self.magic = Some(magic);
        self
    }

    pub const fn version(mut self, version: u16) -> Self {
        self.version = version;
        self
    }

    pub const fn endian(mut self, endian: Endian) -> Self {
        self.endian = endian;
        self
    }

    pub const fn schema_hash(mut self, schema_hash: u64) -> Self {
        self.schema_hash = schema_hash;
        self
    }

    pub const fn extension(mut self, extension: Option<&'static str>) -> Self {
        self.extension = extension;
        self
    }

    pub const fn index_policy(mut self, index_policy: IndexPolicy) -> Self {
        self.index_policy = index_policy;
        self
    }

    pub const fn commit_policy(mut self, commit_policy: CommitPolicy) -> Self {
        self.commit_policy = commit_policy;
        self
    }

    /// Reserves an editable fixed-size region in the file header.
    pub const fn header_slots(mut self, header_slots: HeaderSlots) -> Self {
        self.header_slots = header_slots;
        self
    }

    pub const fn liveness_policy(mut self, liveness_policy: LivenessPolicy) -> Self {
        self.liveness_policy = liveness_policy;
        self
    }

    pub const fn integrity_policy(mut self, integrity_policy: IntegrityPolicy) -> Self {
        self.integrity_policy = integrity_policy;
        self
    }

    pub const fn recovery_policy(mut self, recovery_policy: RecoveryPolicy) -> Self {
        self.recovery_policy = recovery_policy;
        self
    }

    pub const fn manifest_policy(mut self, manifest_policy: ManifestPolicy) -> Self {
        self.manifest_policy = manifest_policy;
        self
    }

    pub const fn compression_policy(mut self, compression_policy: CompressionPolicy) -> Self {
        self.compression_policy = compression_policy;
        self
    }

    pub const fn block_compression(
        mut self,
        block_compression: &'static [BlockCompressionDescriptor],
    ) -> Self {
        self.block_compression = block_compression;
        self
    }

    pub const fn block_residency(
        mut self,
        block_residency: &'static [BlockResidencyDescriptor],
    ) -> Self {
        self.block_residency = block_residency;
        self
    }

    pub const fn blocks(mut self, blocks: &'static [BlockDescriptor]) -> Self {
        self.blocks = blocks;
        self
    }

    pub const fn matrix_spec(
        mut self,
        dimensions: &'static [MatrixDimensionDescriptor],
        commits: &'static [MatrixCommitDescriptor],
        blocks: &'static [MatrixBlockDescriptor],
    ) -> Self {
        self.matrix_dimensions = dimensions;
        self.matrix_commits = commits;
        self.matrix_blocks = blocks;
        self
    }

    pub const fn matrix_aux(mut self, aux: &'static [MatrixAuxDescriptor]) -> Self {
        self.matrix_aux = aux;
        self
    }

    /// See [`FormatSpec::block_identities`].
    pub const fn block_identities(
        mut self,
        block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    ) -> Self {
        self.block_identities = block_identities;
        self
    }

    pub const fn chunk_compression(mut self, compression: VariableCompression) -> Self {
        self.chunk_compression = Some(compression);
        self
    }

    pub const fn growing_matrix_dimension(
        mut self,
        name: &'static str,
        rows_per_chunk: u64,
    ) -> Self {
        self.growing_matrix = Some(GrowingMatrixDimension {
            name,
            rows_per_chunk,
        });
        self
    }

    pub const fn layout(mut self, layout: LayoutSpec) -> Self {
        self.layout = layout;
        self
    }

    pub const fn read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = read_limits;
        self
    }

    pub fn build(self) -> Result<FormatSpec> {
        let magic = self
            .magic
            .ok_or(Error::InvalidFormatSpec("missing magic"))?;
        let spec = FormatSpec::new(
            magic,
            self.version,
            self.endian,
            self.schema_hash,
            self.index_policy,
            self.integrity_policy,
            self.recovery_policy,
            self.manifest_policy,
            self.blocks,
        )
        .with_extension(self.extension)
        .with_commit_policy(self.commit_policy)
        // Both of these were settable on the builder and dropped here, so a
        // caller who reached for `FormatSpec::builder().liveness_policy(..)`
        // silently got `None`. Found while wiring `header_slots`, which the
        // same omission would have made inert the same way.
        .with_liveness_policy(self.liveness_policy)
        .with_header_slots(self.header_slots)
        .with_compression_policy(self.compression_policy)
        .with_block_compression(self.block_compression)
        .with_block_residency(self.block_residency)
        .with_matrix_spec(
            self.matrix_dimensions,
            self.matrix_commits,
            self.matrix_blocks,
        )
        .with_matrix_aux(self.matrix_aux)
        .with_optional_growing_matrix(self.growing_matrix)
        .with_optional_chunk_compression(self.chunk_compression)
        .with_block_identities(self.block_identities)
        .with_layout(self.layout)
        .with_read_limits(self.read_limits);
        spec.validate()?;
        Ok(spec)
    }
}
