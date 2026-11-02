// Calibration of the experimental instrumentation. Negative cases are run
// separately and must fail for the specified property, never a compiler crash.
#[kani::proof]
fn allocated_value() {
    let value: u32 = kani::any();
    let owner = Box::new(value);
    assert_eq!(*owner, value);
}

#[kani::proof]
fn null_wide_pointer() {
    let pointer = unsafe { std::mem::transmute::<(usize, usize), std::ptr::NonNull<[u8]>>((0, 3)) };
    assert_eq!(pointer.len(), 3);
}

#[kani::proof]
fn initialized_reference() {
    unsafe {
        let layout = std::alloc::Layout::new::<u32>();
        let pointer = std::alloc::alloc(layout);
        *pointer = 7;
        // Only the referenced byte must be initialized, not all four bytes.
        let reference: &u8 = std::mem::transmute(pointer as *const u32);
        assert_eq!(*reference, 7);
        std::alloc::dealloc(pointer, layout);
    }
}

#[kani::proof]
fn uninitialized_reference() {
    unsafe {
        let pointer = std::alloc::alloc(std::alloc::Layout::new::<u32>());
        let _reference: &u8 = std::mem::transmute(pointer as *const u32);
    }
}

#[kani::proof]
fn slice_index_with_unreachable_indirect_panic() {
    let values: [u32; 4] = kani::any();
    assert_eq!((&values[..])[1..3][0], values[1]);
}

fn double(value: u32) -> u32 {
    value.wrapping_mul(2)
}

#[kani::proof]
fn reachable_indirect_call() {
    let callback: fn(u32) -> u32 = double;
    let value: u32 = kani::any();
    assert_eq!(callback(value), value.wrapping_mul(2));
}
