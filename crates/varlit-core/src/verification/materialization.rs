use super::MaterializationBudget;

#[kani::proof]
fn cumulative_materialization_never_wraps_or_replenishes() {
    let limit: u64 = kani::any();
    let remaining: u64 = kani::any();
    let request: u64 = kani::any();
    kani::assume(remaining <= limit); // invariant of constructor/reset/consume
    let mut budget = MaterializationBudget { limit, remaining };
    let result = budget.consume(request);
    assert_eq!(
        result.is_ok(),
        request <= remaining,
        "budget accepted excessive materialization"
    );
    assert_eq!(
        budget.remaining,
        if request <= remaining {
            remaining - request
        } else {
            remaining
        },
        "failed charge changed budget"
    );
    assert!(budget.remaining <= limit);
    std::mem::forget(result);
    budget.reset();
    assert_eq!(budget.remaining, limit);
}

/// Retained sibling values and a retry share one real MaterializationBudget.
/// No reset/reconstruction is permitted between the two calls, even on error.
#[kani::proof]
#[kani::unwind(10)]
fn decode_retry_contract_shares_the_remaining_budget_after_errors() {
    use crate::codec::caller_proofs::{TwoCharges, two_charge_oracle};
    let limit: u64 = kani::any();
    let first: u64 = kani::any();
    let second: u64 = kani::any();
    let len: u8 = kani::any();
    let flag: u8 = kani::any();
    kani::assume(len <= 18);
    let mut bytes = [0u8; 18];
    bytes[..8].copy_from_slice(&first.to_le_bytes());
    bytes[8..16].copy_from_slice(&second.to_le_bytes());
    bytes[16] = flag;
    let mut budget = MaterializationBudget {
        limit,
        remaining: limit,
    };
    for _ in 0..2 {
        let before = budget.remaining;
        let result = budget.decode::<TwoCharges>(&bytes[..usize::from(len)], crate::Endian::Little);
        assert_eq!(
            budget.remaining,
            two_charge_oracle(before, first, second, len),
            "decode retry refunded or reset shared budget"
        );
        assert!(budget.remaining <= before);
        assert_eq!(budget.limit, limit);
        assert_eq!(
            result.is_ok(),
            len == 17 && first <= before && second <= before - first && flag == 0
        );
        std::mem::forget(result);
    }
}
