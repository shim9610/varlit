//! External keyed merge. Input order is unchanged; only the retained per-key
//! state and final output ordering move to disposable owner-local files.
use super::*;
use crate::disk_index::ScratchIndex;
use std::collections::BTreeMap;
use std::hash::BuildHasher;

struct BufferedIndex {
    disk: ScratchIndex,
    pending: BTreeMap<Vec<u8>, [u8; 52]>,
    bytes: usize,
    limit: usize,
}

impl BufferedIndex {
    fn new(limit: usize) -> Result<Self> {
        Ok(Self {
            disk: ScratchIndex::new(limit / 2)?,
            pending: BTreeMap::new(),
            bytes: 0,
            limit: limit.max(128) / 2,
        })
    }
    fn get(&self, key: &[u8]) -> Result<Option<[u8; 52]>> {
        self.pending
            .get(key)
            .copied()
            .map_or_else(|| self.disk.get(key), |v| Ok(Some(v)))
    }
    fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            self.disk.apply(&self.pending)?;
            self.pending.clear();
            self.bytes = 0;
        }
        Ok(())
    }
    fn put(&mut self, key: Vec<u8>, value: [u8; 52]) -> Result<()> {
        if let Some(old) = self.pending.get_mut(&key) {
            *old = value;
            return Ok(());
        }
        let bytes = key.len().saturating_add(96);
        if self.bytes.saturating_add(bytes) > self.limit {
            self.flush()?;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.pending.insert(key, value);
        Ok(())
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
fn store_value<T: StratavaBlock>(
    spec: FormatSpec,
    file: &mut File,
    order: MergeOrder,
    value: Option<T>,
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
        let payload = encode_to_vec_limited(
            &value,
            T::ENDIAN.unwrap_or(spec.endian),
            bound,
            "materialized bytes",
        )?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(&payload)?;
        meta[24..32].copy_from_slice(&offset.to_le_bytes());
        meta[32..40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        meta[40] = 1;
    }
    Ok(meta)
}
fn load_value<T: StratavaBlock>(
    spec: FormatSpec,
    file: &mut File,
    meta: &[u8; 52],
    budget: &mut MaterializationBudget,
) -> Result<T> {
    let len = word(meta, 32);
    budget.consume(len)?;
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
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
fn locate_key<K: crate::StratavaKey>(
    spec: FormatSpec,
    state: &BufferedIndex,
    keys: &mut File,
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
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| Error::AllocationFailed {
                resource: "merge key",
                requested: len,
            })?;
        bytes.resize(size, 0);
        keys.read_exact(&mut bytes)?;
        let old: K = probe_budget.decode(&bytes, spec.endian)?;
        if old == *key {
            return Ok(Some(offset));
        }
        at = (previous != u64::MAX).then_some(previous);
    }
    Ok(None)
}
fn insert_key(
    state: &mut BufferedIndex,
    keys: &mut File,
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
    values: File,
    ordered: ScratchIndex,
    _type: std::marker::PhantomData<T>,
}
impl<T: StratavaMerge> ExternalMerge<T>
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
        let mut values = tempfile::tempfile()?;
        let mut keys = tempfile::tempfile()?;
        let hasher = std::collections::hash_map::RandomState::new();
        let mut distinct = 0u64;
        for (shard, path) in std::iter::once(base.as_ref())
            .chain(deltas.iter().map(AsRef::as_ref))
            .enumerate()
        {
            let input = StratavaFile::open_readonly(spec, path)?;
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
                let encoded = encode_to_vec_limited(
                    &key,
                    spec.endian,
                    *budget.remaining_policy(),
                    "materialized bytes",
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
                let meta = store_value(spec, &mut values, order, value)?;
                let at = match found {
                    Some(at) => at,
                    None => insert_key(&mut state, &mut keys, hash, &encoded)?,
                };
                state.put(state_key(1, at), meta)?;
            }
        }
        state.flush()?;
        let mut ordered = BufferedIndex::new(limit)?;
        state.disk.visit(|key, meta| {
            if key.first() == Some(&1) && meta[40] != 0 {
                ordered.put(meta[..24].to_vec(), meta)?;
            }
            Ok(())
        })?;
        ordered.flush()?;
        Ok(Self {
            spec,
            values,
            ordered: ordered.disk,
            _type: std::marker::PhantomData,
        })
    }
    pub(super) fn write(mut self, out: &mut StratavaFile) -> Result<()> {
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
    impl crate::StratavaEncode for Key {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;
        fn encode_stratava(&self, encoder: &mut crate::Encoder) -> Result<()> {
            self.0.encode_stratava(encoder)
        }
    }
    impl crate::StratavaDecode for Key {
        const WIRE_TYPE: crate::WireType = crate::WireType::U64;
        fn decode_stratava(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_stratava(decoder)?))
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
        let mut keys = tempfile::tempfile()?;
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
