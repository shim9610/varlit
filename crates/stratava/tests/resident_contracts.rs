// Resident-facade contract regressions for the 2026-07-19 adversarial review.
//
// API2-05 (report finding API-02): `StratavaFile::push_info` and
// `StratavaWriter::push_info` used to write `prev_same_key_offset = None` for a
// second record with an equal key while the generated keyed writer linked it
// correctly, leaving a silently truncated physical keyed chain. `T: StratavaBlock`
// exposes no key, so those entry points now reject keyed blocks on
// keyed-chaining formats and the maintaining `push_keyed`/`delete` paths carry
// the chain.
//
// PERF2-05 (report finding PERF-05): resident block-offset chaining resolved
// the predecessor with `index.iter().rev().find(...)`, costing Theta(g) per
// append and O(N*B) overall. The predecessor now comes from a maintained tail
// table that never reads the resident index on the append path.

use std::path::Path;

#[cfg(feature = "scalable-fault-injection")]
use stratava::StratavaFile;
use stratava::{Error, stratava_format};

stratava_format! {
    pub format ResidentChainFormat {
        magic: b"RESCHN";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            fixed Marker(id = 10) {
                tag: u32,
            }
            variable Item(id = 11, key = [id]) {
                id: u64,
                value: u32,
            }
            fixed Note(id = 12) {
                note: u32,
            }
        }
    }
}

// API3-02, updated round 9 (F-01): the generated keyed writer no longer owns a
// `HashMap<T::Key, u64>` of its own. It routes through the same byte-keyed
// resident tail cache the generic keyed API uses, so its per-entry charge is
// identical to `ResidentTailBudgetFormat`'s below - inline `(Vec<u8>, u64)`
// storage plus the canonical key payload bytes the map owns. The budget here is
// therefore the same 120 bytes, which admits the first two distinct `u64` keys
// (52, then 104) and refuses the third (156).
stratava_format! {
    pub format GeneratedTailBudgetFormat {
        magic: b"GENBGT";
        version: 1;
        limits {
            keyed_tail: 120;
        }
        index: keyed_offset_chain;
        blocks {
            variable Tracked(id = 41, key = [id]) {
                id: u64,
                value: u32,
            }
        }
    }
}

// API3-02: a keyed-chaining format with a deliberately tiny keyed-tail budget,
// so the resident and generated keyed writers must both refuse growth of the
// resident tail cache at a typed boundary.
stratava_format! {
    pub format ResidentTailBudgetFormat {
        magic: b"RESBGT";
        version: 1;
        limits {
            keyed_tail: 120;
        }
        index: keyed_offset_chain;
        blocks {
            variable Budgeted(id = 31, key = [id]) {
                id: u64,
                value: u32,
            }
        }
    }
}

stratava_format! {
    pub format ResidentPlainFormat {
        magic: b"RESPLN";
        version: 1;
        index: scan_on_open;
        blocks {
            variable Entry(id = 21, key = [id]) {
                id: u64,
                value: u32,
            }
        }
    }
}

/// `(record_offset, prev_same_block_offset, prev_same_key_offset)` for one
/// record.
type ChainLink = (u64, Option<u64>, Option<u64>);

/// Every [`ChainLink`] for `block_id`, read back from a freshly opened file so
/// the assertions are against the persisted footers rather than writer state.
fn chain(path: &Path, block_id: u32) -> stratava::Result<Vec<ChainLink>> {
    let file = ResidentChainFormat::open(path)?;
    Ok(file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == block_id)
        .map(|entry| {
            (
                entry.record_offset,
                entry.prev_same_block_offset,
                entry.prev_same_key_offset,
            )
        })
        .collect())
}

fn assert_keyed_chain_rejection(error: &Error) {
    assert!(
        matches!(
            error,
            Error::KeyedChainRequiresKeyedApi {
                block_id: <Item as stratava::StratavaBlock>::ID
            }
        ),
        "expected a typed keyed-chain rejection, got {error:?}"
    );
}

#[test]
fn generic_push_rejects_keyed_blocks_on_keyed_chaining_formats() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generic-push-reject.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    assert_keyed_chain_rejection(
        &file
            .push_info(&Item { id: 1, value: 1 })
            .expect_err("StratavaFile::push_info must refuse a keyed block here"),
    );
    assert_keyed_chain_rejection(
        &file
            .push(&Item { id: 1, value: 1 })
            .expect_err("StratavaFile::push must refuse a keyed block here"),
    );
    // Unkeyed blocks are unaffected.
    file.push(&Marker { tag: 7 })?;
    file.flush()?;
    drop(file);

    let mut writer = ResidentChainFormat::spec().open_writer(&path)?;
    assert_keyed_chain_rejection(
        &writer
            .push_info(&Item { id: 1, value: 1 })
            .expect_err("StratavaWriter::push_info must refuse a keyed block here"),
    );
    assert_keyed_chain_rejection(
        &writer
            .push(&Item { id: 1, value: 1 })
            .expect_err("StratavaWriter::push must refuse a keyed block here"),
    );
    drop(writer);

    // The refused pushes wrote nothing.
    assert!(chain(&path, <Item as stratava::StratavaBlock>::ID)?.is_empty());
    Ok(())
}

#[test]
fn generic_push_accepts_keyed_blocks_without_keyed_chaining() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generic-push-unchained.stratava");

    let mut file = ResidentPlainFormat::create(&path)?;
    file.push(&Entry { id: 1, value: 1 })?;
    file.push(&Entry { id: 1, value: 2 })?;
    file.flush()?;
    drop(file);

    let file = ResidentPlainFormat::open(&path)?;
    assert_eq!(file.index_entries().len(), 2);
    for entry in file.index_entries() {
        assert_eq!(entry.prev_same_key_offset, None);
    }
    Ok(())
}

#[test]
fn maintaining_keyed_facades_link_the_predecessor_chain() -> stratava::Result<()> {
    let item_id = <Item as stratava::StratavaBlock>::ID;

    // Facade 1: the maintaining generic path on StratavaFile.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file-push-keyed.stratava");
    let mut file = ResidentChainFormat::create(&path)?;
    file.push_keyed(&Item { id: 4, value: 1 })?;
    file.push_keyed(&Item { id: 9, value: 1 })?;
    file.push_keyed(&Item { id: 4, value: 2 })?;
    file.flush()?;
    drop(file);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0].2, None,
        "first record of key 4 has no predecessor"
    );
    assert_eq!(
        records[1].2, None,
        "first record of key 9 has no predecessor"
    );
    assert_eq!(
        records[2].2,
        Some(records[0].0),
        "second record of key 4 must link to the first"
    );

    // Facade 2: the same through StratavaWriter.
    let path = directory.path().join("writer-push-keyed.stratava");
    let mut writer = ResidentChainFormat::spec().create_writer(&path)?;
    writer.push_keyed(&Item { id: 4, value: 1 })?;
    writer.push_keyed(&Item { id: 4, value: 2 })?;
    writer.flush()?;
    drop(writer);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].2, Some(records[0].0));

    // Facade 3: the generated keyed writer, which already linked correctly.
    let path = directory.path().join("generated-push-keyed.stratava");
    let mut generated = ResidentChainFormat::create_writer(&path)?;
    generated.push_item(&Item { id: 4, value: 1 })?;
    generated.push_item(&Item { id: 4, value: 2 })?;
    generated.flush()?;
    drop(generated);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].2, Some(records[0].0));

    Ok(())
}

#[test]
fn keyed_delete_links_the_predecessor_chain() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("delete-keyed.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;
    file.delete::<Item>(&4)?;
    file.flush()?;
    drop(file);

    let opened = ResidentChainFormat::open(&path)?;
    let entries = opened.index_entries();
    let tombstone = entries
        .iter()
        .find(|entry| entry.block_id == stratava::TOMBSTONE_BLOCK_ID)
        .expect("tombstone record");
    assert_eq!(
        tombstone.prev_same_key_offset,
        Some(first.record_offset),
        "the tombstone must link to the record it supersedes"
    );
    Ok(())
}

#[test]
fn keyed_chains_survive_reopen() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-reopen.stratava");
    let item_id = <Item as stratava::StratavaBlock>::ID;

    let mut file = ResidentChainFormat::create(&path)?;
    file.push_keyed(&Item { id: 4, value: 1 })?;
    file.flush()?;
    drop(file);

    let mut reopened = ResidentChainFormat::open(&path)?;
    reopened.push_keyed(&Item { id: 4, value: 2 })?;
    reopened.push_keyed(&Item { id: 5, value: 1 })?;
    reopened.flush()?;
    drop(reopened);

    let mut reopened = ResidentChainFormat::open(&path)?;
    reopened.push_keyed(&Item { id: 4, value: 3 })?;
    reopened.flush()?;
    drop(reopened);

    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].2, None);
    assert_eq!(
        records[1].2,
        Some(records[0].0),
        "the chain must cross the first reopen"
    );
    assert_eq!(records[2].2, None, "key 5 starts its own chain");
    assert_eq!(
        records[3].2,
        Some(records[1].0),
        "the chain must cross the second reopen"
    );
    Ok(())
}

/// The keyed tail cache must not survive a caller-supplied predecessor: the
/// file cannot observe which key that record carried, so the next maintained
/// append has to rebuild from the resident index.
#[test]
fn maintained_keyed_tails_recover_after_an_explicit_predecessor() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-mixed.stratava");
    let item_id = <Item as stratava::StratavaBlock>::ID;

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;
    let second =
        file.push_with_prev_key_info(&Item { id: 4, value: 2 }, Some(first.record_offset))?;
    file.push_keyed(&Item { id: 4, value: 3 })?;
    file.flush()?;
    drop(file);

    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].2, Some(first.record_offset));
    assert_eq!(
        records[2].2,
        Some(second.record_offset),
        "the rebuilt tail must see the explicitly linked record"
    );
    Ok(())
}

#[test]
fn interleaved_block_ids_chain_correctly_across_reopen() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("block-tails.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    for round in 0..8u32 {
        file.push(&Marker { tag: round })?;
        file.push(&Note { note: round })?;
        file.push_keyed(&Item {
            id: u64::from(round),
            value: round,
        })?;
    }
    file.flush()?;
    drop(file);

    // Reopen and keep appending: the tails must be recovered from the resident
    // index, not restarted.
    let mut file = ResidentChainFormat::open(&path)?;
    for round in 8..12u32 {
        file.push(&Marker { tag: round })?;
        file.push(&Note { note: round })?;
        file.push_keyed(&Item {
            id: u64::from(round),
            value: round,
        })?;
    }
    file.flush()?;
    drop(file);

    for block_id in [
        <Marker as stratava::StratavaBlock>::ID,
        <Note as stratava::StratavaBlock>::ID,
        <Item as stratava::StratavaBlock>::ID,
    ] {
        let records = chain(&path, block_id)?;
        assert_eq!(records.len(), 12, "block {block_id}");
        assert_eq!(
            records[0].1, None,
            "block {block_id} head has no predecessor"
        );
        for window in records.windows(2) {
            assert_eq!(
                window[1].1,
                Some(window[0].0),
                "block {block_id} chain must link consecutive records"
            );
        }
    }
    Ok(())
}

/// PERF2-05: the append-time predecessor lookup must not scale with the
/// resident index. The counter records resident index entries examined by the
/// block-tail machinery; wholesale index loads pay one pass, appends must pay
/// nothing at all, so the delta over an append window is zero for any record
/// count.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn append_time_predecessor_lookup_never_reads_the_resident_index() -> stratava::Result<()> {
    /// Returns `(index entries examined while appending, entries examined by
    /// the reopen that follows)`.
    fn measure(rounds: u32, name: &str) -> stratava::Result<(u64, u64)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut file = ResidentChainFormat::create(&path)?;
        // Alternating block ids maximized the old reverse scan: every append
        // walked back past the other block's record.
        let before_appends = StratavaFile::block_tail_index_touches();
        for round in 0..rounds {
            file.push(&Marker { tag: round })?;
            file.push(&Note { note: round })?;
        }
        let after_appends = StratavaFile::block_tail_index_touches();
        file.flush()?;
        drop(file);

        let reopened = ResidentChainFormat::open(&path)?;
        let after_open = StratavaFile::block_tail_index_touches();
        drop(reopened);
        Ok((after_appends - before_appends, after_open - after_appends))
    }

    let (small_appends, small_open) = measure(256, "touches-256.stratava")?;
    let (large_appends, large_open) = measure(2_048, "touches-2048.stratava")?;

    assert_eq!(small_appends, 0, "appends must not read the resident index");
    assert_eq!(large_appends, 0, "appends must not read the resident index");
    // Open used to pay a second pass over the index to rebuild the tails, and
    // this asserted that pass was there. H1 removed it: the tails cannot come
    // from the resident index any more, because a non-resident block's records
    // are not in it, so the scan collects them as it goes and orders them once
    // at the end. One pass instead of two, and the counter that measured the
    // second one now reads zero at open by construction. It stays wired through
    // the generation-rebind paths, which still call `BlockTails::from_index`.
    assert_eq!(
        small_open, 0,
        "open must not pay a separate index pass for tails: {small_open}"
    );
    assert_eq!(
        large_open, 0,
        "open must not pay a separate index pass for tails: {large_open}"
    );
    Ok(())
}

// PERF3-03 (report finding PERF-03): `BlockTails::from_index` claimed
// `O(N log B)` but inserted every first-seen block id into a sorted vector, so
// an index whose first appearances descend by id moved `0 + 1 + ... + (B - 1)`
// tuples - a real `O(N log B + B^2)`. The previous test could not see this
// because it counted resident *index visits*, which the insertion cost does
// not touch. Construction now orders the distinct ids once.
//
// PERF3-05 (report finding PERF-05): every resident input open could copy all
// `N` sequences into an `8N` temporary and sort it, and `KeyedMergeEstimate`
// charged neither the temporary nor the sort. The estimate is the published
// way to size this resident-only family, so an estimate that omits a live
// allocation is a defect in the deliverable itself.

stratava_format! {
    pub format ResidentWideFormat {
        magic: b"RESWID";
        version: 1;
        index: scan_on_open;
        blocks {
            fixed Wide30(id = 30) {
                tag: u32,
            }
            fixed Wide31(id = 31) {
                tag: u32,
            }
            fixed Wide32(id = 32) {
                tag: u32,
            }
            fixed Wide33(id = 33) {
                tag: u32,
            }
            fixed Wide34(id = 34) {
                tag: u32,
            }
            fixed Wide35(id = 35) {
                tag: u32,
            }
            fixed Wide36(id = 36) {
                tag: u32,
            }
            fixed Wide37(id = 37) {
                tag: u32,
            }
            fixed Wide38(id = 38) {
                tag: u32,
            }
            fixed Wide39(id = 39) {
                tag: u32,
            }
            fixed Wide40(id = 40) {
                tag: u32,
            }
            fixed Wide41(id = 41) {
                tag: u32,
            }
            fixed Wide42(id = 42) {
                tag: u32,
            }
            fixed Wide43(id = 43) {
                tag: u32,
            }
            fixed Wide44(id = 44) {
                tag: u32,
            }
            fixed Wide45(id = 45) {
                tag: u32,
            }
        }
    }
}

/// PERF3-03: wholesale block-tail construction must move no tuples at all, in
/// the exact worst case that used to be quadratic. This counter observes
/// vector movement, which is the cost the index-visit counter cannot see.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn block_tail_construction_has_no_quadratic_term() -> stratava::Result<()> {
    /// Appends `rounds` descending sweeps over all 16 block ids and reopens the
    /// file. Returns `(tuples moved while appending, tuples moved by the
    /// reopen)`.
    fn measure(rounds: u32, name: &str) -> stratava::Result<(u64, u64)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut file = ResidentWideFormat::create(&path)?;
        let before_appends = StratavaFile::block_tail_entries_moved();
        for round in 0..rounds {
            // Descending first appearances are the worst case for insertion
            // into a vector kept sorted ascending by block id.
            file.push(&Wide45 { tag: round })?;
            file.push(&Wide44 { tag: round })?;
            file.push(&Wide43 { tag: round })?;
            file.push(&Wide42 { tag: round })?;
            file.push(&Wide41 { tag: round })?;
            file.push(&Wide40 { tag: round })?;
            file.push(&Wide39 { tag: round })?;
            file.push(&Wide38 { tag: round })?;
            file.push(&Wide37 { tag: round })?;
            file.push(&Wide36 { tag: round })?;
            file.push(&Wide35 { tag: round })?;
            file.push(&Wide34 { tag: round })?;
            file.push(&Wide33 { tag: round })?;
            file.push(&Wide32 { tag: round })?;
            file.push(&Wide31 { tag: round })?;
            file.push(&Wide30 { tag: round })?;
        }
        let after_appends = StratavaFile::block_tail_entries_moved();
        file.flush()?;
        drop(file);

        let reopened = ResidentWideFormat::open(&path)?;
        let after_open = StratavaFile::block_tail_entries_moved();
        drop(reopened);
        Ok((after_appends - before_appends, after_open - after_appends))
    }

    let (small_appends, small_open) = measure(4, "wide-tails-4.stratava")?;
    let (large_appends, large_open) = measure(64, "wide-tails-64.stratava")?;

    // Calibration: the counter is wired and this really is the worst case. The
    // append path still inserts, but only once per distinct id for the whole
    // life of the file, so its movement does not grow with the record count.
    assert_eq!(
        small_appends, large_appends,
        "insertion movement must depend on distinct ids, not record count"
    );
    assert!(
        small_appends >= (16 * 15) / 2,
        "descending ids must be the insertion worst case: {small_appends}"
    );
    // The regression: rebuilding the tails on open must not repeat that
    // movement, for any record count.
    assert_eq!(
        small_open, 0,
        "tail construction must not move tuples: {small_open}"
    );
    assert_eq!(
        large_open, 0,
        "tail construction must not move tuples: {large_open}"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, stratava::StratavaBlock)]
#[stratava(id = 60, version = 1, kind = "variable", key = "user_id")]
struct ResidentMergeUser {
    #[stratava(field_id = 1)]
    user_id: u64,
    #[stratava(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, stratava::StratavaBlock)]
#[stratava(id = 61, version = 1, kind = "variable")]
struct ResidentMergeUserOp {
    #[stratava(field_id = 1)]
    rename_to: String,
}

impl stratava::StratavaMerge for ResidentMergeUser {
    type Op = ResidentMergeUserOp;

    fn apply_op(&mut self, op: Self::Op) -> stratava::Result<()> {
        self.name = op.rename_to;
        Ok(())
    }
}

stratava_format! {
    pub struct ResidentEstimateFormat {
        magic: b"RESEST";
        version: 1;
        endian: little;
        blocks: [ResidentMergeUser, ResidentMergeUserOp];
    }
}

/// PERF3-05: the pre-flight estimate must charge the transient an input open
/// allocates alongside its resident index, so
/// `peak_resident_structural_bytes` does not ignore a live allocation it can
/// actually see.
#[test]
fn resident_merge_estimate_includes_the_open_time_transient() -> stratava::Result<()> {
    const BASE_KEYS: u64 = 300;
    const DELTA_KEYS: u64 = 40;

    fn write_input(path: &Path, keys: u64) -> stratava::Result<()> {
        let mut file = ResidentEstimateFormat::create(path)?;
        for key in 0..keys {
            file.push(&ResidentMergeUser {
                user_id: key,
                name: format!("user-{key}"),
            })?;
        }
        file.flush()?;
        Ok(())
    }

    let directory = tempfile::tempdir()?;
    let base = directory.path().join("estimate-base.stratava");
    let delta = directory.path().join("estimate-delta.stratava");
    write_input(&base, BASE_KEYS)?;
    write_input(&delta, DELTA_KEYS)?;

    let estimate = stratava::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        base.as_path(),
        &[delta.as_path()],
    )?;

    // Derived floor: the largest input holds at least `BASE_KEYS` records and
    // the uniqueness witness copies one `u64` per record of that input.
    let floor = BASE_KEYS * (size_of::<u64>() as u64);
    assert!(
        estimate.largest_input_open_transient_bytes >= floor,
        "the open transient must be charged: {} < {floor}",
        estimate.largest_input_open_transient_bytes
    );
    assert!(
        estimate.peak_resident_structural_bytes()
            >= estimate.max_state_bytes + estimate.largest_input_index_bytes + floor,
        "the structural peak must include the state, the largest index, and the transient"
    );
    // The transient is a per-input cost, so it tracks the largest input rather
    // than the delta or the total.
    let base_only = stratava::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        base.as_path(),
        &[],
    )?;
    assert_eq!(
        base_only.largest_input_open_transient_bytes, estimate.largest_input_open_transient_bytes,
        "a smaller delta must not change the largest input's transient"
    );

    let bigger = directory.path().join("estimate-bigger.stratava");
    write_input(&bigger, BASE_KEYS * 4)?;
    let bigger_estimate = stratava::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        bigger.as_path(),
        &[base.as_path()],
    )?;
    assert!(
        bigger_estimate.largest_input_open_transient_bytes
            > estimate.largest_input_open_transient_bytes,
        "the transient must scale with the largest input's record count"
    );
    assert!(
        bigger_estimate.peak_resident_structural_bytes()
            > estimate.peak_resident_structural_bytes(),
        "the structural peak must grow with the inputs"
    );
    Ok(())
}

// API3-01 (report finding F-01): the generic keyed mutation paths appended the
// record first and only then grew the resident keyed-tail cache. A failing
// cache reservation therefore returned `Err` *after* the record was written,
// indexed and published in writer state, and the superseded predecessor stayed
// cached - so the next generic keyed mutation on the same writer linked around
// a record that had in fact succeeded, silently truncating the physical keyed
// chain. Reservation now happens before the append and the post-append commit
// is infallible.

/// Arming helper: the hooks are thread-local, and the test harness gives every
/// test its own thread, so this cannot leak into a sibling test.
#[cfg(feature = "scalable-fault-injection")]
fn assert_tail_allocation_failure(error: &Error) {
    assert!(
        matches!(
            error,
            Error::AllocationFailed {
                resource: "keyed tail offsets",
                ..
            }
        ),
        "expected a typed keyed-tail allocation failure, got {error:?}"
    );
}

/// A failed tail reservation on the push path must mean the record was never
/// appended, and must leave the cache able to link the next mutation.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn keyed_push_tail_reservation_fails_before_the_append() -> stratava::Result<()> {
    let item_id = <Item as stratava::StratavaBlock>::ID;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-push-reserve-fail.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;

    // Key 9 is absent from the cache, so this push has to reserve a slot.
    StratavaFile::inject_keyed_tail_reservation_failures(1);
    let error = file
        .push_keyed(&Item { id: 9, value: 1 })
        .expect_err("the injected tail reservation must fail the push");
    StratavaFile::inject_keyed_tail_reservation_failures(0);
    assert_tail_allocation_failure(&error);

    // Same writer: the cache must still describe the file exactly.
    file.push_keyed(&Item { id: 4, value: 2 })?;
    file.push_keyed(&Item { id: 9, value: 1 })?;
    file.flush()?;
    drop(file);

    let records = chain(&path, item_id)?;
    assert_eq!(
        records.len(),
        3,
        "the failed push must not have appended a record"
    );
    assert_eq!(records[0].2, None, "first record of key 4");
    assert_eq!(
        records[1].2,
        Some(first.record_offset),
        "key 4 must still link to its predecessor after the failed push"
    );
    assert_eq!(
        records[2].2, None,
        "key 9's first surviving record has no predecessor, because the \
         refused push wrote nothing to link to"
    );
    Ok(())
}

/// The same contract on the tombstone path.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn keyed_delete_tail_reservation_fails_before_the_append() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-delete-reserve-fail.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;

    // Key 9 is absent from the cache, so this delete has to reserve a slot.
    StratavaFile::inject_keyed_tail_reservation_failures(1);
    let error = file
        .delete::<Item>(&9)
        .expect_err("the injected tail reservation must fail the delete");
    StratavaFile::inject_keyed_tail_reservation_failures(0);
    assert_tail_allocation_failure(&error);

    file.delete::<Item>(&4)?;
    file.flush()?;
    drop(file);

    let tombstones = chain(&path, stratava::TOMBSTONE_BLOCK_ID)?;
    assert_eq!(
        tombstones.len(),
        1,
        "the failed delete must not have appended a tombstone"
    );
    assert_eq!(
        tombstones[0].2,
        Some(first.record_offset),
        "the surviving tombstone must link to the record it supersedes"
    );
    Ok(())
}

/// If the reserved slot is somehow unusable after the append, the cached map
/// must be discarded, never left holding the superseded predecessor: the next
/// mutation has to link to the record that succeeded, not around it.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn keyed_tail_commit_loss_never_links_around_the_committed_record() -> stratava::Result<()> {
    let item_id = <Item as stratava::StratavaBlock>::ID;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-commit-loss.stratava");

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;

    StratavaFile::inject_keyed_tail_commit_losses(1);
    let second = file.push_keyed_info(&Item { id: 4, value: 2 })?;
    StratavaFile::inject_keyed_tail_commit_losses(0);

    let third = file.push_keyed_info(&Item { id: 4, value: 3 })?;

    StratavaFile::inject_keyed_tail_commit_losses(1);
    file.delete::<Item>(&4)?;
    StratavaFile::inject_keyed_tail_commit_losses(0);

    file.push_keyed(&Item { id: 4, value: 4 })?;
    file.flush()?;
    drop(file);

    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].2, None);
    assert_eq!(records[1].2, Some(first.record_offset));
    assert_eq!(
        records[2].2,
        Some(second.record_offset),
        "a lost cache update must not make the next push link around the \
         record that succeeded"
    );

    let tombstones = chain(&path, stratava::TOMBSTONE_BLOCK_ID)?;
    assert_eq!(tombstones.len(), 1);
    assert_eq!(
        tombstones[0].2,
        Some(third.record_offset),
        "the tombstone links to the newest record for the key"
    );
    assert_eq!(
        records[3].2,
        Some(tombstones[0].0),
        "a lost cache update on the tombstone must not make the next push \
         link around the tombstone"
    );
    Ok(())
}

// PERF4-04 (report finding F-04): `KeyedMergeEstimate::peak_resident_bytes()`
// was published as an *upper bound* while its state term counted only
// `count * size_of::<(Key, (MergeOrder, Option<T>))>()`. That omits HashMap
// bucket slack and control bytes, the output vector reserved while the map is
// still alive, and - without limit - every byte of heap owned by a `Key` or a
// `T`. It is now named and documented as a structural estimate, and the output
// vector is charged. The test below measures a real allocated high-water mark
// so the *documented weaker property* is verified against behaviour rather
// than re-derived from the same arithmetic.

/// Thread-attributed allocation high-water mark.
///
/// The measurement window is opened and closed on one thread and the test
/// harness gives every test its own thread, so a concurrently running sibling
/// test cannot pollute it. All state is `const`-initialised thread-local
/// `Cell`s, so the hook itself never allocates and cannot recurse.
mod peak_allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    std::thread_local! {
        /// `(measuring, live bytes, peak live bytes)`.
        static STATE: Cell<(bool, i64, i64)> = const { Cell::new((false, 0, 0)) };
    }

    fn note(delta: i64) {
        let _ = STATE.try_with(|state| {
            let (measuring, live, peak) = state.get();
            if !measuring {
                return;
            }
            let live = live + delta;
            state.set((true, live, if live > peak { live } else { peak }));
        });
    }

    pub struct PeakTracking;

    // SAFETY: every method forwards to `System` unchanged and only records
    // sizes around it, so the allocator contract is exactly `System`'s.
    unsafe impl GlobalAlloc for PeakTracking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                note(layout.size() as i64);
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                note(layout.size() as i64);
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            note(-(layout.size() as i64));
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let moved = unsafe { System.realloc(pointer, layout, new_size) };
            if !moved.is_null() {
                note(new_size as i64 - layout.size() as i64);
            }
            moved
        }
    }

    /// Runs `body` and returns its result with the peak live heap observed on
    /// this thread while it ran.
    pub fn measure<R>(body: impl FnOnce() -> R) -> (R, u64) {
        STATE.with(|state| state.set((true, 0, 0)));
        let result = body();
        let (_, _, peak) = STATE.with(|state| state.get());
        STATE.with(|state| state.set((false, 0, 0)));
        (result, u64::try_from(peak).unwrap_or(0))
    }
}

#[global_allocator]
static PEAK_TRACKING_ALLOCATOR: peak_allocations::PeakTracking = peak_allocations::PeakTracking;

/// PERF4-04: the estimate is a structural count model. Its documented identity
/// must hold exactly, and - for heap-owning `Key`/`T` - the real allocated
/// peak must exceed it, which is precisely why the old "upper bound" wording
/// was a defect rather than a rounding error.
#[test]
fn resident_merge_estimate_is_structural_and_not_an_allocated_bound() -> stratava::Result<()> {
    const KEYS: u64 = 256;
    /// Big enough that the `String` heap the count model cannot see dominates
    /// every structural term put together.
    const NAME_BYTES: usize = 4096;

    fn write_input(path: &Path, keys: u64) -> stratava::Result<()> {
        let mut file = ResidentEstimateFormat::create(path)?;
        for key in 0..keys {
            file.push(&ResidentMergeUser {
                user_id: key,
                name: "n".repeat(NAME_BYTES),
            })?;
        }
        file.flush()?;
        Ok(())
    }

    let directory = tempfile::tempdir()?;
    let base = directory.path().join("peak-base.stratava");
    let delta = directory.path().join("peak-delta.stratava");
    let merged = directory.path().join("peak-merged.stratava");
    write_input(&base, KEYS)?;
    write_input(&delta, KEYS / 4)?;

    let estimate = stratava::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        base.as_path(),
        &[delta.as_path()],
    )?;

    // The documented identity: the structural peak is exactly the sum of its
    // four structural terms, and each byte term is a plain count model.
    assert_eq!(
        estimate.peak_resident_structural_bytes(),
        estimate.max_state_bytes
            + estimate.max_output_values_bytes
            + estimate.largest_input_index_bytes
            + estimate.largest_input_open_transient_bytes,
        "the structural peak must be the documented sum"
    );
    assert!(
        estimate.max_output_values_bytes > 0,
        "the output vector is reserved while the map is alive and must be charged"
    );
    assert_eq!(
        estimate.max_state_bytes % estimate.max_distinct_keys,
        0,
        "the state term is `keys * size_of::<entry>()`"
    );
    assert_eq!(
        estimate.max_output_values_bytes % estimate.max_distinct_keys,
        0,
        "the output term is `keys * size_of::<entry>()`"
    );
    assert_eq!(
        estimate.largest_input_open_transient_bytes,
        KEYS * (size_of::<u64>() as u64),
        "the open transient is one u64 per record of the largest input"
    );

    // Force spill with an explicit working budget. A generous budget is now
    // allowed to retain encoded values in RAM, so it cannot prove spill bounds.
    let (result, peak) = peak_allocations::measure(|| {
        stratava::merge_keyed_files::<ResidentMergeUser, _>(
            ResidentEstimateFormat::spec().with_resource_limits(
                stratava::ReadLimits::STANDARD.with_max_index_bytes(64 * 1024),
            ),
            base.as_path(),
            &[delta.as_path()],
            merged.as_path(),
        )
    });
    result?;

    // Calibration: the allocator hook is wired and saw the merge.
    assert!(
        peak >= NAME_BYTES as u64,
        "the counting allocator must observe at least one decoded value: {peak}"
    );
    assert!(
        peak > estimate.peak_resident_structural_bytes(),
        "the structural estimate is not an allocated upper bound for \
         heap-owning values: measured peak {peak} must exceed estimate {}",
        estimate.peak_resident_structural_bytes()
    );

    assert!(
        peak < (KEYS * NAME_BYTES as u64) / 2,
        "external merge retained the full value set: {peak}"
    );

    // The merge itself is unaffected by the accounting change.
    let reopened = ResidentEstimateFormat::open(&merged)?;
    assert_eq!(reopened.index_entries().len() as u64, KEYS);
    Ok(())
}

// Cache budgets select spill boundaries; owned result limits remain separate.
#[test]
fn resident_keyed_tail_growth_spills_and_preserves_predecessors() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resident-tail-budget.stratava");
    let mut file = ResidentTailBudgetFormat::create(&path)?;
    for round in 0..3 {
        for id in 0..32 {
            file.push_keyed(&Budgeted { id, value: round })?;
        }
    }
    file.flush()?;
    drop(file);
    let file = ResidentTailBudgetFormat::open(&path)?;
    let records: Vec<_> = file
        .index_entries()
        .iter()
        .filter(|r| r.block_id == <Budgeted as stratava::StratavaBlock>::ID)
        .map(|r| {
            (
                r.record_offset,
                r.prev_same_block_offset,
                r.prev_same_key_offset,
            )
        })
        .collect();
    assert_eq!(records.len(), 96);
    for i in 32..96 {
        assert_eq!(records[i].2, Some(records[i - 32].0));
    }
    Ok(())
}

// Cache budgets select spill boundaries; owned result limits remain separate.
#[test]
fn generated_keyed_writer_spills_tails_for_puts_and_tombstones() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generated-tail-budget.stratava");
    let mut writer = GeneratedTailBudgetFormat::create_writer(&path)?;
    for id in 0..32 {
        writer.push_tracked(&Tracked { id, value: 1 })?;
    }
    writer.delete_tracked(&4)?;
    writer.push_tracked(&Tracked { id: 4, value: 2 })?;
    writer.flush()?;
    drop(writer);
    let mut writer = GeneratedTailBudgetFormat::open_writer(&path)?;
    writer.push_tracked(&Tracked { id: 4, value: 3 })?;
    writer.flush()?;
    drop(writer);
    let file = GeneratedTailBudgetFormat::open(&path)?;
    let values: Vec<Tracked> = file
        .blocks::<Tracked>()?
        .iter()
        .collect::<stratava::Result<_>>()?;
    assert_eq!(values.len(), 34);
    assert_eq!(values.last(), Some(&Tracked { id: 4, value: 3 }));
    Ok(())
}

// API3-05: twins of the two budgeted formats above. They share their magic,
// version, index policy and block layout but declare no keyed-tail ceiling, so
// they can author a file holding more distinct keys than the budgeted format's
// ceiling admits. Neither side declares `schema_hash: computed`, so the
// budgeted format opens what its twin wrote.
//
// These exist because the round-6 tests only ever created a *fresh* file and
// then grew the tail map incrementally, which cannot see the dominant
// allocation: the map built from file content at writer construction
// (`writer_tail_inits` -> `StratavaFile::key_tail_offsets`) and at first resident
// keyed use. That build was guarded by `try_reserve` alone, so a file with `N`
// distinct keys forced an `N`-entry resident map whatever `keyed_tail` said and
// the ceiling only refused further growth within the session.
mod unlimited {
    stratava::stratava_format! {
        pub format GeneratedTailUnlimitedFormat {
            magic: b"GENBGT";
            version: 1;
            index: keyed_offset_chain;
            blocks {
                variable Tracked(id = 41, key = [id]) {
                    id: u64,
                    value: u32,
                }
            }
        }
    }

    stratava::stratava_format! {
        pub format ResidentTailUnlimitedFormat {
            magic: b"RESBGT";
            version: 1;
            index: keyed_offset_chain;
            blocks {
                variable Budgeted(id = 31, key = [id]) {
                    id: u64,
                    value: u32,
                }
            }
        }
    }
}

/// Number of distinct keys the unlimited twins author. Every budgeted format in
/// this file has a ceiling far below what a map this size costs.
const OVER_BUDGET_KEYS: u64 = 8;

// Cache budgets select spill boundaries; owned result limits remain separate.
#[test]
fn generated_keyed_writer_opens_above_the_tail_cache_budget() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generated-tail-build.stratava");
    {
        let mut writer = unlimited::GeneratedTailUnlimitedFormat::create_writer(&path)?;
        for id in 0..OVER_BUDGET_KEYS {
            writer.push_tracked(&unlimited::Tracked {
                id,
                value: id as u32,
            })?;
        }
        writer.flush()?;
    }
    let mut writer = GeneratedTailBudgetFormat::open_writer(&path)?;
    writer.push_tracked(&Tracked {
        id: 100,
        value: 100,
    })?;
    writer.flush()?;
    drop(writer);
    let file = unlimited::GeneratedTailUnlimitedFormat::open(&path)?;
    assert_eq!(
        file.blocks::<unlimited::Tracked>()?.iter().count() as u64,
        OVER_BUDGET_KEYS + 1
    );
    Ok(())
}

// Cache budgets select spill boundaries; owned result limits remain separate.
#[test]
fn owned_tail_map_is_limited_but_writer_tail_cache_can_spill() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resident-tail-build.stratava");
    {
        let mut file = unlimited::ResidentTailUnlimitedFormat::create(&path)?;
        for id in 0..OVER_BUDGET_KEYS {
            file.push_keyed(&unlimited::Budgeted {
                id,
                value: id as u32,
            })?;
        }
        file.flush()?;
    }
    let mut file = ResidentTailBudgetFormat::open(&path)?;
    assert!(matches!(
        file.key_tail_offsets::<Budgeted>(),
        Err(Error::LimitExceeded {
            resource: "keyed tail bytes",
            ..
        })
    ));
    file.push_keyed(&Budgeted {
        id: 100,
        value: 100,
    })?;
    file.flush()?;
    drop(file);
    let file = unlimited::ResidentTailUnlimitedFormat::open(&path)?;
    assert_eq!(
        file.blocks::<unlimited::Budgeted>()?.iter().count() as u64,
        OVER_BUDGET_KEYS + 1
    );
    Ok(())
}

// Cache budgets select spill boundaries; owned result limits remain separate.
#[test]
fn a_small_reopened_tail_cache_can_grow_beyond_its_budget() -> stratava::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resident-tail-within.stratava");
    {
        let mut file = unlimited::ResidentTailUnlimitedFormat::create(&path)?;
        file.push_keyed(&unlimited::Budgeted { id: 1, value: 1 })?;
        file.flush()?;
    }
    let mut file = ResidentTailBudgetFormat::open(&path)?;
    for id in 0..64 {
        file.push_keyed(&Budgeted { id, value: 2 })?;
    }
    file.flush()?;
    drop(file);
    let file = unlimited::ResidentTailUnlimitedFormat::open(&path)?;
    assert_eq!(file.blocks::<unlimited::Budgeted>()?.iter().count(), 65);
    Ok(())
}

/// The generated `_into` twins exist on the inherent route **and** the trait
/// route, for keyed and unkeyed blocks alike.
///
/// The survey that prompted this named the failure mode exactly: each block
/// accessor is emitted three times — inherent impl, trait declaration, trait
/// impl — so a method added to two of the three compiles and is silently
/// missing from the third. One function now emits all three; this is the test
/// that says so from the outside.
///
/// It is a compile-time assertion wearing a `#[test]`: every call below is
/// resolved through a route that would not exist if the emission had drifted.
/// `ResidentChainFormat` has both an unkeyed block (`Marker`) and a keyed one
/// (`Item`), so both branches of the generator are covered.
#[test]
fn the_generated_into_twins_exist_on_both_routes() -> stratava::Result<()> {
    use stratava::RecordIndexEntry;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("into-twins.stratava");
    {
        let mut writer = ResidentChainFormat::create(&path)?;
        writer.push(&Marker { tag: 7 })?;
        // Keyed blocks on a keyed-chaining format must go through the
        // maintaining path; `push` refuses them (API2-05, asserted above).
        writer.push_keyed(&Item { id: 1, value: 10 })?;
        writer.push_keyed(&Item { id: 2, value: 20 })?;
        writer.flush()?;
    }

    let reader = ResidentChainFormat::open_reader(&path)?;

    // --- inherent route ---
    let mut entries: Vec<RecordIndexEntry> = Vec::new();
    let mut markers: Vec<Marker> = Vec::new();
    reader.markers_entries_into(&mut entries)?;
    reader.markers_decoded_into(&mut markers)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(markers, vec![Marker { tag: 7 }]);

    let mut item_entries: Vec<RecordIndexEntry> = Vec::new();
    let mut by_key = std::collections::HashMap::new();
    reader.items_into(&mut item_entries, &mut by_key)?;
    assert_eq!(item_entries.len(), 2);
    assert_eq!(by_key.len(), 2);

    let mut items: Vec<Item> = Vec::new();
    reader.items_decoded_into(&mut items)?;
    assert_eq!(items.len(), 2);

    // --- trait route: same calls, resolved through the generated trait ---
    fn through_the_trait<R: ResidentChainFormatRead>(
        reader: &R,
    ) -> stratava::Result<(usize, usize)> {
        let mut entries: Vec<RecordIndexEntry> = Vec::new();
        let mut markers: Vec<Marker> = Vec::new();
        reader.markers_entries_into(&mut entries)?;
        reader.markers_decoded_into(&mut markers)?;

        let mut item_entries: Vec<RecordIndexEntry> = Vec::new();
        let mut by_key = std::collections::HashMap::new();
        reader.items_into(&mut item_entries, &mut by_key)?;
        let mut items: Vec<Item> = Vec::new();
        reader.items_decoded_into(&mut items)?;
        Ok((markers.len() + entries.len(), items.len() + by_key.len()))
    }
    assert_eq!(through_the_trait(&reader)?, (2, 4));

    // Reuse is the point: filling an already-populated buffer clears it first
    // rather than appending, so a loop over many files does not grow without
    // bound.
    reader.items_decoded_into(&mut items)?;
    assert_eq!(items.len(), 2, "`_into` must clear before it fills");
    Ok(())
}
