// Keyed merges and compaction: estimates, merging delta files into a base,
// and the atomic rewrite of their output.
// Included by `file.rs`, whose module it shares.

/// Record counts and a hypothetical fully resident merge size.
///
/// Merge/compact now use temporary disk indexes. The count fields still size
/// logical work and bound distinct keys. The byte fields retain the historical
/// `count * size_of` model for an equivalent fully resident implementation;
/// they do **not** estimate the external implementation's RAM or scratch disk.
/// They exclude heap-owned payloads and allocator overhead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedMergeEstimate {
    /// Records across all inputs. An exact count.
    pub input_records: u64,
    /// Records that can introduce a distinct key: values of `T`, tombstones,
    /// and merge ops. An exact count.
    pub key_bearing_records: u64,
    /// Upper bound on `K-ever`, the number of distinct keys the merge state
    /// retains. Tombstoned keys count: the collector keeps their entries.
    pub max_distinct_keys: u64,
    /// Resident index bytes for the largest single input, which is opened
    /// whole while its records are applied:
    /// `records * size_of::<RecordIndexEntry>()`.
    pub largest_input_index_bytes: u64,
    /// Transient bytes the largest single input's open can allocate *on top of*
    /// its resident index, and which are live at the same time as that index:
    /// the sequence-uniqueness witness copies one `u64` per record when the
    /// file's sequences are not already ascending in offset order.
    pub largest_input_open_transient_bytes: u64,
    /// Structural bytes of the merge-state map's entries at
    /// [`Self::max_distinct_keys`] keys:
    /// `max_distinct_keys * size_of::<(Key, (MergeOrder, Option<T>))>()`.
    ///
    /// This is not a bound on the map: it excludes hash-table load-factor
    /// slack, control bytes and bucket rounding, and all heap owned by
    /// individual keys and values.
    pub max_state_bytes: u64,
    /// Structural bytes of the output vector, which is reserved for the
    /// surviving entries while the merge state and its values are still alive
    /// `max_distinct_keys * size_of::<(MergeOrder, T)>()`.
    ///
    /// Like [`Self::max_state_bytes`] this counts inline storage only.
    pub max_output_values_bytes: u64,
}

impl KeyedMergeEstimate {
    /// Sum of the hypothetical fully resident implementation's structural
    /// terms. This is neither current merge memory usage nor an upper bound.
    #[must_use]
    pub fn peak_resident_structural_bytes(&self) -> u64 {
        self.max_state_bytes
            .saturating_add(self.max_output_values_bytes)
            .saturating_add(self.largest_input_index_bytes)
            .saturating_add(self.largest_input_open_transient_bytes)
    }
}

/// Counts merge inputs without decoding values. Directory buffers are bounded;
/// large inputs use temporary storage. Byte fields describe a hypothetical
/// fully resident implementation; see [`KeyedMergeEstimate`].
pub fn estimate_keyed_merge<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
) -> Result<KeyedMergeEstimate>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    crate::collections::ensure_registered_block::<T>(spec)?;
    let mut estimate = KeyedMergeEstimate::default();
    accumulate_keyed_merge_estimate::<T, _>(spec, base, &mut estimate)?;
    for delta in deltas {
        accumulate_keyed_merge_estimate::<T, _>(spec, delta, &mut estimate)?;
    }
    estimate.max_distinct_keys = estimate.key_bearing_records;
    let keys = usize::try_from(estimate.max_distinct_keys).unwrap_or(usize::MAX);
    estimate.max_state_bytes =
        allocation_bytes::<(T::Key, (MergeOrder, Option<T>))>(keys, "merge state")?;
    // Historical fully resident model, retained independently of execution.
    estimate.max_output_values_bytes = allocation_bytes::<(MergeOrder, T)>(keys, "merged values")?;
    Ok(estimate)
}

fn accumulate_keyed_merge_estimate<T, P>(
    spec: FormatSpec,
    path: P,
    estimate: &mut KeyedMergeEstimate,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let file = VarlitFile::open_readonly(spec, path)?;
    let records =
        u64::try_from(file.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    let mut key_bearing = 0usize;
    // Walks the file when the handle opened from an anchor without a list.
    for entry in file.resident_directory("estimate_keyed_merge")?.iter() {
        let block_id = entry?.block_id;
        if block_id == T::ID || block_id == TOMBSTONE_BLOCK_ID || block_id == OP_BLOCK_ID {
            key_bearing += 1;
        }
    }
    let key_bearing =
        u64::try_from(key_bearing).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    estimate.input_records = estimate.input_records.saturating_add(records);
    estimate.key_bearing_records = estimate.key_bearing_records.saturating_add(key_bearing);
    estimate.largest_input_index_bytes = estimate
        .largest_input_index_bytes
        .max(index_bytes_for_count(file.index.len())?);
    // the open that produced `file` also validates sequence
    // uniqueness, which can copy every sequence into an `8N` temporary held
    // alongside the resident index. Charging it here keeps
    // `peak_resident_structural_bytes` from ignoring a live allocation it can
    // actually see.
    estimate.largest_input_open_transient_bytes = estimate
        .largest_input_open_transient_bytes
        .max(sequence_uniqueness_transient_bytes(file.index.len())?);
    Ok(())
}

/// Merges `base` with `deltas` into `output`, keeping the last write per key.
///
/// Inputs are processed in shard, sequence and physical-ordinal order. Per-key
/// state, values and output ordering stay in bounded RAM buffers and spill to
/// private files only when full. Explicit finite `max_index_bytes` is honored;
/// the unbounded default uses 2 MiB per buffer. Per-operation
/// decoding uses `max_materialized_bytes`. One record/key and fixed tree-page
/// workspace remain minimum working storage. Buffers are capped independently;
/// they do not grow without bound with the total key count.
///
/// Scratch disk grows with history and must have free space. Output replacement
/// happens only after the complete result has been written and synced. Spilling
/// does not publish intermediate output. Custom key equality/hash semantics are
/// preserved, including hash collisions. Individual oversized records still
/// fail their decode/encode limits. Explicit key-count guards remain available.
pub fn merge_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    merge_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`merge_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// The collector checks `max_distinct_keys` before it admits each new key, so
/// an input whose cardinality exceeds the declared work limit fails with
/// [`Error::LimitExceeded`] (`resource = "merge distinct keys"`) at the
/// boundary instead of being discovered by the allocator. The ceiling counts
/// tombstoned keys, matching what the state actually retains.
pub fn merge_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    refuse_keyed_storage_merge(spec)?;
    let merged =
        external_merge::ExternalMerge::<T>::collect(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(spec, output.as_ref(), |out| merged.write(out))
}

/// Compacts `base` plus `deltas` into `output`, dropping superseded records.
/// Uses the bounded buffers and temporary files described by [`merge_keyed_files`].
/// [`compact_keyed_files_with_key_limit`] additionally bounds distinct keys.
pub fn compact_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    compact_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`compact_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// See [`merge_keyed_files_with_key_limit`] for the guard semantics.
pub fn compact_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    refuse_keyed_storage_merge(spec)?;
    let merged =
        external_merge::ExternalMerge::<T>::collect(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(spec, output.as_ref(), |out| merged.write(out))
}

/// Keyed merge and compaction read keyed blocks from the append log. A
/// keyed-storage format keeps them in its in-place heap instead, so the
/// walk would find none and write an empty output: refuse it.
fn refuse_keyed_storage_merge(spec: FormatSpec) -> Result<()> {
    if spec.keyed_storage.is_some() {
        return Err(Error::InvalidFormatSpec(
            "keyed merge and compaction are not supported for keyed storage",
        ));
    }
    Ok(())
}

fn write_keyed_values_atomically(
    spec: FormatSpec,
    output: &Path,
    populate: impl FnOnce(&mut VarlitFile) -> Result<()>,
) -> Result<()> {
    let _lock = WriterLock::acquire(output)?;
    // STO4-P2: the permission copy below is fallible and used to sit between
    // the temp's creation and the first scope that cleaned it up, so its `?`
    // could leave an empty temp behind. The guard owns the temp from creation,
    // so *every* early return from here on unlinks it; the deliberate
    // retentions below disarm it explicitly.
    let mut temp = RewriteTemp::create(output)?;
    take_injected_rewrite_temp_preparation_failure()?;
    if let Ok(metadata) = std::fs::metadata(output) {
        temp.file().set_permissions(metadata.permissions())?;
    }
    temp.close();
    let temp_path = temp.path().to_path_buf();

    let result = (|| {
        let mut out = VarlitFile::create(spec, &temp_path)?;
        populate(&mut out)?;
        out.flush()?;
        out.sync()
    })();
    if let Err(error) = result {
        // Delete eagerly rather than at drop: the marker cleanup below is only
        // correct once the temp itself is gone.
        temp.cleanup_now();
        remove_rewrite_temp_lock_marker(&temp_path);
        return Err(error);
    }

    let published = match replace_path_atomically(&temp_path, output) {
        // The rename consumed the temp pathname; there is nothing left to
        // unlink and the guard must not try.
        Ok(ReplaceDurability::Durable) => {
            temp.retain();
            Ok(())
        }
        // Publication already happened; the temp file no longer exists and the
        // target pathname resolves to the merged generation, so the caller
        // must not treat this as "target unchanged".
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            temp.retain();
            Err(crate::fallible::boxed_path_error(
                sync_error,
                output,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            ))
        }
        Err(error) => {
            if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
                // the temp may be the only surviving copy of the new
                // generation. Preserving it is the whole point of this arm, so
                // the guard is disarmed rather than allowed to unlink it.
                temp.retain();
            } else {
                temp.cleanup_now();
            }
            Err(error)
        }
    };
    remove_rewrite_temp_lock_marker(&temp_path);
    published
}

/// Removes the writer-lock marker that an internal rewrite temp left behind
///
///
/// Opening the temp through [`VarlitFile::create`] creates `<temp>.lock`.
/// Dropping that writer clears the marker's *contents* but deliberately keeps
/// the file: for a real user path the marker is a stable identity, and
/// unlinking it while another process may already hold the OS lock on that
/// inode would let a third process create a fresh marker at the same pathname
/// and "acquire" a lock on a different inode - exactly the race the persistent
/// marker exists to prevent. None of that applies to this pathname:
///
/// - it was generated by [`create_rewrite_temp_file`] with `create_new`, so
///   this operation exclusively owns it;
/// - the only way to regenerate the same name is another rewrite of the same
///   output in the same process, and the caller holds the output's writer lock
///   across the whole publication, so no such rewrite can be in flight;
/// - by the time this runs the temp has been published or deleted, so the
///   marker names a file that no longer exists.
///
/// The caller must therefore have dropped the temp's `VarlitFile` (releasing its
/// [`WriterLock`]) and must still hold the target's writer lock. If the temp is
/// still present - the indeterminate-publication case, where it is preserved
/// for out-of-band reconciliation - its marker is left with it.
fn remove_rewrite_temp_lock_marker(temp_path: &Path) {
    if temp_path.exists() {
        return;
    }
    let _ = remove_file(lock_path(temp_path));
}

/// Deletes a publication temp file after a pre-publication failure, but
/// preserves it after [`Error::ReplacePublicationIndeterminate`]: in that
/// state the temp may be the only surviving copy of the new generation and is
/// required for out-of-band reconciliation.
fn preserve_temp_on_indeterminate(temp_path: &Path, error: Error) -> Error {
    if !matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
        let _ = remove_file(temp_path);
    }
    error
}

/// Refuses a key the merge state has not seen once the caller's `K-ever`
/// ceiling is reached , so a cardinality the caller cannot afford
/// fails typed at the boundary instead of in the allocator.
fn admit_merge_key<T>(
    state: &HashMap<T::Key, (MergeOrder, Option<T>)>,
    key: &T::Key,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
{
    if state.contains_key(key) {
        return Ok(());
    }
    let admitted = u64::try_from(state.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "merge distinct keys",
    })?;
    if admitted >= max_distinct_keys {
        return Err(Error::LimitExceeded {
            resource: "merge distinct keys",
            actual: admitted.saturating_add(1),
            limit: max_distinct_keys,
        });
    }
    Ok(())
}

/// What one key of a merge state holds inline, charged to the
/// materialization budget when the key first appears.
fn merge_slot_bytes<T: VarlitKeyedBlock>() -> u64 {
    std::mem::size_of::<(T::Key, (MergeOrder, Option<T>))>() as u64
}

fn apply_merge_entries<T, D: RecordDirectory + ?Sized>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entries: &D,
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
    budget: &mut MaterializationBudget,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
{
    // One payload buffer for the whole merge walk, not one per record.
    let mut payload = Vec::new();
    for (record_ordinal, entry) in directory_records(entries).enumerate() {
        let entry = entry?;
        let order = MergeOrder::for_record(shard.ordinal, entry.sequence, record_ordinal);
        match entry.block_id {
            id if id == T::ID => {
                if entry.block_version != T::VERSION {
                    return Err(Error::BlockVersionMismatch {
                        block_id: T::ID,
                        expected: T::VERSION,
                        actual: entry.block_version,
                    });
                }
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_logical_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let block: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                let key = block.key();
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    // A new key holds its slot whatever its payload weighed:
                    // a forty-byte tombstone costs the whole inline `T`.
                    if !state.contains_key(&key) {
                        budget.consume(merge_slot_bytes::<T>())?;
                    }
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, Some(block)));
                }
            }
            TOMBSTONE_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let Some(key) = decode_internal_key_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    // A new key holds its slot whatever its payload weighed:
                    // a forty-byte tombstone costs the whole inline `T`.
                    if !state.contains_key(&key) {
                        budget.consume(merge_slot_bytes::<T>())?;
                    }
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, None));
                }
            }
            OP_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let Some((key, op)) =
                    decode_internal_op_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if !should_apply(state.get(&key), order) {
                    continue;
                }
                let Some((_, Some(mut value))) = state.remove(&key) else {
                    return Err(Error::MissingMergeTarget);
                };
                value.apply_op(op)?;
                state.insert(key, (order, Some(value)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MergeShard {
    ordinal: usize,
}

impl MergeShard {
    fn single_file() -> Self {
        Self { ordinal: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MergeOrder {
    shard_ordinal: usize,
    sequence: u64,
    record_ordinal: usize,
}

impl MergeOrder {
    const fn for_record(shard_ordinal: usize, sequence: u64, record_ordinal: usize) -> Self {
        Self {
            shard_ordinal,
            sequence,
            record_ordinal,
        }
    }
}

fn should_apply<T>(current: Option<&(MergeOrder, Option<T>)>, order: MergeOrder) -> bool {
    current.is_none_or(|(old_order, _)| order >= *old_order)
}
