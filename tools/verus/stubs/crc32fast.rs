//! Stand-in for the crc32fast crate, so geometry.rs type-checks under Verus.
pub fn hash(_bytes: &[u8]) -> u32 {
    unimplemented!()
}
