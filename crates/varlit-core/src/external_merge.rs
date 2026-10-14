//! Budgeted keyed merge. State, encoded bytes and ordering stay in bounded
//! owner-local buffers; overflow moves them to disposable private files.
use super::*;
use crate::disk_index::ScratchIndex;
use std::collections::BTreeMap;
use std::hash::BuildHasher;

struct BufferedIndex {
    disk: Option<ScratchIndex>,
    pending: HashMap<Vec<u8>, [u8; 52]>,
    bytes: usize,
    limit: usize,
}

impl BufferedIndex {
    fn new(limit: usize) -> Result<Self> {
        Ok(Self {
            disk: None,
            pending: HashMap::new(),
            bytes: 0,
            limit: limit.max(128) / 2,
        })
    }
    fn get(&self, key: &[u8]) -> Result<Option<[u8; 52]>> {
        self.pending.get(key).copied().map_or_else(
            || self.disk.as_ref().map_or(Ok(None), |disk| disk.get(key)),
            |v| Ok(Some(v)),
        )
    }
    fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            if self.disk.is_none() {
                self.disk = Some(ScratchIndex::new(self.limit)?);
            }
            let sorted = self
                .pending
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect::<BTreeMap<_, _>>();
            self.disk.as_mut().unwrap().apply(&sorted)?;
            self.pending.clear();
            self.bytes = 0;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if self.disk.is_some() {
            self.flush()?;
        }
        Ok(())
    }
    fn visit(&self, mut visitor: impl FnMut(Vec<u8>, [u8; 52]) -> Result<()>) -> Result<()> {
        if let Some(disk) = &self.disk {
            debug_assert!(self.pending.is_empty());
            disk.visit(visitor)
        } else {
            let mut sorted: Vec<_> = self.pending.iter().collect();
            sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
            for (key, value) in sorted {
                visitor(key.clone(), *value)?;
            }
            Ok(())
        }
    }
    fn put(&mut self, key: Vec<u8>, value: [u8; 52]) -> Result<()> {
        if let Some(old) = self.pending.get_mut(&key) {
            *old = value;
            return Ok(());
        }
        let bytes = key.len().saturating_add(192);
        if self.bytes.saturating_add(bytes) > self.limit {
            self.flush()?;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.pending.insert(key, value);
        Ok(())
    }
}

/// Append/read scratch stays in RAM until its declared buffer fills. Moving
/// to a private file preserves offsets, so hash chains and value references
/// need no rewrite. Each buffer is separately bounded; no publication occurs.
struct SpillBytes {
    memory: std::io::Cursor<Vec<u8>>,
    disk: Option<File>,
    limit: usize,
}
impl SpillBytes {
    fn new(limit: usize) -> Self {
        Self {
            memory: std::io::Cursor::new(Vec::new()),
            disk: None,
            limit,
        }
    }
}
impl Read for SpillBytes {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.disk {
            Some(file) => file.read(bytes),
            None => self.memory.read(bytes),
        }
    }
}
impl Seek for SpillBytes {
    fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
        match &mut self.disk {
            Some(file) => file.seek(at),
            None => self.memory.seek(at),
        }
    }
}
impl Write for SpillBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.disk.is_none() {
            let end = self
                .memory
                .position()
                .checked_add(bytes.len() as u64)
                .ok_or(std::io::ErrorKind::InvalidInput)?;
            if end > self.limit as u64 {
                let mut file = tempfile::tempfile()?;
                file.write_all(self.memory.get_ref())?;
                file.seek(SeekFrom::Start(self.memory.position()))?;
                self.disk = Some(file);
                self.memory = std::io::Cursor::new(Vec::new());
            } else if end as usize > self.memory.get_ref().capacity() {
                let target = self
                    .memory
                    .get_ref()
                    .capacity()
                    .saturating_mul(2)
                    .max(end as usize)
                    .min(self.limit);
                let additional = target - self.memory.get_ref().len();
                self.memory
                    .get_mut()
                    .try_reserve_exact(additional)
                    .map_err(std::io::Error::other)?;
            }
        }
        match &mut self.disk {
            Some(file) => file.write(bytes),
            None => self.memory.write(bytes),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.disk {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

fn order_bytes(order: MergeOrder) -> [u8; 24] {
    let mut key = [0; 24];
    key[..8].copy_from_slice(&(order.shard_ordinal as u64).to_be_bytes());
    key[8..16].copy_from_slice(&order.sequence.to_be_bytes());
    key[16..24].copy_from_slice(&(order.record_ordinal as u64).to_be_bytes());
    key
}
fn word(meta: &[u8; 52], at: usize) -> u64 {
    u64::from_le_bytes(meta[at..at + 8].try_into().unwrap())
}
fn store_value<T: VarlitBlock>(
    spec: FormatSpec,
    file: &mut SpillBytes,
    order: MergeOrder,
    value: Option<T>,
    payload: &mut Vec<u8>,
) -> Result<[u8; 52]> {
    let mut meta = [0; 52];
    meta[..24].copy_from_slice(&order_bytes(order));
    if let Some(value) = value {
        let bound = spec
            .read_limits
            .require(ReadLimitKey::MaterializedBytes)?
            .unwrap_or(u64::MAX)
            .min(
                spec.read_limits
                    .require(ReadLimitKey::LogicalPayloadLen)?
                    .unwrap_or(u64::MAX),
            );
        crate::codec::encode_into_limited(
            &value,
            T::ENDIAN.unwrap_or(spec.endian),
            bound,
            "materialized bytes",
            payload,
        )?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(payload)?;
        meta[24..32].copy_from_slice(&offset.to_le_bytes());
        meta[32..40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        meta[40] = 1;
    }
    Ok(meta)
}
fn load_value<T: VarlitBlock>(
    spec: FormatSpec,
    file: &mut SpillBytes,
    meta: &[u8; 52],
    budget: &mut MaterializationBudget,
) -> Result<T> {
    let len = word(meta, 32);
    budget.consume(len)?;
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    if file.disk.is_none() {
        let offset = usize::try_from(word(meta, 24)).map_err(|_| Error::UnexpectedEof)?;
        let end = offset.checked_add(len).ok_or(Error::UnexpectedEof)?;
        let payload = file
            .memory
            .get_ref()
            .get(offset..end)
            .ok_or(Error::UnexpectedEof)?;
        return budget.decode(payload, T::ENDIAN.unwrap_or(spec.endian));
    }
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource: "merge value",
            requested: len as u64,
        })?;
    payload.resize(len, 0);
    file.seek(SeekFrom::Start(word(meta, 24)))?;
    file.read_exact(&mut payload)?;
    budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))
}

// Hash buckets preserve Rust Eq/Hash semantics even for a user key whose
// equal values have different encodings. Bucket chains and encoded keys stay
// in a private file, so even deliberate hash collisions do not grow RAM.
fn state_key(kind: u8, number: u64) -> Vec<u8> {
    let mut key = vec![kind];
    key.extend_from_slice(&number.to_be_bytes());
    key
}
fn locate_key<K: crate::VarlitKey>(
    spec: FormatSpec,
    state: &BufferedIndex,
    keys: &mut SpillBytes,
    hash: u64,
    key: &K,
    budget: MaterializationBudget,
) -> Result<Option<u64>> {
    let mut at = state.get(&state_key(0, hash))?.map(|v| word(&v, 0));
    while let Some(offset) = at {
        keys.seek(SeekFrom::Start(offset))?;
        let mut header = [0; 16];
        keys.read_exact(&mut header)?;
        let previous = u64::from_le_bytes(header[..8].try_into().unwrap());
        let len = u64::from_le_bytes(header[8..].try_into().unwrap());
        let mut probe_budget = budget;
        probe_budget.consume(len)?;
        let size = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
        let old: K = if keys.disk.is_none() {
            let start =
                usize::try_from(keys.memory.position()).map_err(|_| Error::UnexpectedEof)?;
            let end = start.checked_add(size).ok_or(Error::UnexpectedEof)?;
            let bytes = keys
                .memory
                .get_ref()
                .get(start..end)
                .ok_or(Error::UnexpectedEof)?;
            probe_budget.decode(bytes, spec.endian)?
        } else {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(size)
                .map_err(|_| Error::AllocationFailed {
                    resource: "merge key",
                    requested: len,
                })?;
            bytes.resize(size, 0);
            keys.read_exact(&mut bytes)?;
            probe_budget.decode(&bytes, spec.endian)?
        };
        if old == *key {
            return Ok(Some(offset));
        }
        at = (previous != u64::MAX).then_some(previous);
    }
    Ok(None)
}
fn insert_key(
    state: &mut BufferedIndex,
    keys: &mut SpillBytes,
    hash: u64,
    encoded: &[u8],
) -> Result<u64> {
    let head_key = state_key(0, hash);
    let previous = state.get(&head_key)?.map_or(u64::MAX, |v| word(&v, 0));
    let offset = keys.seek(SeekFrom::End(0))?;
    keys.write_all(&previous.to_le_bytes())?;
    keys.write_all(&(encoded.len() as u64).to_le_bytes())?;
    keys.write_all(encoded)?;
    let mut head = [0; 52];
    head[..8].copy_from_slice(&offset.to_le_bytes());
    state.put(head_key, head)?;
    Ok(offset)
}

pub(super) struct ExternalMerge<T> {
    spec: FormatSpec,
    values: SpillBytes,
    ordered: BufferedIndex,
    _type: std::marker::PhantomData<T>,
}
impl<T: VarlitMerge> ExternalMerge<T>
where
    T::Key: Eq + Hash,
{
    pub(super) fn collect<P: AsRef<Path>>(
        spec: FormatSpec,
        base: P,
        deltas: &[P],
        max_keys: u64,
    ) -> Result<Self> {
        crate::collections::ensure_registered_block::<T>(spec)?;
        let limit = resident_index::buffer_bytes(spec);
        let mut state = BufferedIndex::new(limit)?;
        let mut values = SpillBytes::new(limit);
        let mut keys = SpillBytes::new(limit);
        let hasher = std::collections::hash_map::RandomState::new();
        let mut distinct = 0u64;
        let mut encoded = Vec::new();
        let mut encoded_value = Vec::new();
        for (shard, path) in std::iter::once(base.as_ref())
            .chain(deltas.iter().map(AsRef::as_ref))
            .enumerate()
        {
            let input = VarlitFile::open_readonly(spec, path)?;
            let mut payload = Vec::new();
            for (ordinal, entry) in input.index.iter().enumerate() {
                let entry = entry?;
                if ![T::ID, TOMBSTONE_BLOCK_ID, OP_BLOCK_ID].contains(&entry.block_id) {
                    continue;
                }
                let order = MergeOrder::for_record(shard, entry.sequence, ordinal);
                let order_key = order_bytes(order);
                let mut budget = MaterializationBudget::new(spec);
                let logical_len = entry.logical_payload_len_snapshot(spec, &input.snapshot)?;
                budget.consume(logical_len)?;
                let (key, put, op) = if entry.block_id == T::ID {
                    if entry.block_version != T::VERSION {
                        return Err(Error::BlockVersionMismatch {
                            block_id: T::ID,
                            expected: T::VERSION,
                            actual: entry.block_version,
                        });
                    }
                    entry.read_logical_payload_snapshot_into(
                        spec,
                        &input.snapshot,
                        &mut payload,
                    )?;
                    let value: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                    (value.key(), Some(value), None)
                } else if entry.block_id == TOMBSTONE_BLOCK_ID {
                    entry.read_payload_snapshot_into(spec, &input.snapshot, &mut payload)?;
                    let Some(key) =
                        decode_internal_key_payload::<T>(spec.endian, &payload, &mut budget)?
                    else {
                        continue;
                    };
                    (key, None, None)
                } else {
                    entry.read_payload_snapshot_into(spec, &input.snapshot, &mut payload)?;
                    let Some((key, op)) =
                        decode_internal_op_payload::<T>(spec.endian, &payload, &mut budget)?
                    else {
                        continue;
                    };
                    (key, None, Some(op))
                };
                crate::codec::encode_into_limited(
                    &key,
                    spec.endian,
                    *budget.remaining_policy(),
                    "materialized bytes",
                    &mut encoded,
                )?;
                budget.consume(encoded.len() as u64)?;
                let hash = hasher.hash_one(&key);
                let found = locate_key(spec, &state, &mut keys, hash, &key, budget)?;
                let old = match found {
                    Some(at) => state.get(&state_key(1, at))?,
                    None => None,
                };
                if old
                    .as_ref()
                    .is_some_and(|meta| order_key.as_slice() < &meta[..24])
                {
                    continue;
                }
                let value = if let Some(op) = op {
                    let meta = old
                        .filter(|m| m[40] != 0)
                        .ok_or(Error::MissingMergeTarget)?;
                    let mut value = load_value::<T>(spec, &mut values, &meta, &mut budget)?;
                    value.apply_op(op)?;
                    Some(value)
                } else {
                    put
                };
                if old.is_none() {
                    if distinct >= max_keys {
                        return Err(Error::LimitExceeded {
                            resource: "merge distinct keys",
                            actual: distinct.saturating_add(1),
                            limit: max_keys,
                        });
                    }
                    distinct += 1;
                }
                let meta = store_value(spec, &mut values, order, value, &mut encoded_value)?;
                let at = match found {
                    Some(at) => at,
                    None => insert_key(&mut state, &mut keys, hash, &encoded)?,
                };
                state.put(state_key(1, at), meta)?;
            }
        }
        state.finish()?;
        let mut ordered = BufferedIndex::new(limit)?;
        state.visit(|key, meta| {
            if key.first() == Some(&1) && meta[40] != 0 {
                ordered.put(meta[..24].to_vec(), meta)?;
            }
            Ok(())
        })?;
        ordered.finish()?;
        Ok(Self {
            spec,
            values,
            ordered,
            _type: std::marker::PhantomData,
        })
    }
    pub(super) fn write(mut self, out: &mut VarlitFile) -> Result<()> {
        let spec = self.spec;
        let values = &mut self.values;
        self.ordered.visit(|_, meta| {
            let mut budget = MaterializationBudget::new(spec);
            let value = load_value::<T>(spec, values, &meta, &mut budget)?;
            out.push_with_prev_key_info(&value, None)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_buffers_stay_in_memory_until_full_and_keep_offsets_on_spill() -> Result<()> {
        let mut bytes = SpillBytes::new(64);
        bytes.write_all(&[3; 30])?;
        bytes.write_all(&[7; 30])?;
        assert!(bytes.disk.is_none());
        bytes.seek(SeekFrom::Start(25))?;
        let mut check = [0; 10];
        bytes.read_exact(&mut check)?;
        assert_eq!(check, [3, 3, 3, 3, 3, 7, 7, 7, 7, 7]);
        bytes.seek(SeekFrom::End(0))?;
        bytes.write_all(&[9; 16])?;
        assert!(bytes.disk.is_some());
        bytes.seek(SeekFrom::Start(55))?;
        bytes.read_exact(&mut check)?;
        assert_eq!(check, [7, 7, 7, 7, 7, 9, 9, 9, 9, 9]);
        let mut index = BufferedIndex::new(4096)?;
        index.put(vec![1], [1; 52])?;
        index.finish()?;
        assert!(index.disk.is_none());
        for i in 2..100 {
            index.put(vec![i], [i; 52])?;
        }
        index.finish()?;
        assert!(index.disk.is_some());
        for i in 1..100 {
            assert_eq!(index.get(&[i])?, Some([i; 52]));
        }
        Ok(())
    }
    #[derive(Clone, Debug)]
    struct Key(u64);
    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.0 % 10 == other.0 % 10
        }
    }
    impl Eq for Key {}
    impl Hash for Key {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            0u8.hash(state);
        }
    }
    impl crate::VarlitEncode for Key {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;
        fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
            self.0.encode_varlit(encoder)
        }
    }
    impl crate::VarlitDecode for Key {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;
        fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varlit(decoder)?))
        }
    }
    #[test]
    fn spilled_hash_collisions_use_key_equality_not_encoded_byte_equality() -> Result<()> {
        let spec = FormatSpec::new(
            b"KEYEQ",
            1,
            crate::Endian::Little,
            1,
            crate::IndexPolicy::ScanOnOpen,
            crate::IntegrityPolicy::None,
            crate::RecoveryPolicy::Strict,
            crate::ManifestPolicy::None,
            &[],
        )
        .with_read_limits(crate::ReadLimits::STANDARD);
        let mut state = BufferedIndex::new(128)?;
        let mut keys = SpillBytes::new(0);
        let first = insert_key(&mut state, &mut keys, 0, &3u64.to_le_bytes())?;
        let second = insert_key(&mut state, &mut keys, 0, &7u64.to_le_bytes())?;
        state.flush()?;
        let budget = MaterializationBudget::new(spec);
        assert_eq!(
            locate_key(spec, &state, &mut keys, 0, &Key(13), budget)?,
            Some(first)
        );
        assert_eq!(
            locate_key(spec, &state, &mut keys, 0, &Key(27), budget)?,
            Some(second)
        );
        assert_eq!(
            locate_key(spec, &state, &mut keys, 0, &Key(2), budget)?,
            None
        );
        Ok(())
    }
}
