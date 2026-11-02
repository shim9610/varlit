//! Explicit allocation-failure propagation for owned values.
use crate::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::path::PathBuf;

/// Clone a value without aborting when a requested allocation is refused.
///
/// Implementations must use fallible allocation for every newly owned field and
/// release partially constructed output on error. The source stays unchanged.
/// A successful result has the same value/ownership semantics as `Clone`.
/// Custom implementations, hash/equality callbacks and destructors must uphold
/// this contract themselves. This is not an OS OOM-kill guarantee.
///
/// There is deliberately no blanket `T: Clone` implementation: wrapping
/// `clone()` in `Ok` would still permit an allocation abort. Existing `Clone`
/// implementations remain available for source compatibility, outside this
/// contract. BTreeMap has no implementation until its allocations are fallible.
pub trait TryClone: Sized {
    fn try_clone(&self) -> Result<Self>;

    /// Clone a slice, releasing any partial result on failure. The default
    /// clones each element fallibly; Copy implementations retain bulk copying.
    fn try_clone_slice(source: &[Self]) -> Result<Vec<Self>> {
        let mut output = reserved_vec(source.len())?;
        for value in source {
            output.push(value.try_clone()?);
        }
        Ok(output)
    }
}

fn allocation(resource: &'static str, requested: usize) -> Error {
    Error::AllocationFailed {
        resource,
        requested: requested as u64,
    }
}
macro_rules! copy_clone {
    ($($ty:ty),* $(,)?) => {$(impl TryClone for $ty {
        #[inline]
        fn try_clone(&self) -> Result<Self> { Ok(*self) }
        fn try_clone_slice(source: &[Self]) -> Result<Vec<Self>> { copy_slice(source) }
    })*};
}
copy_clone!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64
);
impl<T: ?Sized> TryClone for &T {
    fn try_clone(&self) -> Result<Self> {
        Ok(*self)
    }
}
impl<T: Copy, const N: usize> TryClone for [T; N] {
    fn try_clone(&self) -> Result<Self> {
        Ok(*self)
    }
    fn try_clone_slice(source: &[Self]) -> Result<Vec<Self>> {
        copy_slice(source)
    }
}
impl<T: TryClone> TryClone for Option<T> {
    fn try_clone(&self) -> Result<Self> {
        self.as_ref().map(TryClone::try_clone).transpose()
    }
}
impl<A: TryClone, B: TryClone> TryClone for (A, B) {
    fn try_clone(&self) -> Result<Self> {
        Ok((self.0.try_clone()?, self.1.try_clone()?))
    }
}
impl<T: TryClone> TryClone for Vec<T> {
    fn try_clone(&self) -> Result<Self> {
        T::try_clone_slice(self)
    }
}
fn reserved_vec<T>(len: usize) -> Result<Vec<T>> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(len)
        .map_err(|_| allocation("vector clone", len.saturating_mul(size_of::<T>())))?;
    Ok(output)
}
fn copy_slice<T: Copy>(source: &[T]) -> Result<Vec<T>> {
    let mut output = reserved_vec(source.len())?;
    output.extend_from_slice(source);
    Ok(output)
}

impl TryClone for String {
    fn try_clone(&self) -> Result<Self> {
        let mut output = String::new();
        output
            .try_reserve_exact(self.len())
            .map_err(|_| allocation("string clone", self.len()))?;
        output.push_str(self);
        Ok(output)
    }
}
impl TryClone for PathBuf {
    fn try_clone(&self) -> Result<Self> {
        let mut output = std::ffi::OsString::new();
        output
            .try_reserve_exact(self.as_os_str().len())
            .map_err(|_| allocation("path clone", self.as_os_str().len()))?;
        output.push(self.as_os_str());
        Ok(output.into())
    }
}
impl<T: TryClone> TryClone for VecDeque<T> {
    fn try_clone(&self) -> Result<Self> {
        let mut output = VecDeque::new();
        output
            .try_reserve_exact(self.len())
            .map_err(|_| allocation("deque clone", self.len().saturating_mul(size_of::<T>())))?;
        for value in self {
            output.push_back(value.try_clone()?);
        }
        Ok(output)
    }
}
impl<K: Eq + Hash + TryClone, V: TryClone> TryClone for HashMap<K, V> {
    fn try_clone(&self) -> Result<Self> {
        // RandomState::clone copies the seed; it does not allocate.
        let mut output = HashMap::with_hasher(self.hasher().clone());
        output
            .try_reserve(self.len())
            .map_err(|_| allocation("map clone", self.len().saturating_mul(size_of::<(K, V)>())))?;
        for (key, value) in self {
            output.insert(key.try_clone()?, value.try_clone()?);
        }
        Ok(output)
    }
}
impl<T: ?Sized> TryClone for std::sync::Arc<T> {
    fn try_clone(&self) -> Result<Self> {
        Ok(std::sync::Arc::clone(self))
    }
}

/// Allocate one owned value, reporting refusal instead of invoking the OOM handler.
/// The value is dropped normally on refusal. User-defined destructors are outside
/// the allocator contract. Zero-sized values need no allocation.
pub fn try_box<T>(value: T) -> Result<Box<T>> {
    Ok(Box::write(try_box_uninit()?, value))
}

/// Reserve uninitialized storage without constructing a `T` first.
///
/// The returned box owns only storage; dropping it does not run `T::drop`.
/// Initialize it with `Box::write`, or initialize in place and satisfy the
/// safety contract of `assume_init`. Zero-sized types require no allocation.
/// This avoids a mandatory by-value `T` argument, but does not make arbitrary
/// caller initialization or destructors fallible.
pub fn try_box_uninit<T>() -> Result<Box<std::mem::MaybeUninit<T>>> {
    use std::mem::MaybeUninit;
    let layout = std::alloc::Layout::new::<MaybeUninit<T>>();
    let pointer = if layout.size() == 0 {
        std::ptr::NonNull::<MaybeUninit<T>>::dangling().as_ptr()
    } else {
        // SAFETY: the nonzero layout has T's exact size and alignment.
        let pointer = unsafe { std::alloc::alloc(layout) }.cast::<MaybeUninit<T>>();
        if pointer.is_null() {
            return Err(allocation("boxed value", layout.size()));
        }
        pointer
    };
    // SAFETY: MaybeUninit<T> permits uninitialized storage, including a
    // dangling aligned pointer for ZSTs. Box adopts exactly this global-
    // allocator layout; neither conversion nor dropping it constructs a T.
    Ok(unsafe { Box::from_raw(pointer) })
}

/// Reserve the box before invoking a returning fallible initializer.
///
/// Allocation refusal skips `initialize`. Its error is returned unchanged and
/// releases the reserved storage. Success transfers the initialized value to
/// the box. Callback panics, nontermination and aborting destructors are outside
/// the Result contract. The callback still returns T by value; use
/// `try_box_uninit` for caller-controlled in-place initialization.
pub fn try_box_with<T>(initialize: impl FnOnce() -> Result<T>) -> Result<Box<T>> {
    let storage = try_box_uninit()?;
    Ok(Box::write(storage, initialize()?))
}
impl<T: TryClone> TryClone for Box<T> {
    fn try_clone(&self) -> Result<Self> {
        try_box_with(|| (**self).try_clone())
    }
}

pub(crate) fn boxed_error(
    source: Error,
    fallback: Error,
    build: impl FnOnce(Box<Error>) -> Error,
) -> Error {
    match try_box(source) {
        Ok(source) => build(source),
        Err(_) => fallback,
    }
}

// A path's Display can write several UTF-8 chunks. Grow only with try_reserve;
// constructing a late publication error must not hide another infallible box.
fn try_display_path(path: &std::path::Path) -> Result<String> {
    struct Output(String);
    impl std::fmt::Write for Output {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0
                .try_reserve(text.len())
                .map_err(|_| std::fmt::Error)?;
            self.0.push_str(text);
            Ok(())
        }
    }
    let mut output = Output(String::new());
    std::fmt::write(&mut output, format_args!("{}", path.display()))
        .map_err(|_| allocation("error path", path.as_os_str().len()))?;
    Ok(output.0)
}
pub(crate) fn boxed_path_error(
    source: Error,
    path: &std::path::Path,
    fallback: Error,
    build: impl FnOnce(String, Box<Error>) -> Error,
) -> Error {
    match try_display_path(path).and_then(|path| try_box(source).map(|source| build(path, source)))
    {
        Ok(error) => error,
        Err(_) => fallback,
    }
}

// Kept private: the existing public standard-library Arc API is unchanged.
pub(crate) use triomphe::Arc;
pub(crate) fn try_arc<T>(value: T) -> std::io::Result<Arc<T>> {
    let mut storage = triomphe::UniqueArc::try_new_uninit()
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
    storage.write(value);
    // SAFETY: write initialized the unique allocation exactly once. Conversion
    // transfers that same allocation without copying or allocating again.
    Ok(unsafe { triomphe::UniqueArc::assume_init(storage) }.shareable())
}

// As with try_box_with, initialization is deferred until ownership storage is
// available. UniqueArc keeps the uninitialized allocation exclusively owned.
fn try_arc_with<T>(initialize: impl FnOnce() -> Result<T>) -> Result<Arc<T>> {
    let mut storage = triomphe::UniqueArc::try_new_uninit()
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
    storage.write(initialize()?);
    // SAFETY: the callback returned an initialized T, written exactly once.
    // On callback failure, only MaybeUninit storage is dropped instead.
    Ok(unsafe { triomphe::UniqueArc::assume_init(storage) }.shareable())
}
impl<T: ?Sized> TryClone for Arc<T> {
    fn try_clone(&self) -> Result<Self> {
        Ok(Arc::clone(self))
    }
}
pub(crate) fn try_make_mut<T: TryClone>(value: &mut Arc<T>) -> Result<&mut T> {
    if Arc::get_mut(value).is_none() {
        // Finish both allocations before replacing the original snapshot.
        let replacement = try_arc_with(|| (**value).try_clone())?;
        *value = replacement;
    }
    Ok(Arc::get_mut(value).expect("unique after fallible copy-on-write"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pib_probe::with_allocation_refusal;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn boxed_storage_handles_alignment_zero_size_and_refusal() {
        #[repr(align(256))]
        struct Aligned([u8; 257]);
        #[repr(align(512))]
        struct Zero;
        let value = try_box(Aligned([17; 257])).unwrap();
        assert_eq!((&*value as *const Aligned as usize) % 256, 0);
        assert_eq!(value.0, [17; 257]);
        let zero = with_allocation_refusal(1, || try_box(Zero)).unwrap();
        assert_eq!((&*zero as *const Zero as usize) % 512, 0);
        assert!(with_allocation_refusal(1, || try_box([0u8; 3])).is_err());
    }

    #[test]
    fn arc_refusal_drops_input_once_and_shared_cow_is_transactional() {
        struct Counted<'a>(&'a AtomicUsize);
        impl Drop for Counted<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[repr(align(256))]
        struct Aligned([u8; 257]);
        let aligned = try_arc(Aligned([19; 257])).unwrap();
        assert_eq!((&*aligned as *const Aligned as usize) % 256, 0);
        assert_eq!(aligned.0, [19; 257]);
        let drops = AtomicUsize::new(0);
        let result = with_allocation_refusal(1, || try_arc(Counted(&drops)));
        assert!(matches!(result, Err(e) if e.kind() == std::io::ErrorKind::OutOfMemory));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let value = try_arc(Counted(&drops)).unwrap();
        let copy = with_allocation_refusal(1, || value.try_clone()).unwrap();
        drop(value);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(copy);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        for at in [1, 2] {
            // Vec payload and shared-owner header
            let mut value = try_arc(vec![7u8; 4096]).unwrap();
            let retained = value.clone();
            let result = with_allocation_refusal(at, || try_make_mut(&mut value).map(|v| v[0] = 9));
            assert!(result.is_err());
            assert!(Arc::ptr_eq(&value, &retained));
            assert_eq!(value[0], 7);
            try_make_mut(&mut value).unwrap()[0] = 9;
            assert_eq!(retained[0], 7);
            assert_eq!(value[0], 9);
            assert!(!Arc::ptr_eq(&value, &retained));
            with_allocation_refusal(1, || try_make_mut(&mut value)).unwrap()[1] = 3;
        }
    }

    #[test]
    fn shared_initialization_and_cow_reserve_before_calling_user_code() {
        let runs = AtomicUsize::new(0);
        let refused = with_allocation_refusal(1, || {
            try_arc_with(|| {
                runs.fetch_add(1, Ordering::Relaxed);
                Ok([7u8; 64])
            })
        });
        assert!(
            matches!(refused, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory)
        );
        assert_eq!(runs.load(Ordering::Relaxed), 0);
        assert!(matches!(
            try_arc_with::<u8>(|| Err(Error::InvalidMatrixLayout)),
            Err(Error::InvalidMatrixLayout)
        ));
        assert_eq!(*try_arc_with(|| Ok(7u8)).unwrap(), 7);

        struct Payload<'a>(&'a AtomicUsize);
        impl TryClone for Payload<'_> {
            fn try_clone(&self) -> Result<Self> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(Error::InvalidMatrixLayout)
            }
        }
        let mut owner = try_arc(Payload(&runs)).unwrap();
        let retained = owner.clone();
        assert!(with_allocation_refusal(1, || try_make_mut(&mut owner)).is_err());
        assert_eq!(runs.load(Ordering::Relaxed), 0);
        assert!(Arc::ptr_eq(&owner, &retained));
        assert!(matches!(
            try_make_mut(&mut owner),
            Err(Error::InvalidMatrixLayout)
        ));
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(&owner, &retained));
        drop(retained);
        with_allocation_refusal(1, || try_make_mut(&mut owner)).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn restricted_values_and_padding_survive_owner_initialization_and_refusal() {
        // Miri checks initialized bytes, validity and aliasing here. In
        // particular, padding is not required to be initialized or zeroed.
        #[repr(C, align(64))]
        struct Payload {
            flag: bool,
            nonzero: std::num::NonZeroU32,
            reference: &'static u8,
            // Separate allocations exercise failure after partial cloning.
            #[allow(clippy::vec_box)]
            nested: Vec<Box<bool>>,
        }
        impl TryClone for Payload {
            fn try_clone(&self) -> Result<Self> {
                Ok(Self {
                    flag: self.flag,
                    nonzero: self.nonzero,
                    reference: self.reference,
                    nested: self.nested.try_clone()?,
                })
            }
        }
        for flag in [false, true] {
            let mut owner = try_arc(Payload {
                flag,
                nonzero: std::num::NonZeroU32::new(19).unwrap(),
                reference: &37,
                nested: vec![try_box(flag).unwrap(), try_box(!flag).unwrap()],
            })
            .unwrap();
            let retained = owner.clone();
            // Header, vector, first Box and second Box: every failure point
            // includes real payload/error drops, including partial clones.
            for at in 1..=4 {
                let result = with_allocation_refusal(at, || try_make_mut(&mut owner));
                assert!(result.is_err());
                drop(result);
                assert!(Arc::ptr_eq(&owner, &retained));
                assert_eq!(owner.flag, flag);
                assert_eq!(owner.nonzero.get(), 19);
                assert_eq!(*owner.reference, 37);
                assert_eq!(*owner.nested[0], flag);
                assert_eq!(*owner.nested[1], !flag);
            }
            let changed = try_make_mut(&mut owner).unwrap();
            changed.flag = !flag;
            *changed.nested[0] = !flag;
            assert!(!Arc::ptr_eq(&owner, &retained));
            assert_eq!(retained.flag, flag);
            assert_eq!(*retained.nested[0], flag);
            assert_eq!((&*owner as *const Payload as usize) % 64, 0);
            let boxed = try_box_with(|| retained.try_clone()).unwrap();
            assert!(Arc::ptr_eq(&boxed, &retained));
        }
    }

    #[test]
    fn shared_owners_across_threads_destroy_the_payload_once() {
        struct Counted<'a>(&'a AtomicUsize);
        impl Drop for Counted<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = AtomicUsize::new(0);
        let value = try_arc(Counted(&drops)).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let shared = value.clone();
                scope.spawn(move || {
                    for _ in 0..64 {
                        drop(shared.clone());
                    }
                });
            }
            drop(value);
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn publication_path_and_source_refusals_keep_the_same_outcome() {
        for at in [1, 2] {
            let error = with_allocation_refusal(at, || {
                boxed_path_error(
                    Error::InvalidImmediatePolicy,
                    std::path::Path::new("published/file"),
                    Error::ErrorContextAllocationFailed {
                        context: crate::AllocationFailureContext::ParentSyncPending,
                    },
                    |path, source| Error::PublishedButParentSyncPending { path, source },
                )
            });
            assert!(matches!(
                error,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending
                }
            ));
        }
        let path = std::path::Path::new("published/file");
        assert_eq!(try_display_path(path).unwrap(), path.display().to_string());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = std::path::Path::new(std::ffi::OsStr::from_bytes(b"invalid/\xff/path"));
            assert_eq!(try_display_path(path).unwrap(), path.display().to_string());
        }
    }

    #[test]
    fn disk_error_wrapper_refusal_keeps_publication_facts() {
        let source = Error::ErrorContextAllocationFailed {
            context: crate::AllocationFailureContext::Committed { sequence: 31 },
        };
        let error = with_allocation_refusal(1, || {
            crate::disk_index::index_error(crate::DiskIndexError::Primary(source))
        });
        assert!(matches!(
            error,
            Error::ErrorContextAllocationFailed {
                context: crate::AllocationFailureContext::Committed { sequence: 31 }
            }
        ));
    }
}

#[cfg(kani)]
mod proofs;

#[cfg(kani)]
mod memory_proofs;
