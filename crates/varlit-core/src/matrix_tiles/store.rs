//! What the tile engine needs from the file under it.

use super::RecordKind;
use crate::Result;

/// Read access at logical offsets of the append log.
pub(crate) trait TileSource {
    /// Fills `buf` from `offset`; a short file is an error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// The logical length of the file: how far appended records reach.
    fn len(&self) -> Result<u64>;
}

/// Write access for the single writer.
pub(crate) trait TileStore: TileSource {
    /// Overwrites bytes inside a record the engine appended earlier.
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()>;
    /// Appends an internal record of `kind` holding `payload` and returns the
    /// logical offset of its payload. Not durable until [`Self::sync`].
    fn append(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64>;
    /// Makes every write and append so far durable: the one barrier a
    /// publication pays.
    fn sync(&mut self) -> Result<()>;
}

#[cfg(test)]
pub(crate) mod memory {
    //! An in-memory store whose crash keeps exactly what was synced plus any
    //! prefix of what was written after, the last write possibly torn.

    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    pub(crate) struct Shared {
        pub bytes: Vec<u8>,
        pub durable: Vec<u8>,
        /// Writes since the last sync, in order: `(offset, bytes)`.
        pub since_sync: Vec<(u64, Vec<u8>)>,
        pub syncs: u64,
        pub appends: Vec<(RecordKind, u64, u64)>,
        /// The sync numbered this fails instead, as a crash before the
        /// barrier would, leaving the writes since the last sync pending.
        pub fail_sync_at: Option<u64>,
    }

    #[derive(Clone, Default)]
    pub(crate) struct MemStore(pub Rc<RefCell<Shared>>);

    impl MemStore {
        pub(crate) fn new() -> Self {
            let store = Self::default();
            // A stand-in for the file header: offset 0 is never a payload.
            store.0.borrow_mut().bytes.resize(64, 0);
            store.0.borrow_mut().durable.resize(64, 0);
            store
        }
        /// The file after a crash: everything synced, then the first `keep`
        /// later writes, the next one cut to `torn` bytes.
        pub(crate) fn crashed(&self, keep: usize, torn: usize) -> Self {
            let shared = self.0.borrow();
            let mut bytes = shared.durable.clone();
            for (index, (at, data)) in shared.since_sync.iter().enumerate() {
                let take = if index < keep {
                    data.len()
                } else if index == keep {
                    torn.min(data.len())
                } else {
                    break;
                };
                let end = *at as usize + take;
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[*at as usize..end].copy_from_slice(&data[..take]);
            }
            let store = Self::default();
            store.0.borrow_mut().durable = bytes.clone();
            store.0.borrow_mut().bytes = bytes;
            store
        }
        /// A copy holding this store's bytes cut back to `len`, as a
        /// recovery that drops an unacknowledged tail leaves a file.
        pub(crate) fn truncated(&self, len: u64) -> Self {
            let mut bytes = self.0.borrow().bytes.clone();
            bytes.truncate(len as usize);
            let store = Self::default();
            store.0.borrow_mut().durable = bytes.clone();
            store.0.borrow_mut().bytes = bytes;
            store
        }
        pub(crate) fn writes_since_sync(&self) -> usize {
            self.0.borrow().since_sync.len()
        }
        pub(crate) fn len(&self) -> u64 {
            self.0.borrow().bytes.len() as u64
        }
        /// Makes the `n`th sync from now fail.
        pub(crate) fn fail_sync_in(&self, n: u64) {
            let mut shared = self.0.borrow_mut();
            shared.fail_sync_at = Some(shared.syncs + n);
        }
    }

    impl TileSource for MemStore {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let shared = self.0.borrow();
            let end = offset as usize + buf.len();
            if end > shared.bytes.len() {
                return Err(crate::Error::InvalidMatrixLayout);
            }
            buf.copy_from_slice(&shared.bytes[offset as usize..end]);
            Ok(())
        }
        fn len(&self) -> Result<u64> {
            Ok(self.0.borrow().bytes.len() as u64)
        }
    }

    impl TileStore for MemStore {
        fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
            let mut shared = self.0.borrow_mut();
            let end = offset as usize + bytes.len();
            assert!(end <= shared.bytes.len(), "write past the end");
            shared.bytes[offset as usize..end].copy_from_slice(bytes);
            shared.since_sync.push((offset, bytes.to_vec()));
            Ok(())
        }
        fn append(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64> {
            let mut shared = self.0.borrow_mut();
            // A 16-byte stand-in record header, so payloads are not adjacent.
            let at = shared.bytes.len() as u64 + 16;
            let mut record = vec![0xA5u8; 16];
            record.extend_from_slice(payload);
            let start = shared.bytes.len() as u64;
            shared.bytes.extend_from_slice(&record);
            shared.since_sync.push((start, record));
            shared.appends.push((kind, at, payload.len() as u64));
            Ok(at)
        }
        fn sync(&mut self) -> Result<()> {
            let mut shared = self.0.borrow_mut();
            if shared.fail_sync_at == Some(shared.syncs + 1) {
                return Err(crate::Error::Io(std::io::Error::other("injected crash")));
            }
            shared.durable = shared.bytes.clone();
            shared.since_sync.clear();
            shared.syncs += 1;
            Ok(())
        }
    }
}
