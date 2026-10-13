#![cfg(all(
    feature = "experimental-matrix-reuse",
    feature = "scalable-fault-injection"
))]
#[path = "support/matrix_reuse_model.rs"]
mod model;
#[test]
fn reuse_model_histories_cover_dirty_discard_and_eviction() {
    for seed in model::seeds(false) {
        model::run(&seed, false);
    }
}
fn publication_cases(kind: u8) {
    for seed in model::seeds(true).into_iter().filter(|s| s[4] == kind) {
        model::run(&seed, true);
    }
}
#[test]
fn reuse_every_publication_cut_has_an_exact_recovery_generation() {
    publication_cases(0);
}
#[test]
fn reuse_during_partial_range_copy_never_returns_mixed_pages() {
    publication_cases(1);
}
#[test]
fn reuse_current_page_corruption_never_falls_back_to_old_values() {
    publication_cases(2);
}

#[test]
fn reuse_corrupt_undo_entry_refuses_repair_before_modifying_primary() {
    publication_cases(3);
}
