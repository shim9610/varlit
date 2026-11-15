//! State-frame and result contracts for the real traversal guards.
use super::*;
#[kani::proof_for_contract(take_visit)]
fn fuel_contract_preserves_failure_and_decreases_success() {
    let mut fuel = kani::any();
    let _ = take_visit(&mut fuel);
}
#[kani::proof_for_contract(height_matches)]
fn height_contract_is_total_and_exact() {
    let _ = height_matches(kani::any(), kani::any());
}
