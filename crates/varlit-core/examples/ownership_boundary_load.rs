//! Equal-work owner allocation and cloning at the public API boundary.
use std::{hint::black_box, time::Instant};
use varlit_core::{ScanCancellationToken, TryClone, try_box};
fn main() -> varlit_core::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = &args[1];
    let iterations: usize = args[2].parse().unwrap();
    let source = Box::new(vec![String::from("0123456789abcdef0123456789abcdef"); 1024]);
    let start = Instant::now();
    let mut sum = 0usize;
    for i in 0..iterations {
        match mode.as_str() {
            "box" => {
                let value = try_box([i as u64; 8])?;
                sum = sum.wrapping_add(value[0] as usize);
                black_box(&value);
            }
            "nested-box" => {
                let value = source.try_clone()?;
                sum = sum.wrapping_add(value.len());
                black_box(&value);
            }
            "shared" => {
                let token = ScanCancellationToken::try_new()?;
                let reader = token.try_clone()?;
                token.cancel();
                sum += usize::from(reader.is_cancelled());
                black_box(&reader);
            }
            _ => panic!("unknown workload"),
        }
    }
    println!(
        "{{\"mode\":\"{mode}\",\"iterations\":{iterations},\"sum\":{sum},\"seconds\":{}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
