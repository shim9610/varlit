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
