// `ReadLimits` presets, setters and resolution. Included by `format.rs`,
// whose module it shares.
impl ReadLimits {
    pub const MISSING: Self = Self::all(ReadLimit::Missing);
    pub const TRUSTED_UNBOUNDED: Self = Self::all(ReadLimit::TrustedUnbounded);
    pub const STANDARD: Self = Self {
        max_file_len: ReadLimit::Finite(u64::MAX),
        max_records: ReadLimit::Finite(u64::MAX),
        // Held memory is finite by default: a record list or map built in
        // memory may not exceed 1 GiB. Record and segment counts and scan
        // bytes stay work counters — every walk is bounded by the file
        // itself, and capping them would refuse a large log, not a resource.
        max_index_bytes: ReadLimit::Finite(1024 * 1024 * 1024),
        max_scan_bytes: ReadLimit::Finite(u64::MAX),
        max_record_payload_len: ReadLimit::Finite(64 * 1024 * 1024),
        max_logical_payload_len: ReadLimit::Finite(256 * 1024 * 1024),
        max_materialized_bytes: ReadLimit::Finite(1024 * 1024 * 1024),
        max_segments: ReadLimit::Finite(u64::MAX),
        max_matrix_dimension: ReadLimit::Finite(u64::MAX),
        max_matrix_cells: ReadLimit::Finite(u64::MAX),
        max_matrix_bitmap_bytes: ReadLimit::Finite(64 * 1024 * 1024),
        max_matrix_crc_bytes: ReadLimit::Finite(u64::MAX),
        max_matrix_metadata_bytes: ReadLimit::Finite(256 * 1024 * 1024),
        max_matrix_slot_region_len: ReadLimit::Finite(u64::MAX),
        max_sidecar_len: ReadLimit::Finite(256 * 1024 * 1024),
        max_mmap_len: ReadLimit::Finite(8 * 1024 * 1024 * 1024),
        max_file_header_extension_len: ReadLimit::Finite(
            crate::native_layout::DEFAULT_MAX_FILE_HEADER_EXTENSION_LEN,
        ),
        max_keyed_tail_bytes: ReadLimit::Finite(256 * 1024 * 1024),
        integrity_verification: IntegrityVerification::Missing,
        read_validation: None,
        is_trusted_api: false,
    };
    /// Finite companion to [`Self::STANDARD`] for input from untrusted
    /// sources. Every aggregate dimension that `STANDARD` leaves effectively
    /// unbounded (total file length, record count, scan bytes, resident index
    /// bytes, segment count) is finite here, so a hostile file cannot choose
    /// the reader's CPU, I/O, or memory. This is the recommended default when
    /// opening files from untrusted sources with the resident API; large
    /// trusted files should use the scalable APIs or explicit wider limits.
    pub const UNTRUSTED: Self = Self {
        max_file_len: ReadLimit::Finite(16 * 1024 * 1024 * 1024),
        max_records: ReadLimit::Finite(16_000_000),
        max_index_bytes: ReadLimit::Finite(1024 * 1024 * 1024),
        max_scan_bytes: ReadLimit::Finite(16 * 1024 * 1024 * 1024),
        max_segments: ReadLimit::Finite(65_536),
        max_keyed_tail_bytes: ReadLimit::Finite(256 * 1024 * 1024),
        ..Self::STANDARD
    };

    const fn all(value: ReadLimit) -> Self {
        Self {
            max_file_len: value,
            max_records: value,
            max_index_bytes: value,
            max_scan_bytes: value,
            max_record_payload_len: value,
            max_logical_payload_len: value,
            max_materialized_bytes: value,
            max_segments: value,
            max_matrix_dimension: value,
            max_matrix_cells: value,
            max_matrix_bitmap_bytes: value,
            max_matrix_crc_bytes: value,
            max_matrix_metadata_bytes: value,
            max_matrix_slot_region_len: value,
            max_sidecar_len: value,
            max_mmap_len: value,
            max_file_header_extension_len: value,
            max_keyed_tail_bytes: value,
            integrity_verification: IntegrityVerification::Missing,
            read_validation: None,
            is_trusted_api: false,
        }
    }

    /// The verification policy in force: the declared one, or
    /// [`IntegrityVerification::DEFAULT`] when nobody declared one.
    pub const fn effective_integrity_verification(self) -> IntegrityVerification {
        match self.integrity_verification {
            IntegrityVerification::Missing => IntegrityVerification::DEFAULT,
            declared => declared,
        }
    }

    /// Declares when record integrity is verified. See [`IntegrityVerification`].
    pub const fn with_integrity_verification(mut self, policy: IntegrityVerification) -> Self {
        self.integrity_verification = policy;
        self
    }

    /// Select optional read checks without changing allocation ceilings,
    /// publication checks or the file's stored integrity policy.
    pub const fn with_read_validation(mut self, validation: ReadValidation) -> Self {
        self.read_validation = Some(validation);
        self
    }

    /// Resolve an unset override for record payload CRC/framing checks.
    /// Other backend admission checks retain their legacy behavior when unset.
    pub const fn effective_read_validation(self, integrity: IntegrityPolicy) -> ReadValidation {
        match self.read_validation {
            Some(policy) => policy,
            None => {
                let enabled = !matches!(integrity, IntegrityPolicy::None);
                ReadValidation {
                    data_crc: enabled,
                    structure: enabled,
                }
            }
        }
    }

    /// The file-header extension ceiling in force, in bytes.
    ///
    /// Unset — which is what every format that never wrote
    /// `limits { header_extension: .. }` carries — resolves here, and only
    /// here, to 64 KiB: the value this was a private constant for until it
    /// became declarable, so an undeclared format reserves the same region,
    /// accepts the same headers and refuses the same ones as before.
    ///
    /// [`ReadLimit::TrustedUnbounded`] resolves to `u32::MAX`, and a declared
    /// value is clamped to it, because that is the largest length the on-disk
    /// `u32` field can name. Above it the ceiling would be unreachable rather
    /// than generous, and a `header_slots` capacity checked against it would be
    /// accepted at create and then refused by the reader's own `u32`.
    pub const fn effective_max_file_header_extension_len(self) -> u64 {
        let ceiling = u32::MAX as u64;
        match Self::STANDARD.overlay(self).max_file_header_extension_len {
            ReadLimit::Finite(value) if value < ceiling => value,
            // `Finite` at or above the field's range, and `TrustedUnbounded`,
            // are both "as much as the format can express".
            _ => ceiling,
        }
    }

    pub const fn missing() -> Self {
        Self::MISSING
    }

    pub const fn trusted_unbounded() -> Self {
        Self::TRUSTED_UNBOUNDED
    }

    pub const fn finite_all(value: u64) -> Self {
        Self::all(ReadLimit::Finite(value))
    }

    pub const fn standard() -> Self {
        Self::STANDARD
    }

    pub const fn untrusted() -> Self {
        Self::UNTRUSTED
    }

    read_limit_setters! {
        (with_max_file_len, max_file_len),
        (with_max_records, max_records),
        (with_max_index_bytes, max_index_bytes),
        (with_max_scan_bytes, max_scan_bytes),
        (with_max_record_payload_len, max_record_payload_len),
        (with_max_logical_payload_len, max_logical_payload_len),
        (with_max_materialized_bytes, max_materialized_bytes),
        (with_max_segments, max_segments),
        (with_max_matrix_dimension, max_matrix_dimension),
        (with_max_matrix_cells, max_matrix_cells),
        (with_max_matrix_bitmap_bytes, max_matrix_bitmap_bytes),
        (with_max_matrix_crc_bytes, max_matrix_crc_bytes),
        (with_max_matrix_metadata_bytes, max_matrix_metadata_bytes),
        (with_max_matrix_slot_region_len, max_matrix_slot_region_len),
        (with_max_sidecar_len, max_sidecar_len),
        (with_max_mmap_len, max_mmap_len),
        (with_max_keyed_tail_bytes, max_keyed_tail_bytes),
        (with_max_file_header_extension_len, max_file_header_extension_len),
    }

    pub const fn tighten(self, runtime: Self) -> Self {
        Self {
            integrity_verification: self
                .integrity_verification
                .tighten(runtime.integrity_verification),
            read_validation: match self.read_validation {
                Some(declared) => Some(declared),
                None => runtime.read_validation,
            },
            max_file_len: self.max_file_len.tighten(runtime.max_file_len),
            max_records: self.max_records.tighten(runtime.max_records),
            max_index_bytes: self.max_index_bytes.tighten(runtime.max_index_bytes),
            max_scan_bytes: self.max_scan_bytes.tighten(runtime.max_scan_bytes),
            max_record_payload_len: self
                .max_record_payload_len
                .tighten(runtime.max_record_payload_len),
            max_logical_payload_len: self
                .max_logical_payload_len
                .tighten(runtime.max_logical_payload_len),
            max_materialized_bytes: self
                .max_materialized_bytes
                .tighten(runtime.max_materialized_bytes),
            max_segments: self.max_segments.tighten(runtime.max_segments),
            max_matrix_dimension: self
                .max_matrix_dimension
                .tighten(runtime.max_matrix_dimension),
            max_matrix_cells: self.max_matrix_cells.tighten(runtime.max_matrix_cells),
            max_matrix_bitmap_bytes: self
                .max_matrix_bitmap_bytes
                .tighten(runtime.max_matrix_bitmap_bytes),
            max_matrix_crc_bytes: self
                .max_matrix_crc_bytes
                .tighten(runtime.max_matrix_crc_bytes),
            max_matrix_metadata_bytes: self
                .max_matrix_metadata_bytes
                .tighten(runtime.max_matrix_metadata_bytes),
            max_matrix_slot_region_len: self
                .max_matrix_slot_region_len
                .tighten(runtime.max_matrix_slot_region_len),
            max_sidecar_len: self.max_sidecar_len.tighten(runtime.max_sidecar_len),
            max_mmap_len: self.max_mmap_len.tighten(runtime.max_mmap_len),
            max_file_header_extension_len: self
                .max_file_header_extension_len
                .tighten(runtime.max_file_header_extension_len),
            max_keyed_tail_bytes: self
                .max_keyed_tail_bytes
                .tighten(runtime.max_keyed_tail_bytes),
            is_trusted_api: false,
        }
    }

    /// Overlays explicitly supplied fields without treating the existing
    /// values as permanent format ceilings.
    pub const fn overlay(self, runtime: Self) -> Self {
        Self {
            integrity_verification: self
                .integrity_verification
                .overlay(runtime.integrity_verification),
            read_validation: match runtime.read_validation {
                Some(declared) => Some(declared),
                None => self.read_validation,
            },
            max_file_len: self.max_file_len.overlay(runtime.max_file_len),
            max_records: self.max_records.overlay(runtime.max_records),
            max_index_bytes: self.max_index_bytes.overlay(runtime.max_index_bytes),
            max_scan_bytes: self.max_scan_bytes.overlay(runtime.max_scan_bytes),
            max_record_payload_len: self
                .max_record_payload_len
                .overlay(runtime.max_record_payload_len),
            max_logical_payload_len: self
                .max_logical_payload_len
                .overlay(runtime.max_logical_payload_len),
            max_materialized_bytes: self
                .max_materialized_bytes
                .overlay(runtime.max_materialized_bytes),
            max_segments: self.max_segments.overlay(runtime.max_segments),
            max_matrix_dimension: self
                .max_matrix_dimension
                .overlay(runtime.max_matrix_dimension),
            max_matrix_cells: self.max_matrix_cells.overlay(runtime.max_matrix_cells),
            max_matrix_bitmap_bytes: self
                .max_matrix_bitmap_bytes
                .overlay(runtime.max_matrix_bitmap_bytes),
            max_matrix_crc_bytes: self
                .max_matrix_crc_bytes
                .overlay(runtime.max_matrix_crc_bytes),
            max_matrix_metadata_bytes: self
                .max_matrix_metadata_bytes
                .overlay(runtime.max_matrix_metadata_bytes),
            max_matrix_slot_region_len: self
                .max_matrix_slot_region_len
                .overlay(runtime.max_matrix_slot_region_len),
            max_sidecar_len: self.max_sidecar_len.overlay(runtime.max_sidecar_len),
            max_mmap_len: self.max_mmap_len.overlay(runtime.max_mmap_len),
            max_file_header_extension_len: self
                .max_file_header_extension_len
                .overlay(runtime.max_file_header_extension_len),
            max_keyed_tail_bytes: self
                .max_keyed_tail_bytes
                .overlay(runtime.max_keyed_tail_bytes),
            is_trusted_api: false,
        }
    }

    pub const fn resolve(self) -> Self {
        Self::STANDARD.overlay(self)
    }

    pub(crate) const fn authorize_trusted_api(mut self) -> Self {
        self.is_trusted_api = true;
        self
    }

    const fn without_unbounded_opt_in(mut self) -> Self {
        self.is_trusted_api = false;
        self
    }

    pub(crate) fn require(self, key: ReadLimitKey) -> Result<Option<u64>> {
        match key.value(self) {
            ReadLimit::Finite(value) => Ok(Some(value)),
            ReadLimit::Missing if self.is_trusted_api => Ok(None),
            ReadLimit::TrustedUnbounded if self.is_trusted_api => Ok(None),
            ReadLimit::Missing => Err(Error::MissingResourceLimit {
                resource: key.resource(),
            }),
            ReadLimit::TrustedUnbounded => Err(Error::TrustedUnboundedRequiresExplicitApi {
                resource: key.resource(),
            }),
        }
    }

    pub(crate) fn check(self, key: ReadLimitKey, actual: u64) -> Result<()> {
        if let Some(limit) = self.require(key)?
            && actual > limit
        {
            return Err(Error::LimitExceeded {
                resource: key.resource(),
                actual,
                limit,
            });
        }
        Ok(())
    }
}
