//! Bounded keyed storage on a native file: keyed blocks stored once per key
//! in the file's record heap (see `crate::keyed_heap`), for formats that
//! declare [`KeyedStorage`](crate::KeyedStorage).
//!
//! The heap's metadata is the first record of the log. A writable handle
//! opens the heap as its writer; a read-only handle as a reader of the
//! generation published when it first reads. What a writer puts is visible
//! to itself at once and to readers once [`VarlitFile::sync`] publishes it.

use super::matrix_tiles_io::{FileTiles, FileTilesRead};
use super::*;
use crate::keyed_heap::heap::{KeyedHeap, Tuning};
use crate::keyed_heap::reader::KeyedReader;

/// The default write-ahead ring of a new file's keyed storage. The ring is
/// a fixed part of the file; the writer ends a round before a publication
/// outgrows it, so a small ring costs more publications during bulk loads,
/// not file space.
const DEFAULT_RING_BYTES: u64 = 2 << 20;

pub(super) enum KeyedEngine {
    Writer(Box<KeyedHeap>),
    Reader(Box<std::cell::RefCell<KeyedReader>>),
}

impl std::fmt::Debug for KeyedEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Writer(_) => "KeyedEngine::Writer",
            Self::Reader(_) => "KeyedEngine::Reader",
        })
    }
}

impl VarlitFile {
    /// Lays out empty keyed storage as the first record of a new file, when
    /// the format declares it.
    pub(super) fn create_keyed_storage(&mut self) -> Result<()> {
        let Some(keyed) = self.spec.keyed_storage else {
            return Ok(());
        };
        let expected = self.keyed_meta_offset()?;
        let ring = keyed.ring_bytes.unwrap_or(DEFAULT_RING_BYTES);
        let mut store = FileTiles::new(self);
        let heap = KeyedHeap::create(
            &mut store,
            ring,
            keyed.free_space_permille,
            keyed.budget(),
            Tuning::DEFAULT,
        )?;
        if heap.meta != expected {
            return Err(Error::InvalidCanonicalEncoding(
                "keyed storage metadata is not the first record",
            ));
        }
        let _ = self.keyed.set(KeyedEngine::Writer(Box::new(heap)));
        Ok(())
    }

    /// The payload offset of the keyed storage metadata: the first record.
    fn keyed_meta_offset(&self) -> Result<u64> {
        Ok(append_log_start_for_file(self)? + RECORD_HEADER_LEN)
    }

    fn require_keyed_storage(&self) -> Result<()> {
        if self.spec.keyed_storage.is_none() {
            return Err(Error::InvalidFormatSpec(
                "the format declares no keyed storage",
            ));
        }
        Ok(())
    }

    /// Runs `op` on the keyed storage writer, opening it first.
    fn with_keyed_writer<R>(
        &mut self,
        op: impl FnOnce(&mut KeyedHeap, &mut FileTiles<'_>) -> Result<R>,
    ) -> Result<R> {
        self.require_keyed_storage()?;
        self.check_write()?;
        let mut heap = match self.keyed.take() {
            Some(KeyedEngine::Writer(heap)) => heap,
            _ => {
                let meta = self.keyed_meta_offset()?;
                Box::new(KeyedHeap::open(
                    &mut FileTiles::new(self),
                    meta,
                    Tuning::DEFAULT,
                )?)
            }
        };
        let result = op(&mut heap, &mut FileTiles::new(self));
        let _ = self.keyed.set(KeyedEngine::Writer(heap));
        result
    }

    /// The keyed storage as this handle reads it: its writer's staged view,
    /// or a reader's snapshot.
    fn keyed_engine(&self) -> Result<&KeyedEngine> {
        self.require_keyed_storage()?;
        if self.keyed.get().is_none() {
            let reader = KeyedReader::open(&FileTilesRead(&self.file), self.keyed_meta_offset()?)?;
            let _ = self
                .keyed
                .set(KeyedEngine::Reader(Box::new(std::cell::RefCell::new(
                    reader,
                ))));
        }
        self.keyed.get().ok_or(Error::InvalidFormatSpec(
            "the format declares no keyed storage",
        ))
    }

    /// Stores `block` as the one record of its key, replacing the one there
    /// is. Space the old value used is reused; the file does not grow with
    /// the number of updates. Visible to this handle at once and to readers
    /// after [`Self::sync`].
    pub fn put_keyed<T>(&mut self, block: &T) -> Result<()>
    where
        T: VarlitKeyedBlock,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        self.require_keyed_storage()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let mut buffer = std::mem::take(&mut self.record_buffer);
        let result = (|| {
            encode_keyed_storage_key_into::<T>(self.spec, &block.key(), &mut buffer)?;
            let key_end = buffer.len();
            encode_keyed_value_after(self.spec, block, endian, &mut buffer)?;
            let (key, payload) = buffer.split_at(key_end);
            let key = &key[KEY_ENVELOPE..];
            self.with_keyed_writer(|heap, store| heap.put(store, T::ID, key, payload))
        })();
        self.record_buffer = buffer;
        result
    }

    /// Removes the record of `key`; `false` when there is none. Its space is
    /// reused, and no tombstone is written.
    pub fn remove_keyed<T>(&mut self, key: &T::Key) -> Result<bool>
    where
        T: VarlitKeyedBlock,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        self.require_keyed_storage()?;
        self.ensure_user_block::<T>()?;
        let mut buffer = std::mem::take(&mut self.record_buffer);
        let result = (|| {
            encode_keyed_storage_key_into::<T>(self.spec, key, &mut buffer)?;
            let key = &buffer[KEY_ENVELOPE..];
            self.with_keyed_writer(|heap, store| heap.delete(store, T::ID, key))
        })();
        self.record_buffer = buffer;
        result
    }

    /// Decodes the record of `key` into `out`; `false` (and `out`
    /// untouched) when there is none. `scratch` holds the encoded key and
    /// then the payload: the read allocates nothing else.
    pub fn get_keyed<T>(&self, key: &T::Key, scratch: &mut Vec<u8>, out: &mut T) -> Result<bool>
    where
        T: VarlitKeyedBlock,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        self.require_keyed_storage()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        encode_keyed_storage_key_into::<T>(self.spec, key, scratch)?;
        let source = FileTilesRead(&self.file);
        match self.keyed_engine()? {
            KeyedEngine::Writer(heap) => {
                let Some(found) = heap.find(&source, T::ID, &scratch[KEY_ENVELOPE..])? else {
                    return Ok(false);
                };
                read_into(scratch, found.payload_len(), |buf| {
                    heap.read_payload(&source, &found, buf)
                })?;
            }
            KeyedEngine::Reader(reader) => {
                let max_payload = match self.spec.read_limits.max_record_payload_len {
                    crate::format::ReadLimit::Finite(limit) => limit,
                    _ => u64::MAX,
                };
                let spec = self.spec;
                let found = reader
                    .try_borrow_mut()
                    .map_err(|_| Error::KeyedStorageBusy)?
                    .get(
                        &source,
                        T::ID,
                        scratch,
                        KEY_ENVELOPE,
                        max_payload,
                        &mut |buf| encode_keyed_storage_key_into::<T>(spec, key, buf),
                    )?;
                if !found {
                    return Ok(false);
                }
            }
        }
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        crate::collections::MaterializationBudget::new(self.spec)
            .decode_keyed_value_into(scratch, endian, key, out)?;
        Ok(true)
    }

    /// What the keyed storage holds, as this handle sees it.
    pub fn keyed_storage_stats(&self) -> Result<crate::KeyedStorageStats> {
        let (state, generation) = match self.keyed_engine()? {
            KeyedEngine::Writer(heap) => (*heap.state(), heap.ring.published()),
            KeyedEngine::Reader(reader) => {
                let reader = reader.borrow();
                (*reader.state(), reader.snapshot())
            }
        };
        Ok(crate::KeyedStorageStats {
            generation,
            keys: state.keys,
            live_bytes: state.record_bytes,
            heap_bytes: state.pages * crate::keyed_heap::PAGE_BYTES,
            free_region_bytes: state.free * crate::keyed_heap::PAGE_BYTES,
            leaf_pages: state.leaves,
            index_pages: state.interiors,
        })
    }

    /// Visits the records of `T` whose keys lie in `range`, in key order,
    /// decoding each into `out` before `visit` sees it; `visit` returns
    /// `false` to stop. Returns the records visited.
    ///
    /// Key order is the order of the stored key bytes: the declared key
    /// fields encoded big-endian, so unsigned integer fields (and tuples of
    /// them) are in numeric order; a signed field orders negative values
    /// after positive ones, and a string field by its length first.
    ///
    /// `scratch` holds the bounds' keys and then each record; the read
    /// allocates nothing else. A reader's range is one snapshot: a reader
    /// the writer's ring moves past during the read gets
    /// [`Error::MatrixSnapshotExpired`] and must `follow`. `visit` must not
    /// read this handle's keyed storage ([`Error::KeyedStorageBusy`]).
    pub fn range_keyed<T, R>(
        &self,
        range: R,
        scratch: &mut Vec<u8>,
        out: &mut T,
        visit: &mut dyn FnMut(&T) -> Result<bool>,
    ) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        R: core::ops::RangeBounds<T::Key>,
    {
        use core::ops::Bound;
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        self.require_keyed_storage()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let spec = self.spec;
        let source = FileTilesRead(&self.file);
        let engine = self.keyed_engine()?;
        let from = match range.start_bound() {
            Bound::Included(key) | Bound::Excluded(key) => {
                encode_keyed_storage_key_into::<T>(spec, key, scratch)?;
                Some((
                    &scratch[KEY_ENVELOPE..],
                    matches!(range.start_bound(), Bound::Included(_)),
                ))
            }
            Bound::Unbounded => None,
        };
        let mut reader = match engine {
            KeyedEngine::Reader(reader) => Some(
                reader
                    .try_borrow_mut()
                    .map_err(|_| Error::KeyedStorageBusy)?,
            ),
            KeyedEngine::Writer(_) => None,
        };
        let cursor = match (engine, reader.as_deref_mut()) {
            (KeyedEngine::Writer(heap), _) => heap.seek(&source, T::ID, from)?,
            (_, Some(reader)) => reader.seek(&source, T::ID, from)?,
            _ => return Err(Error::InvalidMatrixLayout),
        };
        let Some(cursor) = cursor else {
            return Ok(0);
        };
        let (to, to_inclusive) = match range.end_bound() {
            Bound::Included(key) => (Some(key), true),
            Bound::Excluded(key) => (Some(key), false),
            Bound::Unbounded => (None, false),
        };
        match to {
            Some(key) => encode_keyed_storage_key_into::<T>(spec, key, scratch)?,
            None => scratch.clear(),
        }
        let to_end = scratch.len();
        let max_payload = match spec.read_limits.max_record_payload_len {
            crate::format::ReadLimit::Finite(limit) => limit,
            _ => u64::MAX,
        };
        let endian = T::ENDIAN.unwrap_or(spec.endian);
        let mut visited = 0u64;
        let mut each = |ctx: &mut crate::keyed_heap::lookup::Ctx<'_>,
                        page: &[u8],
                        at: u64,
                        body: &crate::keyed_heap::tree::Body|
         -> Result<bool> {
            if body.value_len > max_payload {
                return Err(Error::LimitExceeded {
                    resource: "keyed record payload",
                    actual: body.value_len,
                    limit: max_payload,
                });
            }
            let key_len = crate::width::index(body.key.len)?;
            let total = key_len + crate::width::index(body.value_len)?;
            scratch.truncate(to_end);
            scratch
                .try_reserve(total)
                .map_err(|_| Error::AllocationFailed {
                    resource: "keyed record payload",
                    requested: total as u64,
                })?;
            scratch.resize(to_end + total, 0);
            crate::keyed_heap::tree::read_held(ctx, page, at, body, 0, &mut scratch[to_end..])?;
            let (bound, record) = scratch.split_at(to_end);
            let (key, value) = record.split_at(key_len);
            if to.is_some() {
                let order = key.cmp(&bound[KEY_ENVELOPE..]);
                if order.is_gt() || (order.is_eq() && !to_inclusive) {
                    return Ok(false);
                }
            }
            let mut budget = crate::collections::MaterializationBudget::new(spec);
            budget.decode_with(key, Endian::Big, &mut |decoder| {
                T::__varlit_decode_key_into(out, decoder)
            })?;
            budget.decode_with(value, endian, &mut |decoder| {
                T::__varlit_decode_value_fields_into(out, decoder)
            })?;
            visited += 1;
            visit(out)
        };
        match (engine, reader.as_deref_mut()) {
            (KeyedEngine::Writer(heap), _) => heap.scan(&source, cursor, &mut each)?,
            (_, Some(reader)) => reader.scan(&source, cursor, &mut each)?,
            _ => return Err(Error::InvalidMatrixLayout),
        }
        Ok(visited)
    }

    /// Publishes the keyed storage writer's changes, if any: one generation
    /// that readers adopt when they follow. Part of [`Self::sync`].
    pub(super) fn publish_keyed(&mut self) -> Result<bool> {
        if !matches!(self.keyed.get(), Some(KeyedEngine::Writer(heap)) if heap.is_dirty()) {
            return Ok(false);
        }
        self.with_keyed_writer(|heap, store| heap.publish(store).map(drop))?;
        Ok(true)
    }

    /// Adopts the newest published keyed generation in a reader. Part of a
    /// read-only handle's follow.
    pub(super) fn follow_keyed(&self) -> Result<()> {
        if let Some(KeyedEngine::Reader(reader)) = self.keyed.get() {
            reader
                .try_borrow_mut()
                .map_err(|_| Error::KeyedStorageBusy)?
                .follow(&FileTilesRead(&self.file))?;
        }
        Ok(())
    }
}

/// The envelope in front of an encoded internal key: block id and length.
const KEY_ENVELOPE: usize = 12;

/// Appends to `buffer` the value keyed storage keeps for `block`: its
/// fields but the key fields, bounded like a logical payload.
fn encode_keyed_value_after<T: VarlitKeyedBlock>(
    spec: FormatSpec,
    block: &T,
    endian: Endian,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let mut encoder = crate::codec::Encoder::new_limited_appending(
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
        std::mem::take(buffer),
    );
    let encoded = block.__varlit_encode_value(&mut encoder);
    let (output, finished) = encoder.into_buffer_and_result();
    *buffer = output;
    encoded.and(finished)
}

/// Sizes `scratch` to a payload of `len` bytes and fills it with `read`.
fn read_into(
    scratch: &mut Vec<u8>,
    len: u64,
    read: impl FnOnce(&mut [u8]) -> Result<()>,
) -> Result<()> {
    let len = crate::width::index(len)?;
    scratch.clear();
    scratch
        .try_reserve(len)
        .map_err(|_| Error::AllocationFailed {
            resource: "keyed record payload",
            requested: len as u64,
        })?;
    scratch.resize(len, 0);
    read(scratch)
}
