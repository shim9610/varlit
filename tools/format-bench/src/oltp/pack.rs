//! Application packing of accounts into fixed 44-byte slots, 256 per group,
//! for Varlit layouts that store groups instead of one record per account.
//! app-built: account packing (44-byte slots, 256 per group)
use super::*;

pub const G: usize = 256;
pub const AB: usize = 44; // branch u32 | balance i64 | name [u8;24] | created u64

pub fn ngroups() -> usize {
    (accounts() as usize).div_ceil(G)
}
pub fn enc(id: u64, balance: i64, out: &mut [u8]) {
    let r = initial_row(id);
    out[0..4].copy_from_slice(&r.branch.to_le_bytes());
    out[4..12].copy_from_slice(&balance.to_le_bytes());
    out[12..36].copy_from_slice(&r.name);
    out[36..44].copy_from_slice(&r.created.to_le_bytes());
}
pub fn dec(id: u64, b: &[u8]) -> Row {
    Row {
        id,
        branch: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        balance: i64::from_le_bytes(b[4..12].try_into().unwrap()),
        name: b[12..36].try_into().unwrap(),
        created: u64::from_le_bytes(b[36..44].try_into().unwrap()),
    }
}
pub fn bal_at(group: &[u8], slot: usize) -> i64 {
    i64::from_le_bytes(group[slot * AB + 4..slot * AB + 12].try_into().unwrap())
}
pub fn add_bal(group: &mut [u8], slot: usize, d: i64) {
    let v = bal_at(group, slot) + d;
    group[slot * AB + 4..slot * AB + 12].copy_from_slice(&v.to_le_bytes());
}
pub fn initial_group(g: usize, out: &mut [u8]) {
    out.fill(0);
    for slot in 0..G {
        let id = (g * G + slot) as u64;
        if id < accounts() {
            enc(id, START, &mut out[slot * AB..(slot + 1) * AB]);
        }
    }
}
/// Distinct groups touched by `(id, x)` pairs, with (slot, x) lists.
pub fn by_group(items: &[(u64, i64)]) -> std::collections::BTreeMap<usize, Vec<(usize, i64)>> {
    let mut m: std::collections::BTreeMap<usize, Vec<(usize, i64)>> = Default::default();
    for &(id, d) in items {
        m.entry(id as usize / G)
            .or_default()
            .push((id as usize % G, d));
    }
    m
}
