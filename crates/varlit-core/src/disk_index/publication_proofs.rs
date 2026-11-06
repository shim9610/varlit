//! Production metadata validators; filesystem ordering is tested separately.
use super::*;

#[kani::proof]
#[kani::unwind(26)]
fn wire_frontier_decode_enforces_sequence_exhaustion() {
    let bytes: [u8; 24] = kani::any();
    let exhausted: bool = kani::any();
    let sequence = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let result = decode_frontier(&bytes, exhausted);
    assert_eq!(result.is_ok(), !exhausted || sequence == 0);
    if let Ok(frontier) = &result {
        assert_eq!(
            frontier.eof,
            u64::from_le_bytes(bytes[..8].try_into().unwrap())
        );
        assert_eq!(
            frontier.record_count,
            u64::from_le_bytes(bytes[8..16].try_into().unwrap())
        );
        assert_eq!(
            frontier.next_sequence,
            if exhausted { None } else { Some(sequence) }
        );
    }
    std::mem::forget(result);
}

fn frontier() -> DiskIndexFrontier {
    DiskIndexFrontier::new(kani::any(), kani::any(), kani::any())
}

fn metadata() -> DiskIndexMetadata {
    DiskIndexMetadata {
        identity: DiskIndexIdentity {
            schema_hash: kani::any(),
            primary_fingerprint: kani::any(),
        },
        primary_generation: DiskIndexPrimaryGeneration {
            len: kani::any(),
            digest: DiskIndexDigest(kani::any()),
        },
        mode: if kani::any() {
            DiskIndexMode::StateOnly
        } else {
            DiskIndexMode::DiskPlan(DiskIndexDigest(kani::any()))
        },
        state: if kani::any() {
            DiskIndexState::Clean
        } else {
            DiskIndexState::Dirty
        },
        generation: kani::any(),
        committed: frontier(),
        working: frontier(),
        checkpoint: if kani::any() {
            Some(DiskIndexCheckpoint {
                checkpoint_offset: kani::any(),
                base: frontier(),
            })
        } else {
            None
        },
        tail_limit: kani::any(),
        committed_tail_count: kani::any(),
        working_tail_count: kani::any(),
        committed_tail_digest: DiskIndexDigest(kani::any()),
        working_tail_digest: DiskIndexDigest(kani::any()),
    }
}

fn accepted(result: DiskIndexResult<()>) -> bool {
    let ok = result.is_ok();
    // These validator errors own no allocation. Avoid modeling destruction of
    // unrelated I/O/error variants; all production validation executes above.
    std::mem::forget(result);
    ok
}

#[kani::proof]
#[kani::unwind(35)]
fn accepted_metadata_has_consistent_publication_frontiers() {
    let m = metadata();
    if accepted(validate_metadata(m)) {
        assert!(m.committed.eof <= m.working.eof);
        assert!(m.committed.record_count <= m.working.record_count);
        assert!(m.primary_generation.len <= PRIMARY_GENERATION_WINDOW);
        assert!(m.primary_generation.len <= m.working.eof);
        assert!(m.committed_tail_count <= m.tail_limit);
        assert!(m.working_tail_count <= m.tail_limit);
        match (m.committed.next_sequence, m.working.next_sequence) {
            (None, Some(_)) => panic!("sequence exhaustion was reversed"),
            (Some(a), Some(b)) => assert!(a <= b),
            _ => {}
        }
        match m.state {
            DiskIndexState::Clean => {
                assert!(m.checkpoint.is_none());
                assert!(m.committed == m.working);
                assert!(m.committed_tail_count == m.working_tail_count);
                assert!(m.committed_tail_digest == m.working_tail_digest);
            }
            DiskIndexState::Dirty => {
                let checkpoint = m.checkpoint.unwrap();
                assert!(checkpoint.checkpoint_offset != 0);
                assert!(checkpoint.base == m.committed);
            }
        }
    }
}

#[kani::proof]
#[kani::unwind(35)]
fn valid_clean_frontiers_are_not_rejected() {
    let mut m = metadata();
    m.state = DiskIndexState::Clean;
    if let DiskIndexMode::DiskPlan(digest) = m.mode {
        kani::assume(!digest.is_zero());
    }
    kani::assume(m.primary_generation.len <= PRIMARY_GENERATION_WINDOW);
    kani::assume(m.primary_generation.len <= m.committed.eof);
    m.working = m.committed;
    m.checkpoint = None;
    m.working_tail_count = m.committed_tail_count;
    m.working_tail_digest = m.committed_tail_digest;
    kani::assume(m.committed_tail_count <= m.tail_limit);
    // Full-width counts, offsets, generations and exhausted sequence numbers.
    // A validator that simply rejects all metadata must fail this proof.
    assert!(accepted(validate_metadata(m)));
}

#[kani::proof]
#[kani::unwind(35)]
fn restore_cannot_change_the_confirmed_frontier() {
    let dirty = metadata();
    let clean = metadata();
    if accepted(validate_metadata(dirty))
        && accepted(validate_metadata(clean))
        && accepted(validate_checkpoint_metadata(&dirty, clean))
    {
        assert!(dirty.state == DiskIndexState::Dirty);
        assert!(clean.state == DiskIndexState::Clean);
        assert!(clean.committed == dirty.committed);
        assert!(clean.working == dirty.committed);
        assert!(clean.generation == dirty.generation);
        assert!(clean.identity == dirty.identity);
        assert!(clean.mode == dirty.mode);
        assert!(clean.committed_tail_count == dirty.committed_tail_count);
        assert!(clean.committed_tail_digest == dirty.committed_tail_digest);
    }
}

#[kani::proof]
#[kani::unwind(35)]
fn valid_checkpoint_restore_is_not_rejected() {
    let mut clean = metadata();
    clean.state = DiskIndexState::Clean;
    if let DiskIndexMode::DiskPlan(digest) = clean.mode {
        kani::assume(!digest.is_zero());
    }
    kani::assume(clean.primary_generation.len <= PRIMARY_GENERATION_WINDOW);
    kani::assume(clean.primary_generation.len <= clean.committed.eof);
    clean.working = clean.committed;
    clean.checkpoint = None;
    clean.working_tail_count = clean.committed_tail_count;
    clean.working_tail_digest = clean.committed_tail_digest;
    kani::assume(clean.committed_tail_count <= clean.tail_limit);
    let offset: u64 = kani::any();
    kani::assume(offset != 0);
    let dirty = DiskIndexMetadata {
        state: DiskIndexState::Dirty,
        checkpoint: Some(DiskIndexCheckpoint {
            checkpoint_offset: offset,
            base: clean.committed,
        }),
        ..clean
    };
    assert!(accepted(validate_metadata(clean)));
    assert!(accepted(validate_metadata(dirty)));
    assert!(accepted(validate_checkpoint_metadata(&dirty, clean)));
}
