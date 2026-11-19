// `FormatSpec` construction and settings: `new`, the `with_*` setters and
// the read-limit and trust adjustments. Included by `format.rs`, whose module
// it shares.
impl FormatSpec {
    /// Version of the [`FormatSpec::computed_schema_hash`] algorithm.
    ///
    /// v2 (, pre-1.0 breaking change): hashes the field encoding
    /// ordinal in declaration order and folds in per-block endian,
    /// keyedness, and generated codec identity from
    /// [`FormatSpec::block_identities`]. Values computed by v1 do not match
    /// v2 for any spec.
    ///
    /// v3 (, pre-1.0 breaking change): the generated block fingerprints
    /// folded in from [`FormatSpec::block_identities`] now resolve every
    /// field's codec through [`crate::VarlitEncode::SCHEMA_ID`] instead of the
    /// field type's source spelling, so two custom nested codecs that spell
    /// their field type identically but emit different bytes no longer share
    /// a hash. Values computed by v1 or v2 do not match v3 for any spec.
    pub const SCHEMA_HASH_ALGORITHM_VERSION: u16 = 3;

    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        magic: &'static [u8],
        version: u16,
        endian: Endian,
        schema_hash: u64,
        index_policy: IndexPolicy,
        integrity_policy: IntegrityPolicy,
        recovery_policy: RecoveryPolicy,
        manifest_policy: ManifestPolicy,
        blocks: &'static [BlockDescriptor],
    ) -> Self {
        Self {
            magic,
            version,
            endian,
            schema_hash,
            extension: None,
            index_policy,
            commit_policy: CommitPolicy::None,
            integrity_policy,
            liveness_policy: LivenessPolicy::None,
            header_slots: HeaderSlots::NONE,
            recovery_policy,
            manifest_policy,
            compression_policy: CompressionPolicy::None,
            block_compression: &[],
            block_residency: &[],
            blocks,
            matrix_dimensions: &[],
            matrix_commits: &[],
            matrix_blocks: &[],
            matrix_aux: &[],
            growing_matrix: None,
            chunk_compression: None,
            matrix_ring_bytes: None,
            keyed_storage: None,
            block_identities: &[],
            layout: LayoutSpec::varlit_native(),
            read_limits: ReadLimits::MISSING,
            anchors: crate::AnchorPolicy::DEFAULT,
        }
    }

    /// The anchors files of this spec are created with: its policy, except
    /// that a matrix format has none. Matrix cells are addressed by
    /// arithmetic already, and records after the matrix region are found
    /// through the chunk directory, so anchors would buy nothing there; and
    /// without them an anchored file's anchors always count from the end of
    /// its header, which the header alone determines.
    pub const fn effective_anchors(&self) -> crate::AnchorPolicy {
        self.anchors
    }

    /// Sets whether files this spec creates carry anchors.
    pub const fn with_anchors(mut self, anchors: crate::AnchorPolicy) -> Self {
        self.anchors = anchors;
        self
    }

    pub const fn with_extension(mut self, extension: Option<&'static str>) -> Self {
        self.extension = extension;
        self
    }

    /// Reserves an editable fixed-size region in the file header.
    ///
    /// Changes the computed schema hash, because the region moves
    /// `append_log_start`; see [`HeaderSlots`] for why that is not avoidable.
    pub const fn with_header_slots(mut self, header_slots: HeaderSlots) -> Self {
        self.header_slots = header_slots;
        self
    }

    /// Pins [`FormatSpec::schema_hash`] to the computed hash, opting this
    /// format into the open-time schema-hash equality check. Without this
    /// (or an explicit non-zero literal), `schema_hash` stays `0` and the
    /// comparison is disabled; see [`FormatSpec::schema_hash`].
    pub fn with_computed_schema_hash(mut self) -> Self {
        self.schema_hash = self.computed_schema_hash();
        self
    }

    /// Attaches the per-block schema identities folded into
    /// [`FormatSpec::computed_schema_hash`]; see
    /// [`FormatSpec::block_identities`]. Call before
    /// [`FormatSpec::with_computed_schema_hash`] so the pinned hash covers
    /// the identities.
    pub const fn with_block_identities(
        mut self,
        block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    ) -> Self {
        self.block_identities = block_identities;
        self
    }

    pub const fn with_index_policy(mut self, index_policy: IndexPolicy) -> Self {
        self.index_policy = index_policy;
        self
    }

    pub const fn with_commit_policy(mut self, commit_policy: CommitPolicy) -> Self {
        self.commit_policy = commit_policy;
        self
    }

    /// See [`LivenessPolicy`]. Defaults to `None`, which is byte-identical
    /// to every file written before the policy existed.
    pub const fn with_liveness_policy(mut self, liveness_policy: LivenessPolicy) -> Self {
        self.liveness_policy = liveness_policy;
        self
    }

    pub const fn with_integrity_policy(mut self, integrity_policy: IntegrityPolicy) -> Self {
        self.integrity_policy = integrity_policy;
        self
    }

    pub const fn with_recovery_policy(mut self, recovery_policy: RecoveryPolicy) -> Self {
        self.recovery_policy = recovery_policy;
        self
    }

    pub const fn with_manifest_policy(mut self, manifest_policy: ManifestPolicy) -> Self {
        self.manifest_policy = manifest_policy;
        self
    }

    pub const fn with_compression_policy(mut self, compression_policy: CompressionPolicy) -> Self {
        self.compression_policy = compression_policy;
        self
    }

    pub const fn with_block_compression(
        mut self,
        block_compression: &'static [BlockCompressionDescriptor],
    ) -> Self {
        self.block_compression = block_compression;
        self
    }

    /// Declares which blocks are kept out of the resident index.
    pub const fn with_block_residency(
        mut self,
        block_residency: &'static [BlockResidencyDescriptor],
    ) -> Self {
        self.block_residency = block_residency;
        self
    }

    /// Whether records of `block_id` are mirrored in the resident index.
    pub fn block_is_resident(self, block_id: u32) -> bool {
        self.block_residency
            .iter()
            .find(|descriptor| descriptor.block_id == block_id)
            .is_none_or(|descriptor| descriptor.resident)
    }

    /// Declares the matrix dimension that grows past its declared extent.
    ///
    /// `rows_per_chunk` is the value `name` must be given at create. See
    /// [`GrowingMatrixDimension`].
    pub const fn with_growing_matrix_dimension(
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

    pub(crate) const fn with_optional_growing_matrix(
        mut self,
        growing: Option<GrowingMatrixDimension>,
    ) -> Self {
        self.growing_matrix = growing;
        self
    }

    /// Compresses each matrix block's slot region inside a chunk record.
    ///
    /// A chunk is written whole, so an unfilled one costs its full size on
    /// disk regardless of occupancy. Compression and `rows_per_chunk` control
    /// that storage tradeoff.
    ///
    /// **What it costs is one sub-block decode per cell read** instead of a
    /// `stride`-byte positional read. Only the *slot regions* are compressed:
    /// the prefix, the descriptors, the commit maps and the checksum tables
    /// stay plain, so addressing a cell, reading its commit bit and checking
    /// its checksum all still cost what they cost.
    ///
    /// Inert when not declared: a format that does not call this writes
    /// byte-identical chunks and hashes identically.
    pub const fn with_chunk_compression(mut self, compression: VariableCompression) -> Self {
        self.chunk_compression = Some(compression);
        self
    }

    pub(crate) const fn with_optional_chunk_compression(
        mut self,
        compression: Option<VariableCompression>,
    ) -> Self {
        self.chunk_compression = compression;
        self
    }

    /// Reserves `bytes` of write-ahead ring in new matrix files. A reader can
    /// keep a snapshot across as many publications as fit in it.
    pub const fn with_matrix_ring_bytes(mut self, bytes: u64) -> Self {
        self.matrix_ring_bytes = Some(bytes);
        self
    }

    /// The ring size new matrix files get, when declared.
    pub const fn matrix_ring_bytes(&self) -> Option<u64> {
        self.matrix_ring_bytes
    }

    /// Stores keyed blocks in place (see [`KeyedStorage`]).
    pub const fn with_keyed_storage(mut self, storage: KeyedStorage) -> Self {
        self.keyed_storage = Some(storage);
        self
    }

    /// Rows each chunk covers, or `None` when no dimension grows.
    pub fn growing_rows_per_chunk(self) -> Option<u64> {
        self.growing_matrix.map(|growing| growing.rows_per_chunk)
    }

    /// Whether any declared block opts out of residency.
    pub fn has_non_resident_blocks(self) -> bool {
        self.block_residency
            .iter()
            .any(|descriptor| !descriptor.resident)
    }

    pub const fn with_matrix_spec(
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

    pub const fn with_matrix_aux(mut self, aux: &'static [MatrixAuxDescriptor]) -> Self {
        self.matrix_aux = aux;
        self
    }

    pub const fn with_layout(mut self, layout: LayoutSpec) -> Self {
        self.layout = layout;
        self
    }

    pub const fn with_read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = read_limits.without_unbounded_opt_in();
        self
    }

    /// Sets optional format-level defaults. These are operational defaults,
    /// not wire-format or schema ceilings.
    pub const fn with_resource_defaults(self, limits: ResourceLimits) -> Self {
        self.with_read_limits(limits)
    }

    /// Resolves standard and format defaults, then overlays call-time policy.
    pub const fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.read_limits = self.read_limits.resolve().overlay(limits);
        self
    }

    pub const fn tighten_read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = self.read_limits.resolve().tighten(read_limits);
        self
    }

    pub(crate) const fn authorize_trusted_read(mut self) -> Self {
        self.read_limits = self.read_limits.authorize_trusted_api();
        self
    }

    pub(crate) const fn ordinary_read(mut self) -> Self {
        self.read_limits = self.read_limits.resolve().without_unbounded_opt_in();
        self
    }

    pub(crate) const fn resolve_entrypoint(self) -> Self {
        if self.read_limits.is_trusted_api {
            self
        } else {
            self.ordinary_read()
        }
    }
}
