//! The hole cache: a hole it hands out is one the request may take, and it
//! hands one out whenever it holds such a hole.
use super::{Hole, Holes};

const CAPACITY: usize = 3;

#[kani::proof]
#[kani::unwind(6)]
fn a_taken_hole_is_one_the_request_may_take() {
    let mut holes = Holes::with_capacity(CAPACITY).expect("tiny");
    let mut held = [Hole::default(); CAPACITY + 1];
    for slot in &mut held {
        *slot = Hole {
            region: kani::any(),
            at: kani::any(),
            len: kani::any(),
            generation: kani::any(),
        };
        holes.add(*slot);
    }
    assert!(holes.len <= CAPACITY);
    let len: u64 = kani::any();
    let avoid: u64 = kani::any();
    let in_place: u64 = kani::any();
    let before = holes.len;
    let fits = |hole: &Hole| hole.len >= len && hole.region != avoid && hole.generation <= in_place;
    let any_fits = holes.entries[..holes.len].iter().any(fits);
    match holes.take_fitting(len, avoid, in_place) {
        Some(hole) => {
            assert!(fits(&hole));
            assert_eq!(holes.len, before - 1);
        }
        None => {
            assert!(!any_fits);
            assert_eq!(holes.len, before);
        }
    }
}
