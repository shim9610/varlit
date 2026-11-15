use super::*;
#[kani::proof_for_contract(charge)]
fn charge_transition_is_exact_for_every_unsigned_state() {
    let _ = charge(kani::any(), kani::any());
}
#[kani::proof_for_contract(read_end)]
fn cursor_transition_is_exact_for_every_unsigned_state() {
    let _ = read_end(kani::any(), kani::any(), kani::any());
}
#[kani::proof_for_contract(callback_exit)]
fn callback_transition_accepts_exactly_preserved_frames() {
    let _ = callback_exit(
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
    );
}

#[kani::proof_for_contract(same_scope)]
fn scope_admission_distinguishes_absence_identity_and_other_objects() {
    let a: u8 = kani::any();
    let b: u8 = kani::any();
    let expected = if kani::any() { Some(&a) } else { None };
    let actual = match kani::any::<u8>() % 3 {
        0 => None,
        1 => Some(&a),
        _ => Some(&b),
    };
    let _ = same_scope(expected, actual);
}
