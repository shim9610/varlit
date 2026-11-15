//! The contract is attached to the real helper; inputs retain full width.
use super::*;
#[kani::proof_for_contract(table_extent)]
fn table_extent_contract_is_total_and_exact() {
    let _ = table_extent(kani::any(), kani::any());
}
