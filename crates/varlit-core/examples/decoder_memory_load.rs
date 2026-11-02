//! Equal-work decoder qualification; fixture generation is outside operation time.
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;
use varlit_core::{Decoder, Endian, Result, VarlitDecode, VarlitEncode, encode_to_vec};
fn run<T: VarlitEncode + VarlitDecode>(
    value: T,
    shape: &str,
    count: usize,
    rounds: usize,
) -> Result<()> {
    let bytes = encode_to_vec(&value, Endian::Little)?;
    drop(value);
    let start = Instant::now();
    for _ in 0..rounds {
        let value =
            Decoder::decode_from_slice_limited::<T>(&bytes, Endian::Little, 1024 * 1024 * 1024)?;
        black_box(&value);
        drop(value);
    }
    println!(
        "{{\"shape\":\"{shape}\",\"count\":{count},\"rounds\":{rounds},\"input_bytes\":{},\"seconds\":{}}}",
        bytes.len(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let shape = &args[1];
    let count: usize = args[2].parse().unwrap();
    let rounds: usize = args[3].parse().unwrap();
    match shape.as_str() {
        "fixed" => run(
            (0..count as u64).map(|i| (i, i)).collect::<HashMap<_, _>>(),
            shape,
            count,
            rounds,
        ),
        "strings" => run(
            (0..count as u64)
                .map(|i| (i, "a".repeat(256)))
                .collect::<HashMap<_, _>>(),
            shape,
            count,
            rounds,
        ),
        "nested" => run(
            (0..count as u64)
                .map(|i| {
                    (
                        i,
                        (0..32u64)
                            .map(|j| (j, "z".repeat(128)))
                            .collect::<HashMap<_, _>>(),
                    )
                })
                .collect::<HashMap<_, _>>(),
            shape,
            count,
            rounds,
        ),
        _ => panic!("unknown workload"),
    }
}
