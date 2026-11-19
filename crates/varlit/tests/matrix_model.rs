#![cfg(feature = "integrity")]
#[path = "support/matrix_model.rs"]
mod model;
#[test]
fn seeded_matrix_histories_match_independent_snapshots() {
    for seed in 0..64u64 {
        let mut state = seed;
        let data: Vec<u8> = (0..384)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 32) as u8
            })
            .collect();
        model::run(&data).unwrap_or_else(|error| panic!("seed={seed}: {error}"));
    }
}

/// Fuzzer input (matrix_state_machine, 2026-11-20 corpus run): one
/// publication, then about 120 unpublished overwrites of one cell. The
/// restaged cell and its tile bitmap outgrow half the ring, the overflow
/// publication puts generation 1 in place, and the reader pinned at
/// generation 0 expires. The oracle must accept that expiry and follow.
#[test]
fn restaged_overwrites_expire_a_pinned_reader_that_then_follows() {
    let data = include_bytes!("data/matrix_model/restaged-overwrites-expire-pinned-reader.bin");
    model::run(data).unwrap_or_else(|error| panic!("{error}"));
}
