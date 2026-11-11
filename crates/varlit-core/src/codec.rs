use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::mem::{align_of, size_of};

use crate::{Endian, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum WireType {
    Unit = 0,
    Bool = 1,
    U8 = 2,
    I8 = 3,
    U16 = 4,
    I16 = 5,
    U32 = 6,
    I32 = 7,
    U64 = 8,
    I64 = 9,
    U128 = 10,
    I128 = 11,
    F32 = 12,
    F64 = 13,
    Bytes = 14,
    String = 15,
    Seq = 16,
    Nested = 17,
}

impl WireType {
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Unit),
            1 => Some(Self::Bool),
            2 => Some(Self::U8),
            3 => Some(Self::I8),
            4 => Some(Self::U16),
            5 => Some(Self::I16),
            6 => Some(Self::U32),
            7 => Some(Self::I32),
            8 => Some(Self::U64),
            9 => Some(Self::I64),
            10 => Some(Self::U128),
            11 => Some(Self::I128),
            12 => Some(Self::F32),
            13 => Some(Self::F64),
            14 => Some(Self::Bytes),
            15 => Some(Self::String),
            16 => Some(Self::Seq),
            17 => Some(Self::Nested),
            _ => None,
        }
    }
}

/// FNV-1a 64 state for structural codec identities.
///
/// The derive expands the same folding inline so a generated
/// `SCHEMA_FINGERPRINT` is a const expression over its fields' real
/// `SCHEMA_ID` values rather than over their source spelling.
const SCHEMA_ID_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const SCHEMA_ID_PRIME: u64 = 0x0000_0100_0000_01b3;

const fn schema_id_bytes(acc: u64, bytes: &[u8]) -> u64 {
    let mut acc = acc;
    let mut index = 0;
    while index < bytes.len() {
        acc ^= bytes[index] as u64;
        acc = acc.wrapping_mul(SCHEMA_ID_PRIME);
        index += 1;
    }
    acc
}

const fn schema_id_u64(acc: u64, value: u64) -> u64 {
    schema_id_bytes(acc, &value.to_le_bytes())
}

/// Identity of a leaf codec whose byte layout is fully described by `tag`.
pub(crate) const fn leaf_schema_id(tag: &[u8]) -> u64 {
    schema_id_bytes(SCHEMA_ID_SEED, tag)
}

/// Identity of a container codec, folded transitively over its elements.
///
/// **Absence is contagious.** An element whose `SCHEMA_ID` is `0` declares no
/// identity, so the container built over it cannot claim one either: the
/// result is `0`, not a hash of `0`. Without this rule `Option<Packed>` or
/// `Vec<Packed>` would manufacture a non-zero identity out of two *different*
/// identity-less `Packed` codecs and hand them the same value — exactly the
/// collision `SCHEMA_ID` exists to prevent, merely one container deep. It also
/// keeps the derive's per-field assertion transitively complete: rejecting
/// zero at the field type rejects an identity-less codec at any nesting depth.
const fn container_schema_id(tag: &[u8], elements: &[u64]) -> u64 {
    container_schema_id_with_arity(tag, elements.len() as u64, elements)
}

/// [`container_schema_id`] for containers whose arity is part of the layout
/// rather than an element identity (arrays), so `[T; 0]` is not mistaken for
/// an identity-less element.
pub(crate) const fn container_schema_id_with_arity(
    tag: &[u8],
    arity: u64,
    elements: &[u64],
) -> u64 {
    let mut acc = schema_id_u64(leaf_schema_id(tag), arity);
    let mut index = 0;
    while index < elements.len() {
        if elements[index] == 0 {
            return 0;
        }
        acc = schema_id_u64(acc, elements[index]);
        index += 1;
    }
    acc
}

pub trait VarlitEncode {
    const WIRE_TYPE: WireType;

    /// Structural identity of the bytes this codec emits.
    ///
    /// [`WireType`] is far too coarse to separate two custom codecs that both
    /// encode as [`WireType::Nested`] but disagree on layout, and a derived
    /// block's fingerprint cannot see through a field type's source spelling.
    /// `SCHEMA_ID` is the value that does distinguish them: built-in scalars
    /// and containers define it structurally (containers fold their element
    /// ids transitively), and `#[derive(VarlitBlock)]` computes it from the
    /// block identity plus every field's own `SCHEMA_ID`. Blocks therefore
    /// fingerprint their fields by resolved codec identity, and
    /// [`crate::FormatSpec::computed_schema_hash`] inherits that transitively.
    ///
    /// **Trust boundary.** The default `0` means "this codec declares no
    /// identity". A manual codec that leaves it at `0` is indistinguishable
    /// from any other identity-less codec with the same field type spelling,
    /// so every hand-written codec must declare a value that changes whenever
    /// its emitted bytes change. That requirement is enforced, not merely
    /// documented, wherever it can collide: `#[derive(VarlitBlock)]` rejects at
    /// compile time *any* field whose codec resolves to `SCHEMA_ID == 0`,
    /// regardless of [`WireType`], and the crate-private `container_schema_id`
    /// helper propagates the
    /// zero outward so wrapping an identity-less codec in `Option`, `Vec`, an
    /// array, a map or a tuple cannot launder it into an identity.
    const SCHEMA_ID: u64 = 0;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()>;

    /// Encode consecutive values exactly as repeated [`Self::encode_varlit`]
    /// calls would. Fixed-width built-in scalars override this with one limit
    /// check and one fallible reservation; other codecs keep the default.
    #[doc(hidden)]
    fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()>
    where
        Self: Sized,
    {
        for value in values {
            value.encode_varlit(encoder)?;
        }
        Ok(())
    }
}

/// A value to decode into: the starting point for the read APIs that fill a
/// caller-owned value. It need not be meaningful, only valid and cheap.
/// Derived blocks implement it field by field.
pub trait VarlitEmpty {
    fn empty() -> Self;
}

macro_rules! empty_zero {
    ($($ty:ty => $zero:expr),+ $(,)?) => {
        $(impl VarlitEmpty for $ty {
            #[inline]
            fn empty() -> Self {
                $zero
            }
        })+
    };
}
empty_zero!(
    () => (), bool => false, u8 => 0, u16 => 0, u32 => 0, u64 => 0, u128 => 0,
    i8 => 0, i16 => 0, i32 => 0, i64 => 0, i128 => 0, f32 => 0.0, f64 => 0.0,
    String => String::new()
);
impl<T> VarlitEmpty for Vec<T> {
    fn empty() -> Self {
        Vec::new()
    }
}
impl<T> VarlitEmpty for Option<T> {
    fn empty() -> Self {
        None
    }
}
impl<T: VarlitEmpty, const N: usize> VarlitEmpty for [T; N] {
    fn empty() -> Self {
        std::array::from_fn(|_| T::empty())
    }
}
impl<K, V> VarlitEmpty for BTreeMap<K, V> {
    fn empty() -> Self {
        BTreeMap::new()
    }
}
impl<K, V, S: Default> VarlitEmpty for HashMap<K, V, S> {
    fn empty() -> Self {
        HashMap::default()
    }
}
impl<A: VarlitEmpty, B: VarlitEmpty> VarlitEmpty for (A, B) {
    fn empty() -> Self {
        (A::empty(), B::empty())
    }
}
impl<A: VarlitEmpty, B: VarlitEmpty, C: VarlitEmpty> VarlitEmpty for (A, B, C) {
    fn empty() -> Self {
        (A::empty(), B::empty(), C::empty())
    }
}
impl<A: VarlitEmpty, B: VarlitEmpty, C: VarlitEmpty, D: VarlitEmpty> VarlitEmpty for (A, B, C, D) {
    fn empty() -> Self {
        (A::empty(), B::empty(), C::empty(), D::empty())
    }
}

/// Internal evidence for a library-owned codec implementation.
/// The private invariant field binds the evidence to its exact output type.
///
/// Safe downstream code cannot construct evidence or reuse another type's:
/// ```compile_fail
/// use varlit_core::BuiltinCodec;
/// let _: BuiltinCodec<()> = BuiltinCodec { invariant: std::marker::PhantomData };
/// ```
/// ```compile_fail
/// use varlit_core::{BuiltinCodec, Decoder, Result, VarlitDecode, WireType};
/// struct Custom;
/// impl VarlitDecode for Custom {
///     const WIRE_TYPE: WireType = WireType::Unit;
///     const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = <u64 as VarlitDecode>::__VARLIT_BUILTIN;
///     fn decode_varlit(_: &mut Decoder<'_>) -> Result<Self> { Ok(Self) }
/// }
/// ```
#[doc(hidden)]
pub struct BuiltinCodec<T> {
    invariant: std::marker::PhantomData<fn(T) -> T>,
    callback_free: bool,
}

pub trait VarlitDecode: Sized {
    #[doc(hidden)]
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = None;
    const WIRE_TYPE: WireType;

    /// Structural identity of the bytes this codec accepts; see
    /// [`VarlitEncode::SCHEMA_ID`]. Derived block fingerprints fold both the
    /// encode and the decode identity of every field, so an asymmetric manual
    /// codec cannot pass itself off as its own mirror image.
    const SCHEMA_ID: u64 = 0;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self>;

    /// Decode into an existing value. Codecs that own heap storage override
    /// this to reuse it; the default decodes a fresh value through the
    /// checked boundary and assigns it. Must accept exactly what
    /// [`Self::decode_varlit`] accepts, charge the same budget and leave `out`
    /// equal to what it would return. On error `out` is valid but unspecified.
    fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
        *out = decoder.decode_value::<Self>()?;
        Ok(())
    }

    /// Decode a fixed array, including its input and materialization checks.
    ///
    /// The default calls the element codec and drops the initialized prefix on
    /// failure. Implementations may batch equivalent operations, but must retain
    /// the same validation and resource accounting. Existing custom codecs need
    /// not implement this method.
    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        decoder.preflight_count(
            N,
            minimum_wire_size(Self::WIRE_TYPE),
            size_of::<Self>(),
            "array",
        )?;
        if size_of::<Option<Self>>().saturating_mul(N) <= ARRAY_STACK_DECODE_BUDGET {
            decode_array_on_stack(decoder)
        } else {
            decode_array_on_heap(decoder)
        }
    }
}

#[derive(Debug)]
pub struct Encoder {
    endian: Endian,
    output: Vec<u8>,
    /// Bytes already in `output` before this encode began. Limits and the
    /// lengths they report count only what this encoder appended.
    base: usize,
    max_len: Option<u64>,
    limit_resource: &'static str,
    failure: Option<EncoderFailure>,
}

// Retain only the failure facts that encoding can produce. Storing the general
// Error (which can contain a boxed I/O error) would remove Encoder's public
// UnwindSafe/RefUnwindSafe auto traits even when that variant is never used.
#[derive(Debug)]
enum EncoderFailure {
    LimitExceeded { actual: u64, limit: u64 },
    AllocationFailed { requested: u64 },
}

impl EncoderFailure {
    fn into_error(self, resource: &'static str) -> Error {
        match self {
            Self::LimitExceeded { actual, limit } => Error::LimitExceeded {
                resource,
                actual,
                limit,
            },
            Self::AllocationFailed { requested } => Error::AllocationFailed {
                resource,
                requested,
            },
        }
    }
}

impl Encoder {
    pub fn new(endian: Endian) -> Self {
        Self {
            endian,
            output: Vec::new(),
            base: 0,
            max_len: None,
            limit_resource: "encoded payload",
            failure: None,
        }
    }

    pub(crate) fn new_limited(endian: Endian, max_len: u64, resource: &'static str) -> Self {
        Self {
            endian,
            output: Vec::new(),
            base: 0,
            max_len: Some(max_len),
            limit_resource: resource,
            failure: None,
        }
    }

    /// [`Encoder::new_limited`] over a buffer the caller owns and gets back.
    ///
    /// The writer-owned scratch that keeps a steady-state append from
    /// allocating once per record: the capacity survives, only the length is
    /// reset.
    pub(crate) fn new_limited_with_buffer(
        endian: Endian,
        max_len: u64,
        resource: &'static str,
        mut output: Vec<u8>,
    ) -> Self {
        output.clear();
        Self {
            endian,
            output,
            base: 0,
            max_len: Some(max_len),
            limit_resource: resource,
            failure: None,
        }
    }

    /// [`Encoder::new_limited_with_buffer`] that appends after the bytes
    /// already in `output` instead of clearing them. `max_len` and every
    /// reported length count only the appended bytes, so a value staged into
    /// a shared chunk buffer is limited exactly as if it had its own buffer.
    pub(crate) fn new_limited_appending(
        endian: Endian,
        max_len: u64,
        resource: &'static str,
        output: Vec<u8>,
    ) -> Self {
        Self {
            endian,
            base: output.len(),
            output,
            max_len: Some(max_len),
            limit_resource: resource,
            failure: None,
        }
    }

    /// Bytes appended by this encoder (excludes any pre-existing prefix).
    fn appended_len(&self) -> usize {
        self.output.len() - self.base
    }

    /// Hands the buffer back whichever way the encode went.
    ///
    /// [`Encoder::try_into_inner`] drops it on the error path, which is right
    /// when the buffer is the encoder's own and wrong when it belongs to a
    /// writer that needs it for the next record — a failed encode must not cost
    /// the writer its scratch.
    pub(crate) fn into_buffer_and_result(self) -> (Vec<u8>, Result<()>) {
        match self.failure {
            Some(error) => (self.output, Err(error.into_error(self.limit_resource))),
            None => (self.output, Ok(())),
        }
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.output
    }

    pub(crate) fn try_into_inner(self) -> Result<Vec<u8>> {
        if let Some(error) = self.failure {
            return Err(error.into_error(self.limit_resource));
        }
        Ok(self.output)
    }

    /// Encodes a nested value into its own buffer while inheriting this
    /// encoder's remaining output budget.
    ///
    /// A nested field encoded through this method can never buffer past the
    /// limit its parent writer entry point imposed: the child encoder is
    /// capped at the parent's remaining budget and reports the same typed
    /// [`Error::LimitExceeded`] resource on overflow. An unlimited parent
    /// yields an unlimited child, matching [`encode_to_vec`].
    pub fn encode_nested_to_vec<T: VarlitEncode>(&self, value: &T) -> Result<Vec<u8>> {
        let Some(max_len) = self.max_len else {
            return encode_to_vec(value, self.endian);
        };
        let remaining = max_len.saturating_sub(self.appended_len() as u64);
        encode_to_vec_limited(value, self.endian, remaining, self.limit_resource)
    }

    pub fn write_all(&mut self, bytes: &[u8]) {
        if self.max_len.is_none() {
            self.output.extend_from_slice(bytes);
            return;
        }
        if self.failure.is_some() {
            return;
        }
        let actual = (self.appended_len() as u64).checked_add(bytes.len() as u64);
        if let Some(limit) = self.max_len
            && actual.is_none_or(|actual| actual > limit)
        {
            self.failure = Some(EncoderFailure::LimitExceeded {
                actual: actual.unwrap_or(u64::MAX),
                limit,
            });
            return;
        }
        if bytes.len() > self.output.capacity() - self.output.len()
            && self.output.try_reserve(bytes.len()).is_err()
        {
            self.failure = Some(EncoderFailure::AllocationFailed {
                requested: actual.unwrap_or(u64::MAX),
            });
            return;
        }
        self.output.extend_from_slice(bytes);
    }

    pub fn write_u8(&mut self, value: u8) {
        self.write_all(&[value]);
    }

    /// Writes fixed-width elements with the bytes, limit failure and output
    /// prefix that one scalar write per element would produce.
    ///
    /// A limited encoder checks the limit before reserving, then reserves only
    /// the elements that fit, once and fallibly. When an element would cross
    /// the limit the fitting prefix is written and the failure reports the
    /// same `actual` length the per-element path reports. An unlimited encoder
    /// keeps the infallible `extend` behaviour of [`Self::write_all`].
    fn write_fixed_slice<T: Copy, const W: usize>(
        &mut self,
        values: &[T],
        little: impl Fn(T) -> [u8; W],
        big: impl Fn(T) -> [u8; W],
    ) {
        if values.is_empty() || self.failure.is_some() {
            return;
        }
        let mut count = values.len();
        let mut exceeded = None;
        if let Some(limit) = self.max_len {
            let used = self.appended_len() as u64;
            let fitting = limit.saturating_sub(used) / W as u64;
            if fitting < count as u64 {
                // `fitting < count` so it fits in usize.
                count = fitting as usize;
                exceeded =
                    Some(used.saturating_add(fitting.saturating_add(1).saturating_mul(W as u64)));
            }
        }
        let Some(bytes) = count.checked_mul(W) else {
            self.failure = Some(EncoderFailure::AllocationFailed {
                requested: u64::MAX,
            });
            return;
        };
        if self.max_len.is_some() {
            if bytes > self.output.capacity() - self.output.len()
                && self.output.try_reserve(bytes).is_err()
            {
                self.failure = Some(EncoderFailure::AllocationFailed {
                    requested: (self.appended_len() as u64).saturating_add(bytes as u64),
                });
                return;
            }
        } else {
            self.output.reserve(bytes);
        }
        let start = self.output.len();
        self.output.resize(start + bytes, 0);
        let (destination, _) = self.output[start..].as_chunks_mut::<W>();
        match self.endian {
            Endian::Little => {
                for (out, value) in destination.iter_mut().zip(values) {
                    *out = little(*value);
                }
            }
            Endian::Big => {
                for (out, value) in destination.iter_mut().zip(values) {
                    *out = big(*value);
                }
            }
        }
        if let Some(actual) = exceeded {
            self.failure = Some(EncoderFailure::LimitExceeded {
                actual,
                limit: self.max_len.unwrap_or(u64::MAX),
            });
        }
    }

    pub fn write_u16(&mut self, value: u16) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u32(&mut self, value: u32) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u64(&mut self, value: u64) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u128(&mut self, value: u128) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }
}

#[derive(Debug)]
pub struct Decoder<'a> {
    endian: Endian,
    input: &'a [u8],
    position: usize,
    small_field_ids: u64,
    large_field_ids: Option<HashSet<u32>>,
    materialization_limit: u64,
    materialization_remaining: u64,
    // A short-lived borrow makes an active callback frame unforgeable by safe
    // custom codecs, including whole-value replacement with a fresh Decoder.
    callback_scope: Option<&'a u8>,
}

// Sealed static adapters keep every extensible trait call inside the checked
// boundary and let the compiler inline builtin codecs without an FnOnce shim.
trait DecoderCallback {
    /// What the adapter is handed besides the frame: nothing for a value
    /// decode, the caller's destination for an in-place decode.
    type Input;
    type Output;
    const BUILTIN: bool;
    const CALLBACK_FREE: bool;
    fn call(input: Self::Input, decoder: &mut Decoder<'_>) -> Result<Self::Output>;
}
struct ValueCallback<T>(std::marker::PhantomData<T>);
struct ArrayCallback<T, const N: usize>(std::marker::PhantomData<T>);
struct IntoCallback<'o, T>(std::marker::PhantomData<&'o mut T>);
impl<T: VarlitDecode> DecoderCallback for ValueCallback<T> {
    type Input = ();
    type Output = T;
    const BUILTIN: bool = T::__VARLIT_BUILTIN.is_some();
    const CALLBACK_FREE: bool = matches!(
        T::__VARLIT_BUILTIN,
        Some(BuiltinCodec {
            callback_free: true,
            ..
        })
    );
    #[inline(always)]
    fn call((): (), decoder: &mut Decoder<'_>) -> Result<T> {
        T::decode_varlit(decoder)
    }
}
impl<T: VarlitDecode, const N: usize> DecoderCallback for ArrayCallback<T, N> {
    type Input = ();
    type Output = [T; N];
    const BUILTIN: bool = T::__VARLIT_BUILTIN.is_some();
    const CALLBACK_FREE: bool = matches!(
        T::__VARLIT_BUILTIN,
        Some(BuiltinCodec {
            callback_free: true,
            ..
        })
    );
    #[inline(always)]
    fn call((): (), decoder: &mut Decoder<'_>) -> Result<[T; N]> {
        T::decode_array(decoder)
    }
}
impl<'o, T: VarlitDecode> DecoderCallback for IntoCallback<'o, T> {
    type Input = &'o mut T;
    type Output = ();
    const BUILTIN: bool = T::__VARLIT_BUILTIN.is_some();
    const CALLBACK_FREE: bool = matches!(
        T::__VARLIT_BUILTIN,
        Some(BuiltinCodec {
            callback_free: true,
            ..
        })
    );
    #[inline(always)]
    fn call(out: &'o mut T, decoder: &mut Decoder<'_>) -> Result<()> {
        T::decode_varlit_into(out, decoder)
    }
}

// Nested checked calls already have a short-lived, unforgeable frame borrow.
// Reuse that decoder instead of moving its field-id set at every scalar call.
struct CallbackGuard<'d, 'i> {
    decoder: &'d mut Decoder<'i>,
    input: &'i [u8],
    endian: Endian,
    limit: u64,
    scope: Option<&'i u8>,
}
impl Drop for CallbackGuard<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        *self.decoder = Decoder {
            input: self.input,
            endian: self.endian,
            materialization_limit: self.limit,
            callback_scope: self.scope,
            position: self.input.len(),
            materialization_remaining: 0,
            small_field_ids: 0,
            large_field_ids: None,
        };
    }
}

impl<'a> Decoder<'a> {
    pub const STANDARD_MATERIALIZATION_LIMIT: u64 = 1024 * 1024 * 1024;

    #[inline]
    pub fn new(input: &'a [u8], endian: Endian) -> Self {
        Self::new_limited(input, endian, Self::STANDARD_MATERIALIZATION_LIMIT)
    }

    #[inline]
    pub fn new_limited(input: &'a [u8], endian: Endian, materialization_limit: u64) -> Self {
        Self {
            endian,
            input,
            position: 0,
            small_field_ids: 0,
            large_field_ids: None,
            materialization_limit,
            materialization_remaining: materialization_limit,
            callback_scope: None,
        }
    }

    #[inline(always)]
    pub fn decode_from_slice_limited<T: VarlitDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_limit: u64,
    ) -> Result<T> {
        let mut decoder = Self::new_limited(input, endian, materialization_limit);
        decoder.decode_complete()
    }

    pub fn decode_from_slice_accounted<T: VarlitDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_remaining: &mut u64,
    ) -> Result<T> {
        let mut decoder = Self::new_limited(input, endian, *materialization_remaining);
        // A panicking callback cannot refund previously charged work.
        *materialization_remaining = 0;
        let result = decoder.decode_complete();
        *materialization_remaining = decoder.materialization_remaining;
        result
    }

    /// [`Self::decode_from_slice_limited`] into an existing value; see
    /// [`Self::decode_value_into`].
    pub fn decode_from_slice_limited_into<T: VarlitDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_limit: u64,
        out: &mut T,
    ) -> Result<()> {
        let mut decoder = Self::new_limited(input, endian, materialization_limit);
        decoder.decode_complete_into(out)
    }

    /// [`Self::decode_from_slice_accounted`] into an existing value.
    pub fn decode_from_slice_accounted_into<T: VarlitDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_remaining: &mut u64,
        out: &mut T,
    ) -> Result<()> {
        let mut decoder = Self::new_limited(input, endian, *materialization_remaining);
        // A panicking callback cannot refund previously charged work.
        *materialization_remaining = 0;
        let result = decoder.decode_complete_into(out);
        *materialization_remaining = decoder.materialization_remaining;
        result
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    pub fn materialization_remaining(&self) -> u64 {
        self.materialization_remaining
    }

    pub fn decode_nested<T: VarlitDecode>(&mut self, input: &[u8]) -> Result<T> {
        let mut nested = Decoder::new_limited(input, self.endian, self.materialization_remaining);
        self.materialization_remaining = 0;
        let result = nested.decode_complete();
        self.materialization_remaining = nested.materialization_remaining;
        result
    }

    /// [`Self::decode_nested`] into an existing value; see
    /// [`Self::decode_value_into`].
    pub fn decode_nested_into<T: VarlitDecode>(&mut self, input: &[u8], out: &mut T) -> Result<()> {
        let mut nested = Decoder::new_limited(input, self.endian, self.materialization_remaining);
        self.materialization_remaining = 0;
        let result = nested.decode_complete_into(out);
        self.materialization_remaining = nested.materialization_remaining;
        result
    }

    #[inline]
    pub fn charge_materialization(&mut self, bytes: u64, resource: &'static str) -> Result<()> {
        if let Some(next) = crate::decode_state::charge(self.materialization_remaining, bytes) {
            self.materialization_remaining = next;
            return Ok(());
        }
        let consumed = self
            .materialization_limit
            .checked_sub(self.materialization_remaining)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        let actual = consumed
            .checked_add(bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit: self.materialization_limit,
        })
    }

    /// Decode one value while preserving this decoder's input and budget authority.
    ///
    /// Custom codecs should use this method when composing other codecs. A codec
    /// that returns with a fresh decoder or violates its cursor/budget boundary
    /// is rejected. Completed charges survive errors. A callback panic or invalid
    /// replacement exhausts this frame's input and remaining budget.
    #[inline(always)]
    pub fn decode_value<T: VarlitDecode>(&mut self) -> Result<T> {
        self.with_callback::<ValueCallback<T>>(())
    }

    /// [`Self::decode_value`] into an existing value, reusing what it owns
    /// (vector and string capacity) where its codec supports that. Runs
    /// through the same checked frame, budget and validation. On error `out`
    /// is valid but its contents are unspecified.
    #[inline(always)]
    pub fn decode_value_into<T: VarlitDecode>(&mut self, out: &mut T) -> Result<()> {
        self.with_callback::<IntoCallback<'_, T>>(out)
    }

    #[inline(always)]
    fn with_callback<C: DecoderCallback>(&mut self, input: C::Input) -> Result<C::Output> {
        if C::CALLBACK_FREE || (C::BUILTIN && self.callback_scope.is_some()) {
            return C::call(input, self);
        }
        if self.callback_scope.is_some() {
            return self.with_existing_callback::<C>(input);
        }
        let scope = 0u8;
        let start = self.position;
        let before = self.materialization_remaining;
        // Borrow input only for this call. An active frame cannot be retained
        // in a 'static slot and reused to forge a later invocation's authority.
        let mut frame = Decoder {
            endian: self.endian,
            input: self.input,
            position: start,
            small_field_ids: self.small_field_ids,
            large_field_ids: self.large_field_ids.take(),
            materialization_limit: self.materialization_limit,
            materialization_remaining: before,
            callback_scope: Some(&scope),
        };
        // Establish the safe unwind/rejection state before invoking user code.
        self.position = self.input.len();
        self.materialization_remaining = 0;
        let result = C::call(input, &mut frame);
        let linked = frame
            .callback_scope
            .is_some_and(|actual| std::ptr::eq(actual, &scope))
            && std::ptr::eq(frame.input, self.input)
            && frame.endian as u8 == self.endian as u8
            && frame.materialization_limit == self.materialization_limit;
        if !crate::decode_state::callback_exit(
            linked,
            start,
            frame.position,
            self.input.len(),
            before,
            frame.materialization_remaining,
        ) {
            // Preserve a callback error rather than destroying and replacing
            // it. Successful values are released before rejecting the frame.
            return match result {
                Err(error) => Err(error),
                Ok(_value) => Err(Error::InvalidCanonicalEncoding(
                    "codec replaced or invalidated decoder state",
                )),
            };
        }
        self.position = frame.position;
        self.materialization_remaining = frame.materialization_remaining;
        self.small_field_ids = frame.small_field_ids;
        self.large_field_ids = frame.large_field_ids.take();
        result
    }

    #[inline(always)]
    fn with_existing_callback<C: DecoderCallback>(
        &mut self,
        destination: C::Input,
    ) -> Result<C::Output> {
        let input = self.input;
        let endian = self.endian;
        let limit = self.materialization_limit;
        let scope = self.callback_scope;
        let guard = CallbackGuard {
            decoder: self,
            input,
            endian,
            limit,
            scope,
        };
        let result = C::call(destination, guard.decoder);
        // All safe methods preserve the numeric invariant and immutable frame
        // fields; the MIR closure gate covers every writer. Fresh replacement
        // cannot reproduce the private, short-lived scope borrow.
        if !crate::decode_state::same_scope(scope, guard.decoder.callback_scope) {
            return match result {
                Err(error) => Err(error),
                Ok(_value) => Err(Error::InvalidCanonicalEncoding(
                    "codec replaced or invalidated decoder state",
                )),
            };
        }
        // The guard owns only borrows/scalars. On successful admission its
        // rollback is unnecessary; forgetting it releases no owned resource.
        std::mem::forget(guard);
        result
    }

    #[inline(always)]
    fn decode_complete_into<T: VarlitDecode>(&mut self, out: &mut T) -> Result<()> {
        self.decode_value_into(out)?;
        if self.remaining() != 0 {
            return Err(Error::TrailingBytes {
                remaining: self.remaining(),
            });
        }
        Ok(())
    }

    #[inline(always)]
    fn decode_complete<T: VarlitDecode>(&mut self) -> Result<T> {
        let value = self.decode_value::<T>()?;
        if self.remaining() != 0 {
            return Err(Error::TrailingBytes {
                remaining: self.remaining(),
            });
        }
        Ok(value)
    }

    #[inline]
    pub fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = crate::decode_state::read_end(self.position, len, self.input.len())
            .ok_or(Error::UnexpectedEof)?;
        let bytes = &self.input[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    #[inline]
    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    #[inline]
    pub fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(bytes),
            Endian::Big => u16::from_be_bytes(bytes),
        })
    }

    #[inline]
    pub fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        })
    }

    #[inline]
    pub fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(match self.endian {
            Endian::Little => u64::from_le_bytes(bytes),
            Endian::Big => u64::from_be_bytes(bytes),
        })
    }

    #[inline]
    pub fn read_u128(&mut self) -> Result<u128> {
        let mut bytes = [0; 16];
        bytes.copy_from_slice(self.read_exact(16)?);
        Ok(match self.endian {
            Endian::Little => u128::from_le_bytes(bytes),
            Endian::Big => u128::from_be_bytes(bytes),
        })
    }

    /// Consumes `count` fixed-width elements as one input slice.
    ///
    /// Callers run [`Self::preflight_count`] first, which already bounds
    /// `count` by the remaining input and charges the materialization budget;
    /// this only performs the bounds-checked read.
    fn read_fixed_run(
        &mut self,
        count: usize,
        width: usize,
        resource: &'static str,
    ) -> Result<&'a [u8]> {
        let bytes = count
            .checked_mul(width)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        self.read_exact(bytes)
    }

    pub fn read_len(&mut self) -> Result<usize> {
        let value = self.read_u64()?;
        usize::try_from(value).map_err(|_| Error::LengthOverflow { value })
    }

    fn preflight_count(
        &mut self,
        len: usize,
        minimum_wire_bytes: usize,
        materialized_entry_bytes: usize,
        resource: &'static str,
    ) -> Result<()> {
        if minimum_wire_bytes != 0 && len > self.remaining() / minimum_wire_bytes {
            return Err(Error::UnexpectedEof);
        }
        if minimum_wire_bytes == 0
            && u64::try_from(len).unwrap_or(u64::MAX) > self.materialization_remaining
        {
            return Err(Error::InvalidCanonicalEncoding(
                "collection count cannot make bounded input progress",
            ));
        }
        let charged_entry_bytes = materialized_entry_bytes.max(1);
        let bytes = len
            .checked_mul(charged_entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        self.charge_materialization(
            u64::try_from(bytes).map_err(|_| Error::ResourceArithmeticOverflow { resource })?,
            resource,
        )
    }

    /// Wire-length screen shared by both map codecs.
    ///
    /// Returns the declared count unchanged once the remaining input could
    /// physically carry it; the caller then charges its own materialization
    /// model on top.
    fn screen_map_wire_length<K: VarlitDecode, V: VarlitDecode>(
        &self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        let key_bytes = minimum_wire_size(K::WIRE_TYPE);
        let entry_bytes = key_bytes.saturating_add(minimum_wire_size(V::WIRE_TYPE));
        let minimum_wire_bytes = len
            .checked_mul(entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        if minimum_wire_bytes > self.remaining() {
            return Err(Error::UnexpectedEof);
        }
        Ok(())
    }

    /// Preflight for a node-per-entry map (`BTreeMap`), whose storage really is
    /// driven by the entries it has already accepted.
    ///
    /// the charge used to be `len * size_of::<(K, V)>()`, which
    /// treats a `BTreeMap` as if it stored entries packed end to end. It does
    /// not. A std B-tree node carries a fixed-capacity array - it allocates
    /// room for `2B - 1 = 11` entries whatever its fill - behind a header
    /// (parent pointer, parent index, length), and internal nodes additionally
    /// carry `2B = 12` child pointers. A node is only guaranteed to hold
    /// `B - 1 = 5` entries, so per *live* entry the array alone can cost
    /// `11/5 = 2.2x` the naive model before any header is counted.
    ///
    /// The model below is deliberately an over-estimate, not a measurement:
    /// [`BTREE_ENTRY_SLOT_FACTOR`] covers the fixed-capacity array at minimum
    /// fill plus the internal-node fan-out, and [`BTREE_ENTRY_HEADER_BYTES`]
    /// covers node headers and child pointers amortised over the same minimum
    /// fill. `size_of::<(K, V)>()` is itself an upper bound on the two
    /// separate `K` and `V` arrays a node really holds. This charge is smaller
    /// than the hash-table charge in relative terms because a `BTreeMap` only
    /// grows per *accepted* entry, so the input must already carry at least
    /// one wire byte per entry.
    fn preflight_map_count<K: VarlitDecode, V: VarlitDecode>(
        &mut self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        self.screen_map_wire_length::<K, V>(len, resource)?;
        let entry_bytes = size_of::<(K, V)>()
            .max(1)
            .checked_mul(BTREE_ENTRY_SLOT_FACTOR)
            .and_then(|slots| slots.checked_add(BTREE_ENTRY_HEADER_BYTES))
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        self.preflight_count(len, 0, entry_bytes, resource)
    }

    /// Preflight for an open-addressed hash table (`HashMap`), whose storage is
    /// driven by the *declared* count long before any entry is validated
    ///
    ///
    /// `preflight_count` floors a zero-byte entry at a one-byte charge, which
    /// is a wild under-estimate for a hash table: a `HashMap<(), ()>` pays a
    /// control byte per bucket and rounds the bucket count up to a power of
    /// two, so a one-byte-per-entry charge let the standard 1 GiB budget admit
    /// close to one billion declared entries and hand `try_reserve` a
    /// multi-gigabyte table. Charge [`hash_table_reservation_bytes`] instead,
    /// which over-approximates what hashbrown actually allocates.
    fn preflight_hash_map_count<K: VarlitDecode, V: VarlitDecode>(
        &mut self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        self.screen_map_wire_length::<K, V>(len, resource)?;
        // Preserved from `preflight_count`: a count that no amount of bounded
        // input can make progress against is a canonical-encoding fault, not a
        // budget fault, and rejecting it here keeps hostile `u64::MAX` counts
        // out of the arithmetic below.
        if u64::try_from(len).unwrap_or(u64::MAX) > self.materialization_remaining {
            return Err(Error::InvalidCanonicalEncoding(
                "collection count cannot make bounded input progress",
            ));
        }
        // Growth retains the old table until the new table is populated. Both
        // allocations fit the final-count bound individually. Charge two tables
        // when the declared count can exceed the upfront reservation. Payload charges remain separate.
        let bytes =
            hash_map_decode_reservation_bytes::<(K, V)>(len).ok_or(Error::LimitExceeded {
                resource,
                actual: u64::MAX,
                limit: self.materialization_limit,
            })?;
        self.charge_materialization(bytes, resource)
    }

    fn note_field_id(&mut self, field_id: u32) -> Result<()> {
        if field_id == 0 {
            return Err(Error::InvalidCanonicalEncoding(
                "variable field id must be nonzero",
            ));
        }
        if field_id < u64::BITS {
            let mask = 1u64 << field_id;
            if self.small_field_ids & mask != 0 {
                return Err(Error::InvalidCanonicalEncoding("duplicate variable field"));
            }
            self.small_field_ids |= mask;
            return Ok(());
        }

        if self
            .large_field_ids
            .as_ref()
            .is_some_and(|field_ids| field_ids.contains(&field_id))
        {
            return Err(Error::InvalidCanonicalEncoding("duplicate variable field"));
        }
        // every other decoder-owned container charges the
        // materialization budget before it reserves. Duplicate detection for
        // ids above the small-id mask is the one decoder-owned set whose
        // growth is driven purely by attacker-chosen field ids, so charge
        // before reserving rather than after.
        let count = self.large_field_ids.as_ref().map_or(0, HashSet::len);
        let bound = |count| {
            hash_table_reservation_bytes::<u32>(count)
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "variable field ids",
                })
        };
        // Cover both old and new tables during a rehash, including power-of-two
        // capacity rounding and control bytes. Charge only the bound's growth.
        let charge = bound(
            count
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "variable field ids",
                })?,
        )? - bound(count)?;
        self.charge_materialization(charge, "variable field ids")?;
        let field_ids = self.large_field_ids.get_or_insert_with(HashSet::new);
        let requested = u64::try_from(field_ids.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
            .saturating_mul(4);
        field_ids
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "variable field ids",
                requested,
            })?;
        field_ids.insert(field_id);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldHeader {
    pub field_id: u32,
    pub wire_type: WireType,
    pub payload_len: u64,
}

pub fn write_field(
    encoder: &mut Encoder,
    field_id: u32,
    wire_type: WireType,
    payload: &[u8],
) -> Result<()> {
    if field_id == 0 {
        return Err(Error::InvalidCanonicalEncoding(
            "variable field id must be nonzero",
        ));
    }
    encoder.write_u32(field_id);
    encoder.write_u16(wire_type as u16);
    encoder.write_u16(0);
    encoder.write_u64(payload.len() as u64);
    encoder.write_all(payload);
    Ok(())
}

/// Writes one variable-block field with its value encoded directly into the
/// parent buffer: the header is written with a zero length, the value is
/// encoded in place, then the length is patched.
///
/// Produces the same bytes as `write_field` over `encode_nested_to_vec`, with
/// no intermediate buffer, copy or allocation. The value is bounded by the
/// encoder's own limit; on a limit or allocation failure the encoder records
/// it, the length is left unpatched and `try_into_inner` reports the error.
#[doc(hidden)]
pub fn encode_field_in_place<T: VarlitEncode + ?Sized>(
    encoder: &mut Encoder,
    field_id: u32,
    value: &T,
) -> Result<()> {
    if field_id == 0 {
        return Err(Error::InvalidCanonicalEncoding(
            "variable field id must be nonzero",
        ));
    }
    encoder.write_u32(field_id);
    encoder.write_u16(T::WIRE_TYPE as u16);
    encoder.write_u16(0);
    encoder.write_u64(0);
    let start = encoder.output.len();
    value.encode_varlit(encoder)?;
    if encoder.failure.is_none() {
        let length = (encoder.output.len() - start) as u64;
        let bytes = match encoder.endian {
            Endian::Little => length.to_le_bytes(),
            Endian::Big => length.to_be_bytes(),
        };
        encoder.output[start - 8..start].copy_from_slice(&bytes);
    }
    Ok(())
}

pub fn read_field_header(decoder: &mut Decoder<'_>) -> Result<FieldHeader> {
    let field_id = decoder.read_u32()?;
    let wire_type = decoder.read_u16()?;
    let flags = decoder.read_u16()?;
    let payload_len = decoder.read_u64()?;
    if flags != 0 {
        return Err(Error::InvalidCanonicalEncoding(
            "variable field flags must be zero",
        ));
    }
    let wire_type = WireType::from_u16(wire_type).ok_or(Error::UnknownWireType(wire_type))?;
    let payload_len_usize: usize =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    if payload_len_usize > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    decoder.note_field_id(field_id)?;
    Ok(FieldHeader {
        field_id,
        wire_type,
        payload_len,
    })
}

pub fn encode_to_vec<T: VarlitEncode>(value: &T, endian: Endian) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_limited(endian, u64::MAX, "encoded payload");
    value.encode_varlit(&mut encoder)?;
    encoder.try_into_inner()
}

pub(crate) fn encode_to_vec_limited<T: VarlitEncode>(
    value: &T,
    endian: Endian,
    max_len: u64,
    resource: &'static str,
) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_limited(endian, max_len, resource);
    value.encode_varlit(&mut encoder)?;
    encoder.try_into_inner()
}

/// [`encode_to_vec_limited`] into a caller-owned buffer.
///
/// `buffer` is restored on every path, including the error one.
pub(crate) fn encode_into_limited<T: VarlitEncode>(
    value: &T,
    endian: Endian,
    max_len: u64,
    resource: &'static str,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let mut encoder =
        Encoder::new_limited_with_buffer(endian, max_len, resource, std::mem::take(buffer));
    let encoded = value.encode_varlit(&mut encoder);
    let (output, finished) = encoder.into_buffer_and_result();
    *buffer = output;
    encoded.and(finished)
}

/// [`decode_from_slice`] into an existing value; see [`Decoder::decode_value_into`].
pub fn decode_from_slice_into<T: VarlitDecode>(
    bytes: &[u8],
    endian: Endian,
    out: &mut T,
) -> Result<()> {
    let mut decoder = Decoder::new(bytes, endian);
    decoder.decode_complete_into(out)
}

pub fn decode_from_slice<T: VarlitDecode>(bytes: &[u8], endian: Endian) -> Result<T> {
    Decoder::decode_from_slice_limited(bytes, endian, Decoder::STANDARD_MATERIALIZATION_LIMIT)
}

impl VarlitEncode for () {
    const WIRE_TYPE: WireType = WireType::Unit;
    const SCHEMA_ID: u64 = leaf_schema_id(b"unit");

    fn encode_varlit(&self, _encoder: &mut Encoder) -> Result<()> {
        Ok(())
    }
}

impl VarlitDecode for () {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::Unit;
    const SCHEMA_ID: u64 = leaf_schema_id(b"unit");

    fn decode_varlit(_decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
    }
}

macro_rules! unsigned {
    ($ty:ty, $wire:ident, $write:ident, $read:ident) => {
        impl VarlitEncode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self);
                Ok(())
            }

            fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
                encoder.write_fixed_slice(values, <$ty>::to_le_bytes, <$ty>::to_be_bytes);
                Ok(())
            }
        }

        impl VarlitDecode for $ty {
            const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
                invariant: std::marker::PhantomData,
                callback_free: true,
            });
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            #[inline]
            fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
                decoder.$read()
            }

            fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
                decode_fixed_array(decoder, 0, <$ty>::from_le_bytes, <$ty>::from_be_bytes)
            }
        }
    };
}

macro_rules! signed {
    ($ty:ty, $wire:ident, $uty:ty, $write:ident, $read:ident) => {
        impl VarlitEncode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self as $uty);
                Ok(())
            }

            fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
                encoder.write_fixed_slice(values, <$ty>::to_le_bytes, <$ty>::to_be_bytes);
                Ok(())
            }
        }

        impl VarlitDecode for $ty {
            const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
                invariant: std::marker::PhantomData,
                callback_free: true,
            });
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            #[inline]
            fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
                Ok(decoder.$read()? as Self)
            }

            fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
                decode_fixed_array(decoder, 0, <$ty>::from_le_bytes, <$ty>::from_be_bytes)
            }
        }
    };
}

impl VarlitEncode for u8 {
    const WIRE_TYPE: WireType = WireType::U8;
    const SCHEMA_ID: u64 = leaf_schema_id(b"u8");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u8(*self);
        Ok(())
    }

    fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
        encoder.write_fixed_slice(values, |v| [v], |v| [v]);
        Ok(())
    }
}

impl VarlitDecode for u8 {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::U8;
    const SCHEMA_ID: u64 = leaf_schema_id(b"u8");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        decoder.read_u8()
    }

    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        decoder.preflight_count(N, 1, 1, "array")?;
        if N <= ARRAY_STACK_DECODE_BUDGET {
            decode_byte_array_on_stack(decoder)
        } else {
            // Keep large array temporaries behind the fallible heap reservation.
            decode_array_on_heap(decoder)
        }
    }
}
unsigned!(u16, U16, write_u16, read_u16);
unsigned!(u32, U32, write_u32, read_u32);
unsigned!(u64, U64, write_u64, read_u64);
unsigned!(u128, U128, write_u128, read_u128);
signed!(i8, I8, u8, write_u8, read_u8);
signed!(i16, I16, u16, write_u16, read_u16);
signed!(i32, I32, u32, write_u32, read_u32);
signed!(i64, I64, u64, write_u64, read_u64);
signed!(i128, I128, u128, write_u128, read_u128);

impl VarlitEncode for bool {
    const WIRE_TYPE: WireType = WireType::Bool;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bool");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u8(u8::from(*self));
        Ok(())
    }

    fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
        encoder.write_fixed_slice(values, |v| [u8::from(v)], |v| [u8::from(v)]);
        Ok(())
    }
}

impl VarlitDecode for bool {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::Bool;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bool");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        match decoder.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidCanonicalEncoding(
                "boolean value must be 0 or 1",
            )),
        }
    }

    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        decoder.preflight_count(N, 1, size_of::<bool>(), "array")?;
        let bytes = decoder.read_fixed_run(N, 1, "array")?;
        check_bool_bytes(bytes)?;
        if size_of::<Option<bool>>().saturating_mul(N) <= ARRAY_STACK_DECODE_BUDGET {
            let mut out = [false; N];
            for (slot, &byte) in out.iter_mut().zip(bytes) {
                *slot = byte == 1;
            }
            Ok(out)
        } else {
            let mut values = Vec::new();
            values
                .try_reserve_exact(N)
                .map_err(|_| Error::AllocationFailed {
                    resource: "array",
                    requested: allocation_request::<bool>(N),
                })?;
            values.extend(bytes.iter().map(|&byte| byte == 1));
            values.try_into().map_err(|_| Error::UnexpectedEof)
        }
    }
}

impl VarlitEncode for f32 {
    const WIRE_TYPE: WireType = WireType::F32;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f32");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u32(self.to_bits());
        Ok(())
    }

    fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
        encoder.write_fixed_slice(values, f32::to_le_bytes, f32::to_be_bytes);
        Ok(())
    }
}

impl VarlitDecode for f32 {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::F32;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f32");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u32()?))
    }

    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        decode_fixed_array(decoder, 0.0, f32::from_le_bytes, f32::from_be_bytes)
    }
}

impl VarlitEncode for f64 {
    const WIRE_TYPE: WireType = WireType::F64;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f64");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.to_bits());
        Ok(())
    }

    fn __varlit_encode_slice(values: &[Self], encoder: &mut Encoder) -> Result<()> {
        encoder.write_fixed_slice(values, f64::to_le_bytes, f64::to_be_bytes);
        Ok(())
    }
}

impl VarlitDecode for f64 {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::F64;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f64");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u64()?))
    }

    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        decode_fixed_array(decoder, 0.0, f64::from_le_bytes, f64::from_be_bytes)
    }
}

impl VarlitEncode for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bytes");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self);
        Ok(())
    }
}

impl VarlitDecode for Vec<u8> {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bytes");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let mut value = Vec::new();
        fill_bytes(decoder, &mut value, "byte vector")?;
        Ok(value)
    }

    #[inline]
    fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
        fill_bytes(decoder, out, "byte vector")
    }
}

/// Length-prefixed bytes into `out`, reusing its capacity: input bound and
/// budget charge before any reservation.
#[inline]
fn fill_bytes(decoder: &mut Decoder<'_>, out: &mut Vec<u8>, resource: &'static str) -> Result<()> {
    let len = decoder.read_len()?;
    if len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    decoder.charge_materialization(len as u64, resource)?;
    let bytes = decoder.read_exact(len)?;
    out.clear();
    out.try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len as u64,
        })?;
    out.extend_from_slice(bytes);
    Ok(())
}

impl VarlitEncode for String {
    const WIRE_TYPE: WireType = WireType::String;
    const SCHEMA_ID: u64 = leaf_schema_id(b"string");

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self.as_bytes());
        Ok(())
    }
}

impl VarlitDecode for String {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: true,
    });
    const WIRE_TYPE: WireType = WireType::String;
    const SCHEMA_ID: u64 = leaf_schema_id(b"string");

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        if len > decoder.remaining() {
            return Err(Error::UnexpectedEof);
        }
        decoder.charge_materialization(len as u64, "string")?;
        let bytes = decoder.read_exact(len)?;
        let mut value = Vec::new();
        value
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "string",
                requested: len as u64,
            })?;
        value.extend_from_slice(bytes);
        String::from_utf8(value).map_err(|_| Error::InvalidUtf8)
    }

    #[inline]
    fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
        let len = decoder.read_len()?;
        if len > decoder.remaining() {
            return Err(Error::UnexpectedEof);
        }
        decoder.charge_materialization(len as u64, "string")?;
        let text = std::str::from_utf8(decoder.read_exact(len)?).map_err(|_| Error::InvalidUtf8)?;
        out.clear();
        out.try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "string",
                requested: len as u64,
            })?;
        out.push_str(text);
        Ok(())
    }
}

impl<T> VarlitEncode for Option<T>
where
    T: VarlitEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"option", &[T::SCHEMA_ID]);

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        match self {
            Some(value) => {
                true.encode_varlit(encoder)?;
                value.encode_varlit(encoder)?;
            }
            None => {
                false.encode_varlit(encoder)?;
            }
        }
        Ok(())
    }
}

impl<T> VarlitDecode for Option<T>
where
    T: VarlitDecode,
{
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: matches!(
            T::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ),
    });
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"option", &[T::SCHEMA_ID]);

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        if decoder.decode_value::<bool>()? {
            Ok(Some(decoder.decode_value::<T>()?))
        } else {
            Ok(None)
        }
    }

    fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
        if decoder.decode_value::<bool>()? {
            match out {
                Some(inner) => decoder.decode_value_into(inner)?,
                None => *out = Some(decoder.decode_value::<T>()?),
            }
        } else {
            *out = None;
        }
        Ok(())
    }
}

impl<T, const N: usize> VarlitEncode for [T; N]
where
    T: VarlitEncode,
{
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id_with_arity(b"array", N as u64, &[T::SCHEMA_ID]);

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        T::__varlit_encode_slice(self, encoder)
    }
}

/// Ceiling, in bytes, on the transient `[Option<T>; N]` a fixed-array decode
/// may build on the stack.
///
/// Above it the decode keeps the fallible heap reservation it has always used.
/// A format author's declared array length must not be able to turn into an
/// unbounded stack frame: that would trade a bounded, fallible heap allocation
/// for an unbounded one that cannot be refused, which is the opposite of what
/// this crate's allocation discipline exists for.
const ARRAY_STACK_DECODE_BUDGET: usize = 16 * 1024;

/// Decodes `[T; N]` through a stack `[Option<T>; N]`, allocating nothing.
///
/// `#[inline(never)]` is load-bearing here, not a hint. The budget test in
/// `VarlitDecode::decode_array` is an ordinary runtime `if` over
/// const-foldable operands, so were this body inlined the `[Option<T>; N]` slot
/// would sit in that function's frame whichever branch runs — and a debug
/// build, which is how `cargo test` runs, folds nothing away. Kept as its own
/// non-inlinable function the slot exists only while this function is on the
/// stack, which for an above-budget array is never, in debug and release
/// alike.
///
/// `Option<T>` rather than `MaybeUninit<T>` is what keeps the crate's most
/// adversarially fuzzed surface free of `unsafe`: an early `?` drops the
/// partially filled array through its ordinary `Drop`, the initialised prefix
/// as `Some` and the rest as `None`.
#[inline(never)]
fn decode_array_on_stack<T, const N: usize>(decoder: &mut Decoder<'_>) -> Result<[T; N]>
where
    T: VarlitDecode,
{
    let mut slots: [Option<T>; N] = [const { None }; N];
    for slot in &mut slots {
        *slot = Some(decoder.decode_value::<T>()?);
    }
    Ok(slots.map(|value| value.expect("every array slot was filled by the loop above")))
}

// A byte array has no per-element validity or endian work. Resolve this via
// the actual u8 implementation (including aliases), never via a wire tag that
// a custom codec could also declare. Keep oversized stack frames out of the
// caller just as for the generic array helper.
#[inline(never)]
fn decode_byte_array_on_stack<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[u8; N]> {
    decoder
        .read_exact(N)?
        .try_into()
        .map_err(|_| Error::UnexpectedEof)
}

/// `decode_array` for fixed-width scalars: the default array path's preflight,
/// one input read, and the same stack/heap split, without per-element calls.
fn decode_fixed_array<T: Copy, const W: usize, const N: usize>(
    decoder: &mut Decoder<'_>,
    zero: T,
    little: impl Fn([u8; W]) -> T,
    big: impl Fn([u8; W]) -> T,
) -> Result<[T; N]> {
    match decoder.endian() {
        Endian::Little => decode_fixed_array_with(decoder, zero, little),
        Endian::Big => decode_fixed_array_with(decoder, zero, big),
    }
}

/// [`decode_fixed_array`] for one byte order; `convert` is a concrete,
/// inlinable function so the element loop can be vectorized.
fn decode_fixed_array_with<T: Copy, const W: usize, const N: usize>(
    decoder: &mut Decoder<'_>,
    zero: T,
    convert: impl Fn([u8; W]) -> T,
) -> Result<[T; N]> {
    decoder.preflight_count(N, W, size_of::<T>(), "array")?;
    let bytes = decoder.read_fixed_run(N, W, "array")?;
    let (chunks, _) = bytes.as_chunks::<W>();
    // Same stack/heap boundary as the default `decode_array`, so a declared
    // array length that the default sends to the heap still goes there.
    if size_of::<Option<T>>().saturating_mul(N) <= ARRAY_STACK_DECODE_BUDGET {
        let mut out = [zero; N];
        for (slot, chunk) in out.iter_mut().zip(chunks) {
            *slot = convert(*chunk);
        }
        Ok(out)
    } else {
        let mut values = Vec::new();
        values
            .try_reserve_exact(N)
            .map_err(|_| Error::AllocationFailed {
                resource: "array",
                requested: allocation_request::<T>(N),
            })?;
        values.extend(chunks.iter().map(|chunk| convert(*chunk)));
        values.try_into().map_err(|_| Error::UnexpectedEof)
    }
}

/// Validates canonical booleans (0 or 1) for a run of bytes.
fn check_bool_bytes(bytes: &[u8]) -> Result<()> {
    if bytes.iter().any(|&byte| byte > 1) {
        return Err(Error::InvalidCanonicalEncoding(
            "boolean value must be 0 or 1",
        ));
    }
    Ok(())
}

/// Decodes `[T; N]` through one fallible heap reservation — the behaviour every
/// array decode had before the stack path, kept verbatim for arrays above
/// `ARRAY_STACK_DECODE_BUDGET`.
fn decode_array_on_heap<T, const N: usize>(decoder: &mut Decoder<'_>) -> Result<[T; N]>
where
    T: VarlitDecode,
{
    let mut values = Vec::new();
    values
        .try_reserve_exact(N)
        .map_err(|_| Error::AllocationFailed {
            resource: "array",
            requested: allocation_request::<T>(N),
        })?;
    for _ in 0..N {
        values.push(decoder.decode_value::<T>()?);
    }
    values.try_into().map_err(|_| Error::UnexpectedEof)
}

impl<T, const N: usize> VarlitDecode for [T; N]
where
    T: VarlitDecode,
{
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: matches!(
            T::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ),
    });
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id_with_arity(b"array", N as u64, &[T::SCHEMA_ID]);

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        decoder.with_callback::<ArrayCallback<T, N>>(())
    }
}

impl<K, V> VarlitEncode for BTreeMap<K, V>
where
    K: VarlitEncode + Ord,
    V: VarlitEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"btreemap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        for (key, value) in self {
            key.encode_varlit(encoder)?;
            value.encode_varlit(encoder)?;
        }
        Ok(())
    }
}

impl<K, V> VarlitDecode for BTreeMap<K, V>
where
    K: VarlitDecode + Ord,
    V: VarlitDecode,
{
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: matches!(
            K::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ) && matches!(
            V::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ),
    });
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"btreemap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_map_count::<K, V>(len, "BTreeMap entries")?;
        let mut values = BTreeMap::new();
        if len == 0 {
            return Ok(values);
        }

        let mut pending_key = decoder.decode_value::<K>()?;
        for _ in 1..len {
            let pending_value = decoder.decode_value::<V>()?;
            let next_key = decoder.decode_value::<K>()?;
            match pending_key.cmp(&next_key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
                }
                Ordering::Greater => {
                    return Err(Error::InvalidCanonicalEncoding(
                        "BTreeMap keys must be strictly increasing",
                    ));
                }
            }
            if values.insert(pending_key, pending_value).is_some() {
                return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
            }
            pending_key = next_key;
        }

        let pending_value = decoder.decode_value::<V>()?;
        if values.insert(pending_key, pending_value).is_some() {
            return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
        }
        Ok(values)
    }
}

impl<K, V> VarlitEncode for HashMap<K, V>
where
    K: VarlitEncode + Ord + Eq + Hash,
    V: VarlitEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"hashmap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        // Refuse impossible output before allocating one reference pair per
        // entry. Custom Nested/Seq codecs may be zero-width, so only charge
        // the minimum guaranteed by their declared wire type here.
        if let Some(limit) = encoder.max_len {
            let per_entry =
                minimum_wire_size(K::WIRE_TYPE) as u64 + minimum_wire_size(V::WIRE_TYPE) as u64;
            let actual = (self.len() as u64)
                .checked_mul(per_entry)
                .and_then(|n| n.checked_add(8))
                .and_then(|n| n.checked_add(encoder.output.len() as u64))
                .unwrap_or(u64::MAX);
            if actual > limit {
                return Err(Error::LimitExceeded {
                    resource: encoder.limit_resource,
                    actual,
                    limit,
                });
            }
        }
        let mut entries: Vec<(&K, &V)> = Vec::new();
        entries
            .try_reserve_exact(self.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "HashMap encoding entries",
                requested: allocation_request::<(&K, &V)>(self.len()),
            })?;
        entries.extend(self.iter());
        entries.sort_unstable_by_key(|(key, _)| *key);
        encoder.write_u64(entries.len() as u64);
        for (key, value) in entries {
            key.encode_varlit(encoder)?;
            value.encode_varlit(encoder)?;
        }
        Ok(())
    }
}

impl<K, V> VarlitDecode for HashMap<K, V>
where
    K: VarlitDecode + Ord + Eq + Hash,
    V: VarlitDecode,
{
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: matches!(
            K::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ) && matches!(
            V::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ),
    });
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"hashmap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    #[inline]
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_hash_map_count::<K, V>(len, "HashMap entries")?;
        let mut values = HashMap::new();
        // reserve only a bounded prefix of the declared count, so a
        // hostile count cannot materialize a table before a single entry has
        // been proven decodable. The charge above already bounds the table this
        // map can reach; this bounds what it can reach *on a claim alone*.
        let preallocated = len.min(MAP_PREALLOCATION_ENTRIES);
        values
            .try_reserve(preallocated)
            .map_err(|_| Error::AllocationFailed {
                resource: "HashMap entries",
                requested: allocation_request::<(K, V)>(preallocated),
            })?;
        if len == 0 {
            return Ok(values);
        }

        let mut pending_key = decoder.decode_value::<K>()?;
        for _ in 1..len {
            let pending_value = decoder.decode_value::<V>()?;
            let next_key = decoder.decode_value::<K>()?;
            match pending_key.cmp(&next_key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(Error::InvalidCanonicalEncoding("duplicate HashMap key"));
                }
                Ordering::Greater => {
                    return Err(Error::InvalidCanonicalEncoding(
                        "HashMap keys must be strictly increasing",
                    ));
                }
            }
            insert_hash_map_entry(&mut values, pending_key, pending_value)?;
            pending_key = next_key;
        }

        let pending_value = decoder.decode_value::<V>()?;
        insert_hash_map_entry(&mut values, pending_key, pending_value)?;
        Ok(values)
    }
}

fn insert_hash_map_entry<K: Eq + Hash, V>(
    values: &mut HashMap<K, V>,
    key: K,
    value: V,
) -> Result<()> {
    if values.len() == values.capacity() {
        values.try_reserve(1).map_err(|_| Error::AllocationFailed {
            resource: "HashMap entries",
            requested: allocation_request::<(K, V)>(values.len().saturating_add(1)),
        })?;
    }
    if values.insert(key, value).is_some() {
        return Err(Error::InvalidCanonicalEncoding("duplicate HashMap key"));
    }
    Ok(())
}

macro_rules! vec_seq_codec {
    ($(($ty:ty, $minimum_wire_bytes:expr)),+ $(,)?) => {
        $(
            impl VarlitEncode for Vec<$ty> {
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarlitEncode>::SCHEMA_ID]);

                fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
                    encoder.write_u64(self.len() as u64);
                    <$ty as VarlitEncode>::__varlit_encode_slice(self, encoder)
                }
            }

            impl VarlitDecode for Vec<$ty> {
                const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec { invariant: std::marker::PhantomData, callback_free: true });
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarlitDecode>::SCHEMA_ID]);

                #[inline]
                fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
                    let len = decoder.read_len()?;
                    decoder.preflight_count(
                        len,
                        $minimum_wire_bytes,
                        size_of::<$ty>(),
                        "sequence entries",
                    )?;
                    let mut values = Vec::new();
                    values.try_reserve_exact(len).map_err(|_| Error::AllocationFailed {
                        resource: "sequence entries",
                        requested: allocation_request::<$ty>(len),
                    })?;
                    for _ in 0..len {
                        values.push(decoder.decode_value::<$ty>()?);
                    }
                    Ok(values)
                }

                fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
                    let len = decoder.read_len()?;
                    decoder.preflight_count(
                        len,
                        $minimum_wire_bytes,
                        size_of::<$ty>(),
                        "sequence entries",
                    )?;
                    out.truncate(len);
                    let reused = out.len();
                    out.try_reserve_exact(len - reused).map_err(|_| Error::AllocationFailed {
                        resource: "sequence entries",
                        requested: allocation_request::<$ty>(len),
                    })?;
                    for element in out.iter_mut() {
                        decoder.decode_value_into(element)?;
                    }
                    for _ in reused..len {
                        out.push(decoder.decode_value::<$ty>()?);
                    }
                    Ok(())
                }
            }
        )+
    }
}

/// `Vec<T>` for fixed-width scalars. Same wire bytes, preflight, budget charge
/// and reservation as [`vec_seq_codec`]; the elements are converted from one
/// bounds-checked input slice instead of one decoder call per element.
macro_rules! vec_fixed_codec {
    ($(($ty:ty, $width:expr, $little:expr, $big:expr, $check:expr)),+ $(,)?) => {
        $(
            impl VarlitEncode for Vec<$ty> {
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarlitEncode>::SCHEMA_ID]);

                fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
                    encoder.write_u64(self.len() as u64);
                    <$ty as VarlitEncode>::__varlit_encode_slice(self, encoder)
                }
            }

            impl VarlitDecode for Vec<$ty> {
                const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec { invariant: std::marker::PhantomData, callback_free: true });
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarlitDecode>::SCHEMA_ID]);

                #[inline]
                fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
                    let mut values = Vec::new();
                    Self::fill_fixed(decoder, &mut values)?;
                    Ok(values)
                }

                #[inline]
                fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
                    Self::fill_fixed(decoder, out)
                }
            }

            impl FillFixedVec for Vec<$ty> {
                /// Decodes into `values`, reusing its capacity.
                #[inline]
                fn fill_fixed(decoder: &mut Decoder<'_>, values: &mut Self) -> Result<()> {
                    let len = decoder.read_len()?;
                    // Bounds `len` by the remaining input and charges the budget
                    // before any reservation, exactly as the element-wise codec.
                    decoder.preflight_count(len, $width, size_of::<$ty>(), "sequence entries")?;
                    values.clear();
                    values.try_reserve_exact(len).map_err(|_| Error::AllocationFailed {
                        resource: "sequence entries",
                        requested: allocation_request::<$ty>(len),
                    })?;
                    let bytes = decoder.read_fixed_run(len, $width, "sequence entries")?;
                    let check: fn(&[u8]) -> Result<()> = $check;
                    check(bytes)?;
                    let chunks = bytes.as_chunks::<$width>().0;
                    // Capacity is already reserved: these extends never
                    // reallocate. One loop per byte order, so each converts
                    // with a direct, inlinable call the compiler can vectorize.
                    match decoder.endian() {
                        Endian::Little => values.extend(chunks.iter().map(|chunk| ($little)(*chunk))),
                        Endian::Big => values.extend(chunks.iter().map(|chunk| ($big)(*chunk))),
                    }
                    Ok(())
                }
            }
        )+
    }
}

/// Shared body of the fixed-width `Vec` codecs (fresh and in place).
trait FillFixedVec: Sized {
    fn fill_fixed(decoder: &mut Decoder<'_>, values: &mut Self) -> Result<()>;
}

fn no_check(_: &[u8]) -> Result<()> {
    Ok(())
}

vec_fixed_codec!(
    (
        bool,
        1,
        |b: [u8; 1]| b[0] == 1,
        |b: [u8; 1]| b[0] == 1,
        check_bool_bytes
    ),
    (i8, 1, i8::from_le_bytes, i8::from_be_bytes, no_check),
    (u16, 2, u16::from_le_bytes, u16::from_be_bytes, no_check),
    (i16, 2, i16::from_le_bytes, i16::from_be_bytes, no_check),
    (u32, 4, u32::from_le_bytes, u32::from_be_bytes, no_check),
    (i32, 4, i32::from_le_bytes, i32::from_be_bytes, no_check),
    (u64, 8, u64::from_le_bytes, u64::from_be_bytes, no_check),
    (i64, 8, i64::from_le_bytes, i64::from_be_bytes, no_check),
    (u128, 16, u128::from_le_bytes, u128::from_be_bytes, no_check),
    (i128, 16, i128::from_le_bytes, i128::from_be_bytes, no_check),
    (f32, 4, f32::from_le_bytes, f32::from_be_bytes, no_check),
    (f64, 8, f64::from_le_bytes, f64::from_be_bytes, no_check)
);

vec_seq_codec!((String, 8));

/// A sequence of whole records (`Vec<T>` where `T` is a block type), stored
/// record after record like a compound array.
///
/// Lets an application hand its records to a block field as they are, without
/// splitting them into one vector per field first. Each record uses its own
/// codec; fixed-width members of a record take the bulk scalar paths.
///
/// Decoding charges `len * size_of::<T>()` to the materialization budget
/// before anything is reserved, reserves no more than the remaining input could
/// carry, and grows fallibly from there, so a forged length cannot force a
/// large allocation.
impl<T: crate::VarlitBlock> VarlitEncode for Vec<T> {
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id(b"recordseq", &[<T as VarlitEncode>::SCHEMA_ID]);

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        T::__varlit_encode_slice(self, encoder)
    }
}

impl<T: crate::VarlitBlock> VarlitDecode for Vec<T> {
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec {
        invariant: std::marker::PhantomData,
        callback_free: matches!(
            <T as VarlitDecode>::__VARLIT_BUILTIN,
            Some(BuiltinCodec {
                callback_free: true,
                ..
            })
        ),
    });
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id(b"recordseq", &[<T as VarlitDecode>::SCHEMA_ID]);

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_count(
            len,
            minimum_wire_size(<T as VarlitDecode>::WIRE_TYPE),
            size_of::<T>(),
            "record sequence entries",
        )?;
        let mut values = Vec::new();
        let initial = len.min(decoder.remaining());
        values
            .try_reserve_exact(initial)
            .map_err(|_| Error::AllocationFailed {
                resource: "record sequence entries",
                requested: allocation_request::<T>(initial),
            })?;
        for _ in 0..len {
            if values.len() == values.capacity() {
                values.try_reserve(1).map_err(|_| Error::AllocationFailed {
                    resource: "record sequence entries",
                    requested: allocation_request::<T>(values.len().saturating_add(1)),
                })?;
            }
            values.push(decoder.decode_value::<T>()?);
        }
        Ok(values)
    }

    fn decode_varlit_into(out: &mut Self, decoder: &mut Decoder<'_>) -> Result<()> {
        let len = decoder.read_len()?;
        decoder.preflight_count(
            len,
            minimum_wire_size(<T as VarlitDecode>::WIRE_TYPE),
            size_of::<T>(),
            "record sequence entries",
        )?;
        out.truncate(len);
        for element in out.iter_mut() {
            decoder.decode_value_into(element)?;
        }
        for _ in out.len()..len {
            if out.len() == out.capacity() {
                // Grow by what the remaining input could carry, never by the
                // claimed count alone.
                let step = (len - out.len()).min(decoder.remaining().max(1));
                out.try_reserve(step).map_err(|_| Error::AllocationFailed {
                    resource: "record sequence entries",
                    requested: allocation_request::<T>(out.len().saturating_add(step)),
                })?;
            }
            out.push(decoder.decode_value::<T>()?);
        }
        Ok(())
    }
}

const fn minimum_wire_size(wire_type: WireType) -> usize {
    match wire_type {
        WireType::Unit => 0,
        WireType::Bool | WireType::U8 | WireType::I8 => 1,
        WireType::U16 | WireType::I16 => 2,
        WireType::U32 | WireType::I32 | WireType::F32 => 4,
        WireType::U64 | WireType::I64 | WireType::F64 => 8,
        WireType::U128 | WireType::I128 => 16,
        WireType::Bytes | WireType::String => 8,
        WireType::Seq | WireType::Nested => 0,
    }
}

fn allocation_request<T>(len: usize) -> u64 {
    u64::try_from(len.saturating_mul(size_of::<T>())).unwrap_or(u64::MAX)
}

/// Control-group width the reservation model assumes for hashbrown, in bytes.
///
/// This single constant governs three separate parts of the real allocation,
/// all of which scale with `Group::WIDTH`:
///
/// * the trailing duplicated control group appended after the control array;
/// * the minimum bucket count of a small table (: hashbrown rounds a
///   small table with a one-byte-or-smaller entry up to a whole group so the
///   control array is never shorter than one SIMD load);
/// * the control array's alignment, which is `max(Group::WIDTH, align_of::<T>())`
///   and therefore bounds the padding inserted between the bucket array and the
///   control array.
///
/// Shipping hashbrown uses a 16-byte SSE2 group on x86-64 and an 8-byte group
/// on NEON and the generic fallback, so the true width on every target this
/// project supports today is at most 16. The model deliberately assumes **32**
/// instead: the value cannot be read from the standard library at compile time,
/// a wider group is the only way this model can silently start undercharging,
/// and one doubling buys immunity to a future 32-byte-group implementation at a
/// cost of at most a few tens of bytes per map plus a bounded over-charge on
/// tables of fewer than fifteen entries. Charging more than hashbrown allocates
/// is always sound here; charging less is the defect this constant exists to
/// prevent.
/// Entry-slot multiplier for the `BTreeMap` materialization model.
///
/// A std B-tree node allocates a fixed array of `2B - 1 = 11` entry slots
/// whatever its fill, and is only guaranteed to hold `B - 1 = 5` of them, so
/// the array costs up to `11/5 = 2.2` slots per live entry. Internal nodes add
/// a further `1/(B - 1)` of the leaf population. Three is the next integer
/// above that bound and is chosen so a future change of `B` in the standard
/// library cannot silently turn the model into an under-charge.
const BTREE_ENTRY_SLOT_FACTOR: usize = 3;

/// Per-entry allowance for B-tree node headers and child pointers.
///
/// A leaf node header is a parent pointer, a parent index and a length; an
/// internal node adds `2B = 12` child pointers. Amortised over the guaranteed
/// minimum fill of five entries that is under four bytes per entry for leaves
/// and under twenty for internal nodes, of which there are at most a fifth as
/// many. Sixteen bytes per entry covers both comfortably.
const BTREE_ENTRY_HEADER_BYTES: usize = 16;

const HASH_TABLE_GROUP_BYTES: u64 = 32;

/// Entries a map codec reserves for before it has validated a single entry
///
///
/// The declared count is attacker-chosen, so a table sized from it up front is
/// an allocation granted on nothing but a claim. Reserving a small fixed prefix
/// instead keeps the common small-map decode allocation-exact while leaving a
/// hostile count nothing to pre-allocate; beyond this bound the table grows
/// through `insert`'s own amortized doubling, which performs no syscalls and no
/// per-entry allocation.
const MAP_PREALLOCATION_ENTRIES: usize = 1024;

fn hash_map_decode_reservation_bytes<T>(len: usize) -> Option<u64> {
    // The bounded upfront reservation guarantees no growth for small maps.
    let copies = if len > MAP_PREALLOCATION_ENTRIES {
        2
    } else {
        1
    };
    hash_table_reservation_bytes::<T>(len)?.checked_mul(copies)
}

/// Upper bound on the heap a `HashMap<K, V>` allocates when asked to hold `len`
/// entries, with `T = (K, V)`.
///
/// Derivation, from what hashbrown actually allocates:
///
/// * `try_reserve(len)` on an empty map requests a table with capacity `len`.
/// * Entries live at a maximum 7/8 load factor, so a large table needs at least
///   `ceil(len * 8 / 7)` buckets (hashbrown itself floors that division; the
///   ceiling used here can only round the model up).
/// * The bucket count is rounded **up to a power of two**, which can almost
///   double that figure again.
/// * Below that, hashbrown does **not** allocate less: it applies *small-table
///   capacity classes* with a hard floor. On the review toolchain (Rust 1.95,
///   x86-64) an empty `HashMap` asked for any capacity in `1..=14` reports
///   capacity 14, i.e. a **16-bucket** table, for a zero-sized or one-byte
///   entry; larger entries floor at 4 or 8 buckets. This is why the previous
///   comment here — that "small-table specializations only ever allocate less"
///   — was false, and why the old two-bucket model undercharged
///   `HashMap<(), ()>` by charging 19 bytes for a table that really costs 32
///   bytes of control storage alone.
/// * Every bucket owns one `T` *and* one control byte — the true floor is never
///   one byte per entry, not even for a zero-sized `T`.
/// * The allocation additionally carries one trailing duplicated control group,
///   plus the padding that aligns the control array, which is bounded by
///   `max(Group::WIDTH, align_of::<T>())` rather than by `align_of::<T>()`
///   alone (the old model charged only the latter and could undercharge a
///   low-alignment entry by up to a group).
///
/// So the charge is
///
/// ```text
/// buckets = max(next_power_of_two(ceil(len * 8 / 7)), HASH_TABLE_GROUP_BYTES)
/// bytes   = buckets * (size_of::<T>() + 1)
///         + HASH_TABLE_GROUP_BYTES
///         + max(HASH_TABLE_GROUP_BYTES, align_of::<T>())
/// ```
///
/// The bucket floor is *not* a transcription of one observed capacity class: it
/// is [`HASH_TABLE_GROUP_BYTES`], deliberately set to a wider control group
/// (32) than any supported toolchain uses (8 or 16). Every small-table class
/// hashbrown can pick is either a fixed small count (4 or 8 buckets) or one
/// whole control group, so on any implementation whose group is at most 32 wide
/// the class is at most 32 buckets and the floor dominates it — for `len <= 14`
/// this is decided by that floor alone, without hard-coding what any one build
/// was observed to do. From `len = 15` upwards the power-of-two term takes
/// over, and it is already at least 32 there.
///
/// The result is therefore an over-estimate at every capacity, MSRV 1.95 and
/// later stable toolchains alike, and the guard must never charge less than the
/// reservation it guards. `None` means the model overflowed `u64`, which the
/// caller must report as a budget rejection rather than attempt.
pub(crate) fn hash_table_reservation_bytes<T>(len: usize) -> Option<u64> {
    if len == 0 {
        return Some(0);
    }
    let len = u64::try_from(len).ok()?;
    let buckets = len
        .checked_mul(8)?
        .checked_add(6)?
        .checked_div(7)?
        .checked_next_power_of_two()?
        .max(HASH_TABLE_GROUP_BYTES);
    buckets
        .checked_mul((size_of::<T>() as u64).checked_add(1)?)?
        .checked_add(HASH_TABLE_GROUP_BYTES)?
        .checked_add(HASH_TABLE_GROUP_BYTES.max(align_of::<T>() as u64))
}

macro_rules! tuple_codec {
    ($($name:ident),+) => {
        impl<$($name),+> VarlitEncode for ($($name,)+)
        where
            $($name: VarlitEncode),+
        {
            const WIRE_TYPE: WireType = WireType::Nested;
            const SCHEMA_ID: u64 =
                container_schema_id(b"tuple", &[$(<$name as VarlitEncode>::SCHEMA_ID),+]);

            #[allow(non_snake_case)]
            fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
                let ($($name,)+) = self;
                $($name.encode_varlit(encoder)?;)+
                Ok(())
            }
        }

        impl<$($name),+> VarlitDecode for ($($name,)+)
        where
            $($name: VarlitDecode),+
        {
            const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = Some(BuiltinCodec { invariant: std::marker::PhantomData, callback_free: true $(&& matches!($name::__VARLIT_BUILTIN, Some(BuiltinCodec { callback_free:true, .. })))+ });
            const WIRE_TYPE: WireType = WireType::Nested;
            const SCHEMA_ID: u64 =
                container_schema_id(b"tuple", &[$(<$name as VarlitDecode>::SCHEMA_ID),+]);

            #[inline(always)]
            fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
                Ok(($(decoder.decode_value::<$name>()?,)+))
            }
        }
    };
}

tuple_codec!(A, B);
tuple_codec!(A, B, C);
tuple_codec!(A, B, C, D);

#[cfg(test)]
mod tests {
    use super::*;

    // ---- bulk fixed-width scalar paths vs. the element-wise reference ----

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state ^ (*state >> 29)
    }

    /// Reference encoding: the length prefix plus one scalar call per element,
    /// which is exactly what the codec did before the bulk path.
    fn elementwise_vec<T: VarlitEncode>(values: &[T], endian: Endian) -> Vec<u8> {
        let mut encoder = Encoder::new_limited(endian, u64::MAX, "encoded payload");
        encoder.write_u64(values.len() as u64);
        for value in values {
            value.encode_varlit(&mut encoder).unwrap();
        }
        encoder.try_into_inner().unwrap()
    }

    fn elementwise_decode<T: VarlitDecode>(bytes: &[u8], endian: Endian) -> Result<Vec<T>> {
        let mut decoder = Decoder::new(bytes, endian);
        let len = decoder.read_len()?;
        let mut values = Vec::new();
        for _ in 0..len {
            values.push(decoder.decode_value::<T>()?);
        }
        Ok(values)
    }

    fn check_vec<T>(values: Vec<T>, same: impl Fn(&T, &T) -> bool)
    where
        T: VarlitEncode + VarlitDecode + std::fmt::Debug,
        Vec<T>: VarlitEncode + VarlitDecode,
    {
        for endian in [Endian::Little, Endian::Big] {
            let bulk = encode_to_vec(&values, endian).unwrap();
            assert_eq!(
                bulk,
                elementwise_vec(&values, endian),
                "encoded bytes differ"
            );
            let decoded: Vec<T> = decode_from_slice(&bulk, endian).unwrap();
            let reference: Vec<T> = elementwise_decode(&bulk, endian).unwrap();
            assert_eq!(decoded.len(), values.len());
            for ((a, b), c) in decoded.iter().zip(&values).zip(&reference) {
                assert!(same(a, b) && same(a, c), "decoded {a:?} expected {b:?}");
            }
        }
    }

    #[test]
    fn bulk_vec_codecs_match_elementwise_bytes_and_values() {
        let mut state = 0x5EED;
        for len in [0usize, 1, 2, 3, 7, 64, 1000, 5000] {
            let mut r = || lcg(&mut state);
            check_vec(
                (0..len).map(|_| r() & 1 == 1).collect::<Vec<bool>>(),
                |a, b| a == b,
            );
            check_vec((0..len).map(|_| r() as i8).collect::<Vec<i8>>(), |a, b| {
                a == b
            });
            check_vec(
                (0..len).map(|_| r() as u16).collect::<Vec<u16>>(),
                |a, b| a == b,
            );
            check_vec(
                (0..len).map(|_| r() as i16).collect::<Vec<i16>>(),
                |a, b| a == b,
            );
            check_vec(
                (0..len).map(|_| r() as u32).collect::<Vec<u32>>(),
                |a, b| a == b,
            );
            check_vec(
                (0..len).map(|_| r() as i32).collect::<Vec<i32>>(),
                |a, b| a == b,
            );
            check_vec((0..len).map(|_| r()).collect::<Vec<u64>>(), |a, b| a == b);
            check_vec(
                (0..len).map(|_| r() as i64).collect::<Vec<i64>>(),
                |a, b| a == b,
            );
            check_vec(
                (0..len)
                    .map(|_| (r() as u128) << 64 | r() as u128)
                    .collect::<Vec<u128>>(),
                |a, b| a == b,
            );
            check_vec(
                (0..len)
                    .map(|_| ((r() as u128) << 64 | r() as u128) as i128)
                    .collect::<Vec<i128>>(),
                |a, b| a == b,
            );
            // Arbitrary bit patterns, including NaN payloads, must survive exactly.
            check_vec(
                (0..len)
                    .map(|_| f32::from_bits(r() as u32))
                    .collect::<Vec<f32>>(),
                |a, b| a.to_bits() == b.to_bits(),
            );
            check_vec(
                (0..len).map(|_| f64::from_bits(r())).collect::<Vec<f64>>(),
                |a, b| a.to_bits() == b.to_bits(),
            );
        }
        check_vec(vec![i64::MIN, -1, 0, i64::MAX], |a, b| a == b);
        check_vec(vec![i8::MIN, -1, 0, i8::MAX], |a, b| a == b);
    }

    fn check_array<T, const N: usize>(values: [T; N], same: impl Fn(&T, &T) -> bool)
    where
        T: VarlitEncode + VarlitDecode + std::fmt::Debug,
    {
        for endian in [Endian::Little, Endian::Big] {
            let bulk = encode_to_vec(&values, endian).unwrap();
            let mut reference = Encoder::new_limited(endian, u64::MAX, "encoded payload");
            for value in &values {
                value.encode_varlit(&mut reference).unwrap();
            }
            assert_eq!(
                bulk,
                reference.try_into_inner().unwrap(),
                "array bytes differ"
            );
            let decoded: Box<[T; N]> = Box::new(decode_from_slice(&bulk, endian).unwrap());
            for (a, b) in decoded.iter().zip(&values) {
                assert!(same(a, b), "decoded {a:?} expected {b:?}");
            }
        }
    }

    #[test]
    fn bulk_array_codecs_match_elementwise_on_both_sides_of_the_stack_budget() {
        let mut state = 7;
        let mut r = || lcg(&mut state);
        check_array([0u16; 0], |a, b| a == b);
        check_array(
            std::array::from_fn::<u16, 500, _>(|_| r() as u16),
            |a, b| a == b,
        );
        check_array(std::array::from_fn::<i8, 10, _>(|_| r() as i8), |a, b| {
            a == b
        });
        check_array(
            std::array::from_fn::<bool, 3, _>(|_| r() & 1 == 1),
            |a, b| a == b,
        );
        check_array(std::array::from_fn::<u8, 33, _>(|_| r() as u8), |a, b| {
            a == b
        });
        // The boundary counts `Option<T>` slots, as the default path does:
        // 2048 x Option<u32> is exactly 16 KiB; one more goes to the heap.
        check_array(
            *Box::new(std::array::from_fn::<u32, 2048, _>(|_| r() as u32)),
            |a, b| a == b,
        );
        check_array(
            *Box::new(std::array::from_fn::<u32, 2049, _>(|_| r() as u32)),
            |a, b| a == b,
        );
        check_array(
            *Box::new(std::array::from_fn::<f32, 16384, _>(|_| {
                f32::from_bits(r() as u32)
            })),
            |a, b| a.to_bits() == b.to_bits(),
        );
        check_array(
            *Box::new(std::array::from_fn::<bool, 20000, _>(|_| r() & 1 == 1)),
            |a, b| a == b,
        );
        check_array(
            std::array::from_fn::<f64, 7, _>(|_| f64::from_bits(r())),
            |a, b| a.to_bits() == b.to_bits(),
        );
    }

    #[test]
    fn bulk_encode_reproduces_elementwise_limit_failures_and_prefixes() {
        let values: Vec<u32> = (0..50).collect();
        let full = 8 + 50 * 4;
        for limit in 0..=(full as u64 + 3) {
            for endian in [Endian::Little, Endian::Big] {
                let mut bulk =
                    Encoder::new_limited_with_buffer(endian, limit, "encoded payload", Vec::new());
                let bulk_result = values.encode_varlit(&mut bulk);
                let (bulk_bytes, bulk_finished) = bulk.into_buffer_and_result();

                let mut reference =
                    Encoder::new_limited_with_buffer(endian, limit, "encoded payload", Vec::new());
                reference.write_u64(values.len() as u64);
                for value in &values {
                    value.encode_varlit(&mut reference).unwrap();
                }
                let (reference_bytes, reference_finished) = reference.into_buffer_and_result();

                assert!(bulk_result.is_ok());
                assert_eq!(
                    bulk_bytes, reference_bytes,
                    "prefix differs at limit {limit}"
                );
                match (bulk_finished, reference_finished) {
                    (Ok(()), Ok(())) => assert!(limit >= full as u64),
                    (
                        Err(Error::LimitExceeded {
                            actual: a,
                            limit: la,
                            ..
                        }),
                        Err(Error::LimitExceeded {
                            actual: b,
                            limit: lb,
                            ..
                        }),
                    ) => assert_eq!((a, la), (b, lb), "limit failure differs at {limit}"),
                    other => panic!("mismatched outcomes at limit {limit}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn bulk_decode_keeps_input_and_budget_guards_before_reserving() {
        // A length claim larger than the remaining input fails before reserving.
        let mut claim = Vec::new();
        claim.extend_from_slice(&u64::MAX.to_le_bytes());
        claim.extend_from_slice(&[0; 6]);
        assert!(matches!(
            decode_from_slice::<Vec<u16>>(&claim, Endian::Little),
            Err(Error::UnexpectedEof | Error::LengthOverflow { .. })
        ));
        // Truncated by one element.
        let mut bytes = encode_to_vec(&vec![1u32, 2, 3], Endian::Little).unwrap();
        bytes.pop();
        assert!(matches!(
            decode_from_slice::<Vec<u32>>(&bytes, Endian::Little),
            Err(Error::UnexpectedEof)
        ));
        // A budget below the materialized size is refused before allocation.
        let big = encode_to_vec(&vec![0u64; 1024], Endian::Little).unwrap();
        assert!(matches!(
            Decoder::decode_from_slice_limited::<Vec<u64>>(&big, Endian::Little, 1024 * 8 - 1),
            Err(Error::LimitExceeded { .. })
        ));
        assert_eq!(
            Decoder::decode_from_slice_limited::<Vec<u64>>(&big, Endian::Little, 1024 * 8)
                .unwrap()
                .len(),
            1024
        );
        // Array input shorter than N.
        let short = encode_to_vec(&[1u16; 4], Endian::Little).unwrap();
        assert!(matches!(
            decode_from_slice::<[u16; 5]>(&short, Endian::Little),
            Err(Error::UnexpectedEof)
        ));
    }

    /// Replacing the decoder from inside an in-place decode is rejected the
    /// same way as from inside a value decode, whether the codec keeps the
    /// default `decode_varlit_into` or overrides it.
    #[test]
    fn in_place_decode_rejects_decoder_replacement() {
        struct ReplaceInValue;
        impl VarlitDecode for ReplaceInValue {
            const WIRE_TYPE: WireType = WireType::Nested;
            fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
                *d = Decoder::new_limited(&[], Endian::Little, u64::MAX);
                Ok(Self)
            }
        }
        struct ReplaceInInto;
        impl VarlitDecode for ReplaceInInto {
            const WIRE_TYPE: WireType = WireType::Nested;
            fn decode_varlit(_d: &mut Decoder<'_>) -> Result<Self> {
                Ok(Self)
            }
            fn decode_varlit_into(_out: &mut Self, d: &mut Decoder<'_>) -> Result<()> {
                *d = Decoder::new_limited(&[], Endian::Little, u64::MAX);
                Ok(())
            }
        }
        let mut parent = Decoder::new_limited(&[], Endian::Little, 100);
        let mut out = ReplaceInValue;
        assert!(matches!(
            parent.decode_nested_into(&[], &mut out),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        assert_eq!(parent.materialization_remaining(), 0);
        let mut parent = Decoder::new_limited(&[], Endian::Little, 100);
        let mut out = ReplaceInInto;
        assert!(matches!(
            parent.decode_nested_into(&[], &mut out),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        assert_eq!(parent.materialization_remaining(), 0);
        // A well-behaved in-place decode keeps the unspent budget.
        let bytes = encode_to_vec(&vec![1u32, 2, 3], Endian::Little).unwrap();
        let mut parent = Decoder::new_limited(&[], Endian::Little, 100);
        let mut out: Vec<u32> = Vec::new();
        parent.decode_nested_into(&bytes, &mut out).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(parent.materialization_remaining(), 100 - 12);
    }

    #[test]
    fn bulk_bool_decode_rejects_non_canonical_bytes() {
        let mut vec_bytes = encode_to_vec(&vec![true, false, true], Endian::Little).unwrap();
        *vec_bytes.last_mut().unwrap() = 2;
        assert!(matches!(
            decode_from_slice::<Vec<bool>>(&vec_bytes, Endian::Little),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        let mut small = encode_to_vec(&[false; 3], Endian::Little).unwrap();
        small[1] = 0xFF;
        assert!(matches!(
            decode_from_slice::<[bool; 3]>(&small, Endian::Little),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        let mut large = encode_to_vec(&*Box::new([true; 20000]), Endian::Little).unwrap();
        large[19999] = 3;
        assert!(matches!(
            decode_from_slice::<[bool; 20000]>(&large, Endian::Little),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
    }

    #[test]
    fn encoder_preserves_public_auto_traits_and_catch_unwind_usage() {
        fn assert_traits<
            T: Send + Sync + Unpin + std::panic::UnwindSafe + std::panic::RefUnwindSafe,
        >() {
        }
        assert_traits::<Encoder>();

        let encoder = Encoder::new(Endian::Little);
        assert_eq!(
            std::panic::catch_unwind(|| encoder.endian()).unwrap(),
            Endian::Little
        );
        assert!(
            std::panic::catch_unwind(move || encoder.into_inner())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn limited_encoder_stops_before_the_over_limit_allocation() {
        let mut encoder = Encoder::new_limited(Endian::Little, 8, "test payload");
        encoder.write_all(&[1; 8]);
        encoder.write_all(&[2; 1024]);
        encoder.write_all(&[3; 1024]);
        assert_eq!(encoder.output.len(), 8);
        assert!(matches!(
            encoder.try_into_inner(),
            Err(Error::LimitExceeded {
                resource: "test payload",
                actual: 1032,
                limit: 8,
            })
        ));
    }

    #[test]
    fn nested_encode_inherits_the_parent_remaining_budget() {
        let mut encoder = Encoder::new_limited(Endian::Little, 16, "test payload");
        encoder.write_all(&[1; 4]);
        // 12 bytes of budget remain; a Vec<u8> encodes as an 8-byte length
        // prefix plus its bytes, so a 4-byte nested value fits exactly ...
        let nested = encoder
            .encode_nested_to_vec(&vec![2u8; 4])
            .expect("nested value within the remaining budget");
        assert_eq!(nested.len(), 12);
        // .. while a value larger than the remaining budget is refused with
        // the parent's typed resource before it is buffered.
        let error = encoder
            .encode_nested_to_vec(&vec![3u8; 1024])
            .expect_err("nested value beyond the remaining budget");
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "test payload",
                limit: 12,
                ..
            }
        ));

        // An unlimited parent still yields an unlimited child.
        let unlimited = Encoder::new(Endian::Little);
        let payload = unlimited
            .encode_nested_to_vec(&vec![4u8; 1024])
            .expect("unlimited nested encode");
        assert_eq!(payload.len(), 8 + 1024);
    }

    /// Smallest table the *running* standard library can be shown to allocate
    /// for `try_reserve(len)` on an empty map, in bytes.
    ///
    /// The allocation size is not observable, so this reconstructs a lower
    /// bound from the one number that is: the reported capacity. A hashbrown
    /// table always has a power-of-two bucket count and always keeps at least
    /// one bucket free, so `buckets >= next_power_of_two(capacity + 1)`. Each
    /// bucket owns one `T` and one control byte, and the control array carries
    /// one trailing duplicated group (16 bytes for the widest group any
    /// currently supported target uses).
    ///
    /// This is deliberately a *lower* bound on the real allocation: the model
    /// under test must dominate it at every capacity, and a toolchain whose
    /// small-table classes grew past the model would fail this assertion
    /// instead of silently undercharging.
    fn observed_table_floor_bytes<K, V>(len: usize) -> u64
    where
        K: Hash + Eq,
    {
        let mut map: HashMap<K, V> = HashMap::new();
        map.try_reserve(len).expect("probe reservation");
        let buckets = (map.capacity() as u64 + 1).next_power_of_two();
        buckets * (size_of::<(K, V)>() as u64 + 1) + 16
    }

    /// The reservation model must never charge less than the
    /// table `try_reserve` really allocates, at *every* capacity — including
    /// the small-table classes `1..=14`, where the previous two-bucket model
    /// charged `HashMap<(), ()>` 19 bytes for a 16-bucket table.
    #[test]
    fn hash_table_model_dominates_the_real_reservation_at_every_capacity() {
        macro_rules! check {
            ($k:ty, $v:ty) => {
                for len in 1..=64usize {
                    let modelled =
                        hash_table_reservation_bytes::<($k, $v)>(len).expect("model fits in u64");
                    let observed = observed_table_floor_bytes::<$k, $v>(len);
                    assert!(
                        modelled >= observed,
                        "{}/{} at len {len}: charged {modelled} < real floor {observed}",
                        stringify!($k),
                        stringify!($v),
                    );
                }
            };
        }

        check!((), ());
        check!(u8, ());
        check!(u8, u8);
        check!(u16, u32);
        check!(u64, u64);
        check!(String, Vec<u8>);
        check!([u8; 96], [u8; 96]);
    }

    /// The model is monotonic in the declared count and never zero for a
    /// non-empty map: a budget guard that could be made cheaper by declaring
    /// *more* entries would be defeatable by inflating the count.
    #[test]
    fn hash_table_model_is_monotonic_and_never_free() {
        let mut previous = 0u64;
        for len in 1..=4096usize {
            let charge = hash_table_reservation_bytes::<(u32, u32)>(len).expect("model fits");
            assert!(charge > 0, "len {len} charged nothing");
            assert!(
                charge >= previous,
                "len {len} charged less than len {}",
                len - 1
            );
            previous = charge;
        }
        assert_eq!(hash_table_reservation_bytes::<((), ())>(0), Some(0));
        // A count whose table cannot be represented is a rejection, never a
        // wrap-around to a small charge. On targets where the arithmetic still
        // fits, the charge must at least exceed the declared count.
        match hash_table_reservation_bytes::<(u64, u64)>(usize::MAX) {
            None => {}
            Some(charge) => assert!(charge >= usize::MAX as u64),
        }
    }
}

#[cfg(test)]
mod field_guard_tests {
    use super::*;
    #[test]
    fn zero_field_id_is_refused_by_both_codec_directions() {
        let mut encoder = Encoder::new(Endian::Little);
        assert!(matches!(
            write_field(&mut encoder, 0, WireType::U8, &[1]),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        let mut bytes = [0u8; 17];
        bytes[4..6].copy_from_slice(&(WireType::U8 as u16).to_le_bytes());
        bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
        let mut decoder = Decoder::new(&bytes, Endian::Little);
        assert!(matches!(
            read_field_header(&mut decoder),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
    }
    #[test]
    fn field_set_budget_covers_growth_and_rehash() {
        let bytes = [];
        let mut decoder = Decoder::new_limited(&bytes, Endian::Little, 1 << 20);
        for id in 64..10064 {
            decoder.note_field_id(id).unwrap();
            let count = (id - 63) as usize;
            let charged = (1 << 20) - decoder.materialization_remaining();
            assert_eq!(
                charged,
                2 * hash_table_reservation_bytes::<u32>(count).unwrap()
            );
        }
        let mut small = Decoder::new_limited(&bytes, Endian::Little, 8);
        assert!(matches!(
            small.note_field_id(64),
            Err(Error::LimitExceeded { .. })
        ));
        assert!(small.large_field_ids.is_none());
    }
}

#[cfg(kani)]
#[path = "verification/decoder.rs"]
mod resource_proofs;

#[cfg(kani)]
#[path = "verification/decoder_callers.rs"]
pub(crate) mod caller_proofs;
