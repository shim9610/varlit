//! Conditional contracts for the actual decoder. No allocator or BTreeMap model.
use super::*;

#[kani::proof]
fn full_width_decoder_charge_preserves_budget_on_failure() {
    let limit: u64 = kani::any();
    let remaining: u64 = kani::any();
    let bytes: u64 = kani::any();
    kani::assume(remaining <= limit); // constructor / successful-charge invariant
    let mut decoder = Decoder::new_limited(&[], Endian::Little, limit);
    decoder.materialization_remaining = remaining;
    let result = decoder.charge_materialization(bytes, "proof");
    assert_eq!(result.is_ok(), bytes <= remaining, "decoder budget bypass");
    assert_eq!(
        decoder.materialization_remaining,
        if bytes <= remaining {
            remaining - bytes
        } else {
            remaining
        },
        "failed decoder charge replenished or consumed budget"
    );
    assert!(decoder.materialization_remaining <= limit);
    std::mem::forget(result);
    std::mem::forget(decoder);
}

#[kani::proof]
fn full_width_collection_preflight_bounds_zero_width_work() {
    let count: usize = kani::any();
    let wire: usize = kani::any();
    let entry: usize = kani::any();
    let budget: u64 = kani::any();
    let input = [0u8; 64];
    let mut decoder = Decoder::new_limited(&input, Endian::Little, budget);
    let result = decoder.preflight_count(count, wire, entry, "proof");
    let charge = count as u128 * entry.max(1) as u128;
    let fits_wire = wire == 0 || count as u128 * wire as u128 <= 64;
    let accepted = fits_wire && charge <= usize::MAX as u128 && charge <= budget as u128;
    assert_eq!(
        result.is_ok(),
        accepted,
        "collection preflight differs from wide oracle"
    );
    assert_eq!(
        decoder.materialization_remaining,
        if accepted {
            budget - charge as u64
        } else {
            budget
        }
    );
    if accepted {
        assert!(
            count as u128 <= budget as u128,
            "zero-width collection escaped finite work budget"
        );
    }
    std::mem::forget(result);
    std::mem::forget(decoder);
}

#[kani::proof]
fn input_cursor_advances_exactly_or_stays_unchanged() {
    let input = [0u8; 64];
    let start: usize = kani::any();
    let len: usize = kani::any();
    kani::assume(start <= input.len()); // reachable cursor invariant, not file validity
    let mut decoder = Decoder::new(&input, Endian::Little);
    decoder.position = start;
    let result = decoder.read_exact(len);
    let accepted = start as u128 + len as u128 <= input.len() as u128;
    assert_eq!(
        result.is_ok(),
        accepted,
        "cursor accepted out-of-input extent"
    );
    if let Ok(bytes) = &result {
        assert_eq!(bytes.len(), len);
    }
    assert_eq!(decoder.position, if accepted { start + len } else { start });
    std::mem::forget(result);
    std::mem::forget(decoder);
}

// Models a returning callback that obeys the public charge interface. The
// actual nested/accounted transfer code is executed, including error returns.
struct Charged;
impl VarlitDecode for Charged {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let bytes = decoder.read_u64()?;
        decoder.charge_materialization(bytes, "proof")?;
        if decoder.read_u8()? != 0 {
            return Err(Error::InvalidUtf8);
        }
        Ok(Self)
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn nested_failure_cannot_refund_a_successful_charge() {
    let limit: u64 = kani::any();
    let bytes: u64 = kani::any();
    let fail: bool = kani::any();
    let mut input = [0u8; 9];
    input[..8].copy_from_slice(&bytes.to_le_bytes());
    input[8] = u8::from(fail);
    let mut parent = Decoder::new_limited(&[], Endian::Little, limit);
    let result = parent.decode_nested::<Charged>(&input);
    assert_eq!(
        result.is_ok(),
        bytes <= limit && !fail,
        "nested positive/error witness lost"
    );
    assert_eq!(
        parent.materialization_remaining,
        if bytes <= limit { limit - bytes } else { limit },
        "nested error refunded charge"
    );
    std::mem::forget(result);
    std::mem::forget(parent);
}

#[kani::proof]
#[kani::unwind(10)]
fn retained_sibling_decodes_share_one_budget_even_after_errors() {
    let limit: u64 = kani::any();
    let first: u64 = kani::any();
    let second: u64 = kani::any();
    let mut remaining = limit;
    for charge in [first, second] {
        let mut input = [0u8; 9];
        input[..8].copy_from_slice(&charge.to_le_bytes());
        input[8] = kani::any();
        let before = remaining;
        let result =
            Decoder::decode_from_slice_accounted::<Charged>(&input, Endian::Little, &mut remaining);
        assert_eq!(
            remaining,
            if charge <= before {
                before - charge
            } else {
                before
            },
            "accounted decode reset the shared budget"
        );
        assert!(remaining <= before);
        std::mem::forget(result);
    }
    if first <= limit && second <= limit - first {
        assert_eq!((limit - remaining) as u128, first as u128 + second as u128);
    }
}

#[kani::proof]
fn hashmap_preflight_accounts_for_overlapping_tables() {
    let count: usize = kani::any();
    let budget: u64 = kani::any();
    let mut decoder = Decoder::new_limited(&[], Endian::Little, budget);
    let result = decoder.preflight_hash_map_count::<(), ()>(count, "proof");
    let table = hash_table_reservation_bytes::<((), ())>(count);
    let copies = if count > MAP_PREALLOCATION_ENTRIES {
        2
    } else {
        1
    };
    let total = table.and_then(|bytes| bytes.checked_mul(copies));
    let accepted = count as u128 <= budget as u128 && total.is_some_and(|bytes| bytes <= budget);
    assert_eq!(
        result.is_ok(),
        accepted,
        "rehash charge acceptance mismatch"
    );
    if let Some(total) = total.filter(|_| accepted) {
        assert_eq!(
            budget - decoder.materialization_remaining,
            total,
            "rehash overlap is not charged"
        );
    } else {
        assert_eq!(decoder.materialization_remaining, budget);
    }
    std::mem::forget(result);
    std::mem::forget(decoder);
}
