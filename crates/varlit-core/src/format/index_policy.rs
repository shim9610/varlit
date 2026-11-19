// The `IndexPolicy` constructors and setters. Included by `format.rs`, whose
// module it shares.
#[allow(non_upper_case_globals)]
impl IndexPolicy {
    pub const ScanOnOpen: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: false,
        keyed_offset_chain: false,
        segment_on_flush: false,
        open_digest_on_flush: false,
        header_tails: false,
    };

    pub const CheckpointOnFlush: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: true,
        block_offset_chain: false,
        keyed_offset_chain: false,
        segment_on_flush: false,
        open_digest_on_flush: false,
        header_tails: false,
    };

    pub const BlockOffsetChain: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: false,
        segment_on_flush: false,
        open_digest_on_flush: false,
        header_tails: false,
    };

    pub const KeyedOffsetChain: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: true,
        segment_on_flush: false,
        open_digest_on_flush: false,
        header_tails: false,
    };

    /// Segment-chained open, built on the block offset chain it needs.
    pub const SegmentOnFlush: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: false,
        segment_on_flush: true,
        open_digest_on_flush: false,
        header_tails: false,
    };

    pub const fn new(
        scan_on_open: bool,
        checkpoint_on_flush: bool,
        block_offset_chain: bool,
        keyed_offset_chain: bool,
    ) -> Self {
        Self {
            scan_on_open,
            checkpoint_on_flush,
            block_offset_chain,
            keyed_offset_chain,
            segment_on_flush: false,
            open_digest_on_flush: false,
            header_tails: false,
        }
    }

    pub const fn with_scan_on_open(mut self, enabled: bool) -> Self {
        self.scan_on_open = enabled;
        self
    }

    pub const fn with_checkpoint_on_flush(mut self, enabled: bool) -> Self {
        self.checkpoint_on_flush = enabled;
        if enabled {
            self.scan_on_open = true;
        }
        self
    }

    pub const fn with_block_offset_chain(mut self, enabled: bool) -> Self {
        self.block_offset_chain = enabled;
        if enabled {
            self.scan_on_open = true;
        }
        self
    }

    pub const fn with_keyed_offset_chain(mut self, enabled: bool) -> Self {
        self.keyed_offset_chain = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    /// Enables the internal segment chain.
    ///
    /// The chain *is* `prev_same_block_offset` in the record footer, so this
    /// turns the block offset chain on with it; there is no segment chain
    /// without a footer to carry it.
    pub const fn with_segment_on_flush(mut self, enabled: bool) -> Self {
        self.segment_on_flush = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    /// Enables the open digest.
    ///
    /// Turns the block offset chain on with it, for the same reason
    /// `segment_on_flush` does and for one more: the digest hands out each
    /// block's tail offset, and the only thing to do with a tail offset is walk
    /// back through `prev_same_block_offset`. Handing out an entry point to a
    /// chain that is not there would be handing out a dead end.
    pub const fn with_open_digest_on_flush(mut self, enabled: bool) -> Self {
        self.open_digest_on_flush = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    /// Enables the header tail region.
    ///
    /// Turns the block offset chain on with it, for the reason
    /// [`Self::with_open_digest_on_flush`] does: the region hands out each
    /// block's tail offset, and the only thing to do with one is walk back
    /// through `prev_same_block_offset`.
    pub const fn with_header_tails(mut self, enabled: bool) -> Self {
        self.header_tails = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    pub const fn requires_record_footer(self) -> bool {
        self.block_offset_chain || self.keyed_offset_chain || self.segment_on_flush
    }
}
