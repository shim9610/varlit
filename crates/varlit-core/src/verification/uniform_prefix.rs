//! Bounded state-machine proof: four observed rows, 16-bit offsets, arbitrary
//! truncation. This does not prove arbitrary-length sequences or all u64 offsets.
use super::UniformPrefix;

#[kani::proof]
#[kani::unwind(5)]
fn observed_prefix_never_invents_membership() {
    let offsets: [u16; 4] = kani::any();
    let wanted = u64::from(kani::any::<u16>());
    let keep: usize = kani::any();
    kani::assume(keep <= offsets.len());
    let mut proof = None;
    for (row, offset) in offsets.iter().enumerate() {
        UniformPrefix::observe(&mut proof, row, u64::from(*offset));
    }
    let proof = proof.as_mut().unwrap();
    if proof.contains(wanted) {
        assert!(offsets.iter().any(|&offset| u64::from(offset) == wanted));
    }
    proof.truncate(keep);
    if proof.contains(wanted) {
        assert!(
            offsets[..keep]
                .iter()
                .any(|&offset| u64::from(offset) == wanted)
        );
    }
}

// Inductive representation invariant. Rows are internal validated directory
// indices (strictly below usize::MAX), not assumed-valid serialized offsets.
fn invariant(p: &UniformPrefix) -> bool {
    p.first_row as u128 + p.count as u128 <= usize::MAX as u128
        && (p.count <= 1 || p.step != 0)
        && p.start <= p.last
        && (p.count > 1 || p.start == p.last)
}

fn arbitrary_prefix() -> UniformPrefix {
    let p = UniformPrefix {
        first_row: kani::any(),
        count: kani::any(),
        start: kani::any(),
        step: kani::any(),
        last: kani::any(),
        closed: kani::any(),
    };
    kani::assume(invariant(&p));
    p
}

#[kani::proof]
fn full_width_prefix_initialization_establishes_invariant() {
    let row: usize = kani::any();
    kani::assume(row < usize::MAX);
    let offset: u64 = kani::any();
    let mut state = None;
    UniformPrefix::observe(&mut state, row, offset);
    let p = state.unwrap();
    assert!(invariant(&p));
    assert!(p.contains(offset));
    assert_eq!(p.count, 1);
}

#[kani::proof]
fn full_width_prefix_observation_preserves_invariant() {
    let mut p = arbitrary_prefix();
    let row: usize = kani::any();
    kani::assume(row < usize::MAX);
    let offset: u64 = kani::any();
    let old_count = p.count;
    let old_last = p.last;
    let old_start = p.start;
    let closed = p.closed;
    p.push(row, offset);
    assert!(invariant(&p), "prefix representation invariant broken");
    if old_count != 0 {
        assert!(p.count == old_count || p.count == old_count + 1);
        assert_eq!(p.start, old_start);
        if p.count != old_count {
            assert!(
                !closed && p.last == offset && offset > old_last,
                "prefix added an unobserved boundary"
            );
        } else {
            assert_eq!(p.last, old_last);
        }
    }
}

#[kani::proof]
fn full_width_bounded_prefix_truncation_only_removes_rows() {
    // Full-width offsets/strides and internal row indices, up to four observed
    // rows. General count*stride induction is NOT claimed by this harness.
    let small_count: u8 = kani::any();
    kani::assume(small_count <= 4);
    let count = small_count as usize;
    let start: u64 = kani::any();
    let step: u64 = kani::any();
    let first_row: usize = kani::any();
    let last = start as u128 + step as u128 * count.saturating_sub(1) as u128;
    kani::assume(last <= u64::MAX as u128);
    kani::assume(first_row as u128 + count as u128 <= usize::MAX as u128);
    kani::assume(count <= 1 || step != 0);
    let mut p = UniformPrefix {
        first_row,
        count,
        start,
        step,
        last: last as u64,
        closed: kani::any(),
    };
    let len: usize = kani::any();
    p.truncate(len);
    assert!(invariant(&p), "truncation broke representation invariant");
    assert_eq!(p.count, count.min(len.saturating_sub(first_row)));
    assert_eq!(p.start, start);
    assert_eq!(p.step, step);
    assert!(p.last as u128 <= last, "truncation extended the prefix");
}
