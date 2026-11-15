//! Module shell so `super::wal` and `crate::...` resolve for geometry.rs.
pub(crate) mod wal {
    pub(crate) const MIN_RING: u64 = 64 * 1024;
    pub(crate) const MAX_RING_BYTES: u64 = 64 << 20;
}
#[path = "../../../crates/varlit-core/src/matrix_tiles/geometry.rs"]
pub(crate) mod geometry;
