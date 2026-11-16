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
