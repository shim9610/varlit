//! Diagnostic owner workloads with dispatch outside each timed loop.
use std::{hint::black_box, time::Instant};
use varlit_core::{ScanCancellationToken, TryClone, try_box};

#[inline(never)]
fn boxes(iterations: usize) -> varlit_core::Result<usize> {
    let mut sum = 0usize;
    for i in 0..iterations {
        let value = try_box([i as u64; 8])?;
        sum = sum.wrapping_add(value[0] as usize);
        black_box(&value);
    }
    Ok(sum)
}

#[inline(never)]
fn nested<T: TryClone + AsRef<Vec<String>>>(
    source: &T,
    iterations: usize,
) -> varlit_core::Result<usize> {
    let mut sum = 0usize;
    for _ in 0..iterations {
        let value = source.try_clone()?;
        sum = sum.wrapping_add(value.as_ref().len());
        black_box(&value);
    }
    Ok(sum)
}

#[inline(never)]
fn shared(iterations: usize) -> varlit_core::Result<usize> {
    let mut sum = 0usize;
    for _ in 0..iterations {
        let token = ScanCancellationToken::try_new()?;
        let reader = token.try_clone()?;
        token.cancel();
        sum += usize::from(reader.is_cancelled());
        black_box(&reader);
    }
    Ok(sum)
}

fn main() -> varlit_core::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = &args[1];
    let iterations: usize = args[2].parse().unwrap();
    let source = Box::new(vec![String::from("0123456789abcdef0123456789abcdef"); 1024]);
    let start = Instant::now();
    let sum = match mode.as_str() {
        "box" => boxes(iterations)?,
        "nested-box" => nested(&source, iterations)?,
        "shared" => shared(iterations)?,
        _ => panic!("unknown workload"),
    };
    let seconds = start.elapsed().as_secs_f64();
    black_box(&source);
    println!(
        "{{\"mode\":\"{mode}\",\"iterations\":{iterations},\"sum\":{sum},\"seconds\":{seconds}}}"
    );
    Ok(())
}
