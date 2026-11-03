//! Actual owner/clone code under a refusing allocator model. Single-threaded.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

static mut CALLS: usize = 0;
static mut DENY_FROM: usize = usize::MAX;
static mut LIVE: isize = 0;

fn denied() -> bool {
    // Harnesses run in separate verifier executions, without threads.
    unsafe {
        let call = CALLS;
        CALLS += 1;
        call >= DENY_FROM
    }
}
fn refuse_from(position: usize) {
    unsafe {
        CALLS = 0;
        DENY_FROM = position;
    }
}
fn live() -> isize {
    unsafe { LIVE }
}
fn calls() -> usize {
    unsafe { CALLS }
}
unsafe fn model_alloc(layout: Layout) -> *mut u8 {
    if denied() {
        return std::ptr::null_mut();
    }
    let result = unsafe { System.alloc(layout) };
    if !result.is_null() {
        unsafe {
            LIVE += layout.size() as isize;
        }
    }
    result
}
unsafe fn model_zeroed(layout: Layout) -> *mut u8 {
    if denied() {
        return std::ptr::null_mut();
    }
    let result = unsafe { System.alloc_zeroed(layout) };
    if !result.is_null() {
        unsafe {
            LIVE += layout.size() as isize;
        }
    }
    result
}
unsafe fn model_realloc(ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    if denied() {
        return std::ptr::null_mut();
    }
    let result = unsafe { System.realloc(ptr, layout, size) };
    if !result.is_null() {
        unsafe {
            LIVE += size as isize - layout.size() as isize;
        }
    }
    result
}
fn model_empty_vec<T>() -> Vec<T> {
    // Kani 0.68's allocation-stub build can misrepresent RawVec::new_in:
    // its trace returns cap=1 for Vec::new(). Use the standard empty Vec
    // invariant through its documented raw-parts constructor. No T exists,
    // capacity is zero, and the pointer is non-null and correctly aligned.
    unsafe { Vec::from_raw_parts(std::ptr::NonNull::<T>::dangling().as_ptr(), 0, 0) }
}
unsafe fn model_global_grow(
    _: &std::alloc::Global,
    _: std::ptr::NonNull<u8>,
    _: Layout,
    _: Layout,
) -> std::result::Result<std::ptr::NonNull<[u8]>, std::alloc::AllocError> {
    // Every clone here reserves into an empty Vec once. Later pushes/copies
    // must fit that reservation; an existing allocation must never grow.
    panic!("fresh clone unexpectedly grew an existing allocation")
}
unsafe fn model_global_dealloc(_: &std::alloc::Global, ptr: std::ptr::NonNull<u8>, layout: Layout) {
    if layout.size() != 0 {
        unsafe { model_dealloc(ptr.as_ptr(), layout) }
    }
}
unsafe fn model_dealloc(ptr: *mut u8, layout: Layout) {
    unsafe {
        LIVE -= layout.size() as isize;
        System.dealloc(ptr, layout);
    }
}

// The contract ends at returning the error to the caller. All errors used
// here carry no heap resource; do not unfold the unrelated recursive Error
// drop graph in every harness. Payload owners are always actually dropped.
fn returned<T>(result: Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            assert!(
                matches!(
                    &error,
                    Error::AllocationFailed { .. } | Error::InvalidMatrixLayout
                ) || matches!(&error, Error::Io(e) if e.kind() == std::io::ErrorKind::OutOfMemory)
            );
            std::mem::forget(error);
            None
        }
    }
}

struct Counted<'a> {
    value: u8,
    drops: &'a Cell<usize>,
}
impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[kani::proof]
#[kani::unwind(8)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn box_value_refusal_or_unique_ownership() {
    let drops = Cell::new(0);
    let value: u8 = kani::any();
    let refuse: bool = kani::any();
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = try_box(Counted {
        value,
        drops: &drops,
    });
    assert_eq!(result.is_ok(), !refuse, "box allocation refusal lost");
    if let Some(owner) = returned(result) {
        assert_eq!(owner.value, value);
        assert_eq!(drops.get(), 0);
        drop(owner);
    }
    assert_eq!(drops.get(), 1, "owned value was not dropped exactly once");
    assert_eq!(live(), 0, "box storage leaked");
}

#[kani::proof]
#[kani::unwind(8)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn box_initializer_runs_only_after_reservation() {
    let refuse: bool = kani::any();
    let init_error: bool = kani::any();
    let value: u8 = kani::any();
    let runs = Cell::new(0);
    let drops = Cell::new(0);
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = try_box_with(|| {
        runs.set(runs.get() + 1);
        if init_error {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(Counted {
            value,
            drops: &drops,
        })
    });
    assert_eq!(
        runs.get(),
        usize::from(!refuse),
        "initializer ran without reserved storage"
    );
    assert_eq!(
        result.is_ok(),
        !refuse && !init_error,
        "box initializer result lost"
    );
    if !refuse && init_error {
        assert!(matches!(result, Err(Error::InvalidMatrixLayout)));
    }
    if let Some(owner) = returned(result) {
        assert_eq!(owner.value, value);
        drop(owner);
    }
    assert_eq!(drops.get(), usize::from(!refuse && !init_error));
    assert_eq!(live(), 0, "failed initializer leaked storage");
}

#[kani::proof]
#[kani::unwind(8)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn uninitialized_box_alignment_and_zero_size() {
    #[repr(align(64))]
    struct Aligned([u8; 3]);
    #[repr(align(128))]
    struct Zero;
    let refuse: bool = kani::any();
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = try_box_uninit::<Aligned>();
    assert_eq!(result.is_ok(), !refuse);
    if let Some(storage) = returned(result) {
        assert_eq!((&*storage as *const _ as usize) % 64, 0);
        let owner = Box::write(storage, Aligned([7; 3]));
        assert_eq!(owner.0, [7; 3]);
    }
    assert_eq!(live(), 0);
    refuse_from(0);
    let zero = try_box_uninit::<Zero>().unwrap();
    assert_eq!((&*zero as *const _ as usize) % 128, 0);
    assert_eq!(calls(), 0, "zero-sized box attempted allocation");
    drop(zero);
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn byte_clone_preserves_source_on_every_refusal() {
    let data: [u8; 4] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 4);
    let source = data[..len].to_vec();
    let baseline = live();
    let fail: usize = kani::any();
    refuse_from(fail);
    let result = source.try_clone();
    assert_eq!(
        result.is_ok(),
        len == 0 || fail > 0,
        "byte clone refusal lost"
    );
    if let Ok(copy) = &result {
        assert_eq!(copy.as_slice(), &data[..len]);
    }
    drop(returned(result));
    assert_eq!(source.as_slice(), &data[..len]);
    assert_eq!(live(), baseline, "byte clone leaked partial output");
    drop(source);
    assert_eq!(live(), 0);
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn string_clone_preserves_source_on_every_refusal() {
    let mut data: [u8; 4] = kani::any();
    for byte in &mut data {
        *byte &= 0x7f;
    }
    let len: usize = kani::any();
    kani::assume(len <= data.len());
    // The bounded formal case uses valid ASCII; native/Miri tests also cover
    // multi-byte UTF-8 and embedded NUL. No arbitrary-size validity claim.
    // SAFETY: Every initialized fixture byte was masked to ASCII above.
    // Input validation is a precondition here, not the clone being verified.
    let source = unsafe { String::from_utf8_unchecked(data[..len].to_vec()) };
    let baseline = live();
    let fail: usize = kani::any();
    refuse_from(fail);
    let result = source.try_clone();
    assert_eq!(
        result.is_ok(),
        len == 0 || fail > 0,
        "string clone refusal lost"
    );
    if let Ok(copy) = &result {
        assert_eq!(copy.as_bytes(), &data[..len]);
    }
    drop(returned(result));
    assert_eq!(source.as_bytes(), &data[..len]);
    assert_eq!(live(), baseline, "string clone storage leaked");
    drop(source);
    assert_eq!(live(), 0);
}

fn nested_clone_case<const N: usize>() {
    struct Item {
        id: u8,
        fail_item: u8,
        cloned: bool,
    }
    impl TryClone for Item {
        fn try_clone(&self) -> Result<Self> {
            if self.id == self.fail_item {
                return Err(Error::InvalidMatrixLayout);
            }
            unsafe {
                CREATED += 1;
            }
            Ok(Self {
                cloned: true,
                ..*self
            })
        }
    }
    impl Drop for Item {
        fn drop(&mut self) {
            if self.cloned {
                unsafe {
                    DESTROYED += 1;
                }
            }
        }
    }
    let len = N;
    let fail_item: u8 = kani::any();
    static mut CREATED: usize = 0;
    static mut DESTROYED: usize = 0;
    // Fixed fixture storage avoids symbolically proving Vec growth while
    // preparing the input. Vec<T>::try_clone delegates to this exact method.
    let item = |id| {
        Box::new(Item {
            id,
            fail_item,
            cloned: false,
        })
    };
    let source = [item(0), item(1), item(2)];
    let baseline = live();
    let fail: usize = kani::any();
    refuse_from(fail);
    let result = Box::<Item>::try_clone_slice(&source[..len]);
    assert_eq!(
        result.is_ok(),
        (len == 0 || fail > len) && usize::from(fail_item) >= len,
        "nested clone success/refusal contract lost"
    );
    if let Some(copy) = returned(result) {
        assert_eq!(copy.len(), len);
        for (i, item) in copy.iter().enumerate() {
            assert_eq!(item.id, i as u8);
        }
        drop(copy);
    }
    assert_eq!(
        unsafe { CREATED },
        unsafe { DESTROYED },
        "partial clone payload leaked"
    );
    assert_eq!(live(), baseline, "partial clone storage leaked");
    for (i, item) in source.iter().enumerate() {
        assert_eq!(item.id, i as u8);
        assert!(!item.cloned);
    }
    drop(source);
    assert_eq!(live(), 0);
}

#[kani::proof]
#[kani::unwind(4)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn nested_clone_0_releases_every_partial_owner() {
    nested_clone_case::<0>();
}

#[kani::proof]
#[kani::unwind(4)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn nested_clone_1_releases_every_partial_owner() {
    nested_clone_case::<1>();
}

#[kani::proof]
#[kani::unwind(4)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn nested_clone_2_releases_every_partial_owner() {
    nested_clone_case::<2>();
}

#[kani::proof]
#[kani::unwind(4)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn nested_clone_3_releases_every_partial_owner() {
    nested_clone_case::<3>();
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn shared_initializer_refusal_and_last_owner_drop() {
    let refuse: bool = kani::any();
    let init_error: bool = kani::any();
    let runs = Cell::new(0);
    let drops = Cell::new(0);
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = try_arc_with(|| {
        runs.set(runs.get() + 1);
        if init_error {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(Counted {
            value: 17,
            drops: &drops,
        })
    });
    assert_eq!(
        runs.get(),
        usize::from(!refuse),
        "shared initializer ran without reserved storage"
    );
    assert_eq!(result.is_ok(), !refuse && !init_error);
    if !refuse && init_error {
        assert!(matches!(&result, Err(Error::InvalidMatrixLayout)));
    }
    if let Some(owner) = returned(result) {
        assert_eq!(owner.value, 17);
        refuse_from(0);
        let copy = owner.try_clone().unwrap();
        assert!(Arc::ptr_eq(&owner, &copy));
        assert_eq!(calls(), 0, "shared clone attempted allocation");
        drop(owner);
        assert_eq!(drops.get(), 0);
        drop(copy);
        assert_eq!(drops.get(), 1);
    }
    assert_eq!(live(), 0, "shared owner storage leaked");
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn copy_on_write_refusal_preserves_shared_source() {
    let mut owner = try_arc(vec![1u8, 2, 3]).unwrap();
    let shared: bool = kani::any();
    let retained = shared.then(|| owner.clone());
    let baseline = live();
    let fail: usize = kani::any();
    refuse_from(fail);
    let result = try_make_mut(&mut owner);
    let succeeded = result.is_ok();
    if !shared {
        assert_eq!(calls(), 0, "unique COW attempted allocation");
    }
    assert_eq!(succeeded, !shared || fail >= 2, "COW refusal outcome lost");
    match returned(result) {
        Some(value) => {
            assert_eq!(value.as_slice(), &[1, 2, 3], "COW copied payload changed");
            value[0] = 9;
        }
        None => {
            assert!(Arc::ptr_eq(&owner, retained.as_ref().unwrap()));
            assert_eq!(owner.as_slice(), &[1, 2, 3]);
            assert_eq!(live(), baseline, "failed COW leaked partial output");
        }
    }
    if succeeded {
        assert_eq!(owner.as_slice(), &[9, 2, 3]);
    }
    if let Some(old) = retained {
        assert_eq!(old.as_slice(), &[1, 2, 3]);
    }
    drop(owner);
    assert_eq!(live(), 0);
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn full_width_clone_capacity_refuses_without_abort() {
    let len: usize = kani::any();
    refuse_from(0);
    let result = reserved_vec_for::<u16>(len, "vector clone");
    assert_eq!(result.is_ok(), len == 0, "capacity refusal lost");
    drop(returned(result));
    assert_eq!(live(), 0);
    assert_eq!(
        calls(),
        usize::from(len != 0 && len <= isize::MAX as usize / 2)
    );
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
fn shared_vector_initialization_preserves_payload() {
    let value = try_arc(vec![1u8, 2, 3]).unwrap();
    assert_eq!(value.len(), 3, "shared vector length");
    assert_eq!(value.capacity(), 3, "shared vector capacity");
    assert_eq!(value.as_slice(), &[1, 2, 3], "shared vector data");
    drop(value);
    assert_eq!(live(), 0, "shared vector allocation balance");
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn boxed_clone_reserves_before_payload_callback() {
    struct Payload<'a> {
        calls: &'a Cell<usize>,
        fails: bool,
    }
    impl TryClone for Payload<'_> {
        fn try_clone(&self) -> Result<Self> {
            self.calls.set(self.calls.get() + 1);
            if self.fails {
                Err(Error::InvalidMatrixLayout)
            } else {
                Ok(Self { ..*self })
            }
        }
    }
    let runs = Cell::new(0);
    let fails: bool = kani::any();
    let source = try_box(Payload {
        calls: &runs,
        fails,
    })
    .unwrap();
    let baseline = live();
    let refuse: bool = kani::any();
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = source.try_clone();
    assert_eq!(result.is_ok(), !refuse && !fails);
    if !refuse && fails {
        assert!(matches!(&result, Err(Error::InvalidMatrixLayout)));
    }
    assert_eq!(
        runs.get(),
        usize::from(!refuse),
        "boxed clone callback ran before reservation"
    );
    drop(returned(result));
    assert_eq!(live(), baseline, "boxed clone leaked partial storage");
    assert_eq!(source.fails, fails);
    drop(source);
    assert_eq!(live(), 0);
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
fn empty_vector_model_obeys_its_standard_contract() {
    #[repr(align(64))]
    struct Aligned {
        _bytes: [u8; 3],
    }
    #[repr(align(128))]
    struct Zero;
    refuse_from(0);
    let bytes = model_empty_vec::<u8>();
    let aligned = model_empty_vec::<Aligned>();
    let zero = model_empty_vec::<Zero>();
    assert!(bytes.is_empty() && aligned.is_empty() && zero.is_empty());
    assert_eq!(bytes.capacity(), 0);
    assert_eq!(aligned.capacity(), 0);
    assert_eq!(zero.capacity(), usize::MAX);
    assert_eq!(aligned.as_ptr() as usize % 64, 0);
    assert_eq!(zero.as_ptr() as usize % 128, 0);
    drop((bytes, aligned, zero));
    assert_eq!(calls(), 0);
    assert_eq!(live(), 0);
}

#[kani::proof]
#[kani::unwind(5)]
#[kani::stub(std::alloc::alloc, model_alloc)]
#[kani::stub(std::vec::Vec::<u8>::new, model_empty_vec)]
#[kani::stub(std::alloc::alloc_zeroed, model_zeroed)]
#[kani::stub(std::alloc::realloc, model_realloc)]
#[kani::stub(std::alloc::dealloc, model_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::deallocate, model_global_dealloc)]
#[kani::stub(<std::alloc::Global as std::alloc::Allocator>::grow, model_global_grow)]
fn shared_value_refusal_or_unique_ownership() {
    let drops = Cell::new(0);
    let value: u8 = kani::any();
    let refuse: bool = kani::any();
    refuse_from(if refuse { 0 } else { usize::MAX });
    let result = try_arc(Counted {
        value,
        drops: &drops,
    });
    assert_eq!(
        result.is_ok(),
        !refuse,
        "shared value allocation refusal lost"
    );
    if let Some(owner) = returned(result.map_err(Error::from)) {
        assert_eq!(owner.value, value);
        assert_eq!(drops.get(), 0);
        drop(owner);
    }
    assert_eq!(drops.get(), 1, "shared input was not dropped exactly once");
    assert_eq!(live(), 0);
}
