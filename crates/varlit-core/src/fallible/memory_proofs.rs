//! Additional instrumentation of actual helpers, without an allocator model on
//! the success path. These scalar Box cases do not establish arbitrary-T or Arc
//! memory validity. The original refusal/cleanup proofs remain independent.
use super::*;

#[kani::proof]
#[kani::unwind(5)]
fn initialized_scalar_box_and_clone() {
    let value: u32 = kani::any();
    let owner = try_box(value).unwrap();
    let copy = owner.try_clone().unwrap();
    assert_eq!(*owner, value);
    assert_eq!(*copy, value);
    assert!(!std::ptr::eq(&*owner, &*copy));
    drop(copy);
    drop(owner);
}

unsafe fn refuse(_: std::alloc::Layout) -> *mut u8 {
    std::ptr::null_mut()
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, refuse)]
fn refused_scalar_box_does_not_initialize() {
    let mut invoked = false;
    let result = try_box_with(|| {
        invoked = true;
        Ok(kani::any::<u32>())
    });
    assert!(!invoked);
    assert!(matches!(result, Err(Error::AllocationFailed { .. })));
    // The caller receives a heap-free allocation error. The Error drop graph
    // is outside this harness; native/Miri tests exercise real error drops.
    std::mem::forget(result);
}
