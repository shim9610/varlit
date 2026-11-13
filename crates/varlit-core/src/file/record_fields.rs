//! Decoding a variable record field by field, straight from storage.
//!
//! A slice decode needs the whole record in memory first, so a large field
//! exists twice: once in the buffer the record was read into and once in the
//! decoded value. Here the record's fields are walked where they are stored:
//! a byte vector, string or numeric vector is read (and checked, a chunk at
//! a time) directly into the storage of the value being decoded, and only
//! the other fields pass through the caller's scratch buffer, one at a time.
//!
//! The same bytes are accepted and the same budget is charged as by the
//! slice decode. Fields are decoded as they are reached, so damage late in a
//! record is reported after earlier fields were decoded, not before.

use super::record_read::{ChunkCache, fill_range, fill_record};
use super::*;
use crate::codec::BulkField;
use crate::codec::Decoder;
use crate::collections::MaterializationBudget;
use std::collections::HashSet;

/// A field value shorter than this is decoded from scratch: positioning a
/// storage read costs more than copying it.
const BULK_MIN: u64 = 4096;
const FIELD_HEADER_LEN: u64 = 16;

/// How a derived variable block decodes itself from a field walk.
#[doc(hidden)]
pub struct FieldsCodec<T> {
    pub decode_into: fn(&mut T, &mut RecordFields<'_>) -> Result<()>,
}

/// One field of the record being walked.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct RecordField {
    id: u32,
    wire: WireType,
    at: u64,
    len: u64,
}

impl RecordField {
    pub const fn id(&self) -> u32 {
        self.id
    }
    pub const fn wire_type(&self) -> WireType {
        self.wire
    }
}

/// A walk over one stored record's fields.
#[doc(hidden)]
pub struct RecordFields<'a> {
    spec: FormatSpec,
    snapshot: &'a SnapshotFile,
    entry: &'a RecordIndexEntry,
    cache: &'a std::cell::RefCell<ChunkCache>,
    /// The materialization budget left for this record. The walk charges
    /// this copy; what it spent is settled against the caller's budget when
    /// the walk ends, however it ends.
    remaining: u64,
    endian: Endian,
    /// What the budget held when the walk began: the limit a decoder over
    /// the whole payload would report against.
    frame_limit: u64,
    scratch: &'a mut Vec<u8>,
    position: u64,
    /// The record's logical length, once the first read has reported it.
    total: Option<u64>,
    /// Field ids seen so far, as a payload decoder tracks them.
    small_field_ids: u64,
    large_field_ids: Option<HashSet<u32>>,
}

impl RecordFields<'_> {
    /// Charges `bytes` and reports a refusal as a decoder would whose limit
    /// is `frame_limit`, or, for a field's own decoder, what is left now.
    fn spend(
        &mut self,
        bytes: u64,
        resource: &'static str,
        frame_limit: Option<u64>,
    ) -> Result<()> {
        if let Some(left) = self.remaining.checked_sub(bytes) {
            self.remaining = left;
            return Ok(());
        }
        let limit = frame_limit.unwrap_or(self.remaining);
        let actual = limit
            .checked_sub(self.remaining)
            .and_then(|consumed| consumed.checked_add(bytes))
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit,
        })
    }

    /// Fills all of `out` from logical offset `from`, ending no later than
    /// `end`.
    fn read(&self, mut from: u64, end: u64, out: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < out.len() {
            let page = fill_range(
                self.spec,
                self.snapshot,
                self.entry,
                from,
                end,
                &mut out[filled..],
                self.cache,
            )?;
            if page.filled == 0 {
                return Err(Error::UnexpectedEof);
            }
            filled += page.filled;
            from += page.filled as u64;
        }
        Ok(())
    }

    fn u64_at(&self, bytes: &[u8]) -> u64 {
        let bytes: [u8; 8] = bytes[..8].try_into().expect("eight");
        match self.endian {
            Endian::Little => u64::from_le_bytes(bytes),
            Endian::Big => u64::from_be_bytes(bytes),
        }
    }

    /// Records a field id; a repeated id is refused. The same rule, charge
    /// and reservation as `Decoder::note_field_id`.
    fn note_field(&mut self, field_id: u32) -> Result<()> {
        const RESOURCE: &str = "variable field ids";
        if field_id == 0 {
            return Err(Error::InvalidCanonicalEncoding(
                "variable field id must be nonzero",
            ));
        }
        let duplicate = Error::InvalidCanonicalEncoding("duplicate variable field");
        if field_id < u64::BITS {
            let mask = 1u64 << field_id;
            if self.small_field_ids & mask != 0 {
                return Err(duplicate);
            }
            self.small_field_ids |= mask;
            return Ok(());
        }
        if self
            .large_field_ids
            .as_ref()
            .is_some_and(|field_ids| field_ids.contains(&field_id))
        {
            return Err(duplicate);
        }
        let count = self.large_field_ids.as_ref().map_or(0, HashSet::len);
        let overflow = Error::ResourceArithmeticOverflow { resource: RESOURCE };
        let bound = |count| {
            crate::codec::hash_table_reservation_bytes::<u32>(count)
                .and_then(|bytes| bytes.checked_mul(2))
        };
        let grown = count.checked_add(1).and_then(bound);
        let (Some(grown), Some(held)) = (grown, bound(count)) else {
            return Err(overflow);
        };
        self.spend(grown - held, RESOURCE, Some(self.frame_limit))?;
        let field_ids = self.large_field_ids.get_or_insert_with(HashSet::new);
        let requested = u64::try_from(field_ids.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
            .saturating_mul(4);
        field_ids
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: RESOURCE,
                requested,
            })?;
        field_ids.insert(field_id);
        Ok(())
    }

    /// The next field's header, checked as `read_field_header` checks it.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<RecordField>> {
        let mut header = [0u8; FIELD_HEADER_LEN as usize];
        let page = fill_record(
            self.spec,
            self.snapshot,
            self.entry,
            self.position,
            &mut header,
            self.cache,
        )?;
        let total = *self.total.insert(page.end);
        if self.position == total {
            return Ok(None);
        }
        if page.filled != header.len() {
            return Err(Error::UnexpectedEof);
        }
        let endian = self.endian;
        let u32_at = |at: usize| {
            let bytes: [u8; 4] = header[at..at + 4].try_into().expect("four");
            match endian {
                Endian::Little => u32::from_le_bytes(bytes),
                Endian::Big => u32::from_be_bytes(bytes),
            }
        };
        let u16_at = |at: usize| {
            let bytes: [u8; 2] = header[at..at + 2].try_into().expect("two");
            match endian {
                Endian::Little => u16::from_le_bytes(bytes),
                Endian::Big => u16::from_be_bytes(bytes),
            }
        };
        let id = u32_at(0);
        let wire = u16_at(4);
        let len = self.u64_at(&header[8..]);
        if u16_at(6) != 0 {
            return Err(Error::InvalidCanonicalEncoding(
                "variable field flags must be zero",
            ));
        }
        let wire = WireType::from_u16(wire).ok_or(Error::UnknownWireType(wire))?;
        usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
        let at = self.position + FIELD_HEADER_LEN;
        if len > total - at {
            return Err(Error::UnexpectedEof);
        }
        self.note_field(id)?;
        self.position = at + len;
        Ok(Some(RecordField { id, wire, at, len }))
    }

    /// Decodes `field`'s value into `out`: from storage for a large value
    /// whose codec fills in place, through the scratch buffer otherwise.
    pub fn decode_into<F: crate::VarlitDecode>(
        &mut self,
        field: &RecordField,
        out: &mut F,
    ) -> Result<()> {
        let end = field.at + field.len;
        if let Some(bulk) = F::__VARLIT_BULK
            && field.len >= 8 + BULK_MIN
        {
            let mut prefix = [0u8; 8];
            self.read(field.at, end, &mut prefix)?;
            let declared = self.u64_at(&prefix);
            let mut position = field.at + 8;
            let (spec, snapshot, entry, cache) = (self.spec, self.snapshot, self.entry, self.cache);
            let mut read = |buf: &mut [u8]| -> Result<()> {
                let mut filled = 0;
                while filled < buf.len() {
                    let page = fill_range(
                        spec,
                        snapshot,
                        entry,
                        position,
                        end,
                        &mut buf[filled..],
                        cache,
                    )?;
                    if page.filled == 0 {
                        return Err(Error::UnexpectedEof);
                    }
                    filled += page.filled;
                    position += page.filled as u64;
                }
                Ok(())
            };
            let (len, bytes, resource) = bulk.admit(declared, field.len - 8)?;
            self.spend(bytes, resource, None)?;
            return bulk.fill(
                out,
                &mut BulkField {
                    endian: self.endian,
                    len,
                    data_len: field.len - 8,
                    read: &mut read,
                },
            );
        }
        let len = field.len as usize;
        let mut scratch = std::mem::take(self.scratch);
        let result = (|| {
            if scratch.len() < len {
                scratch
                    .try_reserve_exact(len - scratch.len())
                    .map_err(|_| Error::AllocationFailed {
                        resource: "record field",
                        requested: field.len,
                    })?;
                scratch.resize(len, 0);
            }
            self.read(field.at, end, &mut scratch[..len])?;
            Decoder::decode_from_slice_accounted_into(
                &scratch[..len],
                self.endian,
                &mut self.remaining,
                out,
            )
        })();
        *self.scratch = scratch;
        result
    }
}

/// Reads and decodes `entry`'s record into `out`.
///
/// A chunked record of a derived variable block is decoded field by field
/// from storage; any other record is read whole into `scratch` (it is at most
/// one chunk long, or its block has no field walk) and decoded from there.
pub(crate) fn decode_record_into<T: VarlitBlock>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    scratch: &mut Vec<u8>,
    budget: &mut MaterializationBudget,
    cache: &std::cell::RefCell<ChunkCache>,
    out: &mut T,
) -> Result<()> {
    let endian = T::ENDIAN.unwrap_or(spec.endian);
    let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
    budget.consume(logical_len)?;
    if let Some(fields) = T::__VARLIT_FIELDS
        && T::KIND == BlockKind::Variable
        && entry.is_chunked()
    {
        entry.verify_snapshot_framing(spec, snapshot)?;
        let frame_limit = budget.remaining();
        let mut walk = RecordFields {
            spec,
            snapshot,
            entry,
            cache,
            remaining: frame_limit,
            endian,
            frame_limit,
            scratch,
            position: 0,
            total: None,
            small_field_ids: 0,
            large_field_ids: None,
        };
        let decoded = (fields.decode_into)(out, &mut walk);
        // Completed charges survive an error, as a decoder's do.
        budget.consume(frame_limit - walk.remaining)?;
        return decoded;
    }
    entry.read_logical_payload_snapshot_into(spec, snapshot, scratch)?;
    budget.decode_into(scratch, endian, out)
}
