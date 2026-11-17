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

/// The default write-ahead ring of a new file's keyed storage.
const DEFAULT_RING_BYTES: u64 = 16 << 20;

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
            encode_internal_key_payload_into::<T>(self.spec, &block.key(), &mut buffer)?;
            let key_end = buffer.len();
            encode_logical_payload_limited_after(self.spec, block, endian, &mut buffer)?;
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
            encode_internal_key_payload_into::<T>(self.spec, key, &mut buffer)?;
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
        encode_internal_key_payload_into::<T>(self.spec, key, scratch)?;
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
                let found = reader.borrow_mut().get(
                    &source,
                    T::ID,
                    scratch,
                    KEY_ENVELOPE,
                    max_payload,
                    &mut |buf| encode_internal_key_payload_into::<T>(spec, key, buf),
                )?;
                if !found {
                    return Ok(false);
                }
            }
        }
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        crate::collections::MaterializationBudget::new(self.spec)
            .decode_into(scratch, endian, out)?;
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
            live_bytes: state.live + state.leaves * crate::keyed_heap::node::CAPACITY as u64
                - state.leaf_free,
            heap_bytes: state.pages * crate::keyed_heap::PAGE_BYTES,
            free_region_bytes: state.pooled * crate::keyed_heap::PAGE_BYTES,
            leaf_pages: state.leaves,
            index_pages: state.interiors,
        })
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
            reader.borrow_mut().follow(&FileTilesRead(&self.file))?;
        }
        Ok(())
    }
}

/// The envelope in front of an encoded internal key: block id and length.
const KEY_ENVELOPE: usize = 12;

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
