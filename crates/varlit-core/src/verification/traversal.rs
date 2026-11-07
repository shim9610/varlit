use super::take_visit;

#[kani::proof]
fn every_node_visit_consumes_finite_fuel() {
    let before: u64 = kani::any();
    let mut fuel = before;
    let result = take_visit(&mut fuel);
    assert_eq!(
        result.is_some(),
        before != 0,
        "exhausted traversal accepted"
    );
    if result.is_some() {
        assert!(fuel < before, "traversal did not decrease fuel");
        assert_eq!(fuel, before - 1);
    } else {
        assert_eq!(fuel, 0);
    }
}

#[kani::proof]
fn accepted_child_height_strictly_decreases() {
    let parent: u8 = kani::any();
    let child: u8 = kani::any();
    if parent != 0 && super::height_matches(Some(parent - 1), child) {
        assert!(child < parent, "child height did not decrease");
    }
    assert!(super::height_matches(None, child));
    assert!(super::height_matches(Some(child), child));
}
