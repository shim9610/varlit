//! Contract proofs for the actual allocation/progress decisions.
use super::*;
#[kani::proof_for_contract(output_capacity)]
fn reservation_contract_is_total_and_exact() {
    let _ = output_capacity(kani::any(), kani::any(), kani::any());
}
#[kani::proof_for_contract(block_extent)]
fn block_contract_is_total_and_exact() {
    let _ = block_extent(kani::any(), kani::any(), kani::any());
}
#[kani::proof_for_contract(stream_progress)]
fn streaming_contract_is_total_and_exact() {
    let _ = stream_progress(
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
    );
}
