use std::hash::Hash;
use std::marker::PhantomData;

use crate::{BlockKind, Endian, FieldDescriptor, Result, StratavaDecode, StratavaEncode};

pub trait StratavaBlock: StratavaEncode + StratavaDecode {
    const ID: u32;
    const VERSION: u16;
    const KIND: BlockKind;
    const ENDIAN: Option<Endian>;
    /// Require a durable boundary after each append or deletion of this type.
    /// This is a writer policy, not part of the on-disk schema identity.
    const IMMEDIATE: bool = false;

    /// An optional value-dependent Immediate condition, ORed with `IMMEDIATE`.
    /// Called before publication; implementations should be pure.
    fn immediate_if(&self) -> bool {
        false
    }
    /// Whether this block has a generated logical key.
    ///
    /// Generated blocks set this exactly. Manual implementations must declare
    /// it explicitly so keyedness cannot silently
    /// default to the chain-unsafe value.
    const IS_KEYED: bool;
    /// Process-local identity of this block's declared schema.
    ///
    /// `#[derive(StratavaBlock)]` computes this deterministically (FNV-1a 64 over
    /// the canonical schema: id, version, kind, endian, keyedness, and ordered
    /// field name/type identities). Typed registration rejects two
    /// implementations that claim the same block id with different
    /// fingerprints, so a manual implementation cannot impersonate a
    /// registered type by matching only id/version/kind. Manual
    /// implementations mirroring a generated block should reuse that block's
    /// const instead of inventing a value. This is deliberately not part of
    /// the wire format or on-disk descriptors.
    const SCHEMA_FINGERPRINT: u64;
    /// Immutable registration contract for manual `FormatSpec` declarations.
    /// Pass a static slice of these values to `with_block_identities`.
    const IDENTITY: (u32, Option<Endian>, bool, u64) = (
        Self::ID,
        Self::ENDIAN,
        Self::IS_KEYED,
        Self::SCHEMA_FINGERPRINT,
    );
    const FIELDS: &'static [FieldDescriptor] = &[];
}

/// Compile-time proof that a keyed implementation agrees with its declared
/// [`StratavaBlock::IS_KEYED`] value.
///
/// Keyed-only generic entry points evaluate [`KeyedBlockContract::OK`], which
/// turns `impl StratavaKeyedBlock` + `IS_KEYED = false` into a
/// post-monomorphization compile error at every keyed use site instead of a
/// silent index-consistency hazard.
pub struct KeyedBlockContract<T: StratavaKeyedBlock>(PhantomData<T>);

impl<T: StratavaKeyedBlock> KeyedBlockContract<T> {
    pub const OK: () = assert!(
        T::IS_KEYED,
        "this type implements StratavaKeyedBlock but declares StratavaBlock::IS_KEYED = false; \
         keyed blocks must declare IS_KEYED = true"
    );
}

/// Opts a block into sequence-preserving copy-on-write replacement.
///
/// Implementations may reject a replacement before any new generation is
/// published. Generated keyed blocks use this hook to require equal keys.
pub trait StratavaReplaceBlock: StratavaBlock {
    fn validate_replacement(old: &Self, new: &Self) -> Result<()>;
}

pub trait StratavaKey:
    Eq + Hash + Clone + StratavaEncode + StratavaDecode + Send + Sync + 'static
{
}

impl<T> StratavaKey for T where
    T: Eq + Hash + Clone + StratavaEncode + StratavaDecode + Send + Sync + 'static
{
}

pub trait StratavaKeyedBlock: StratavaBlock {
    type Key: StratavaKey;

    fn key(&self) -> Self::Key;

    /// Whether this value's key equals `other`, without building the key.
    ///
    /// [`key`](Self::key) clones every key field, so asking a record "is this
    /// your key?" allocated once per heap-typed key field and dropped the
    /// answer immediately. On the disk-index verify path that is per candidate
    /// record, for a value that is thrown away when it matches.
    ///
    /// The default is the old behaviour, so a hand-written implementation that
    /// predates this method keeps compiling and keeps working. `stratava_format!`
    /// generates a field-by-field override that borrows.
    fn key_eq(&self, other: &Self::Key) -> bool {
        &self.key() == other
    }
}

pub trait StratavaMatrixBlock: StratavaBlock {
    const DIMENSIONS: [&'static str; 2];
    const CATEGORY: &'static str;
    const SLOT_STRIDE: u64;
}

pub trait StratavaMigration<From, To>
where
    From: StratavaBlock,
    To: StratavaBlock,
{
    fn migrate(from: From) -> crate::Result<To>;
}

#[cfg(feature = "zero-copy")]
/// Raw fixed blocks can be viewed directly from mmap-backed payload bytes.
///
/// # Safety
///
/// Implementors must guarantee that every payload for this block id/version is
/// a valid immutable instance of `Self` in Rust's raw memory layout, using
/// `RAW_ENDIAN`, `size_of::<Self>()`, and `align_of::<Self>()`. This is not the
/// same contract as Stratava's canonical fixed encoding.
pub unsafe trait StratavaRawFixedBlock:
    StratavaBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout
{
    const RAW_ENDIAN: Endian;
}

#[cfg(feature = "zero-copy")]
/// Raw matrix blocks can be viewed directly from mmap-backed fixed-stride slots.
///
/// # Safety
///
/// Implementors must guarantee that every committed slot payload for this block
/// id/version/category is a valid immutable instance of `Self` in Rust's raw
/// memory layout, using `RAW_ENDIAN`, `size_of::<Self>()`, and
/// `align_of::<Self>()`. This is an explicit opt-in contract and is separate
/// from Stratava's normal canonical matrix decoding.
pub unsafe trait StratavaRawMatrixBlock:
    StratavaMatrixBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout
{
    const RAW_ENDIAN: Endian;
}
