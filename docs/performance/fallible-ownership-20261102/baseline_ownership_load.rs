//! Equal-work ownership allocation qualification.
use std::{hint::black_box, time::Instant};
use varlit_core::{LayoutValue};
fn main() -> varlit_core::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let mode = &args[1];
    let count: usize = args[2].parse().unwrap();
    let iterations: usize = args[3].parse().unwrap();
    let mut sum = 0u64;
    let elapsed = if mode == "bytes" {
        let value = LayoutValue::Bytes(vec![17; count]);
        let start = Instant::now();
        for _ in 0..iterations {
            let LayoutValue::Bytes(copy) = value.clone() else { unreachable!() };
            sum += copy.len() as u64 + u64::from(copy[copy.len()-1]);
            black_box(&copy);
        }
        start.elapsed()
    } else {
        let value = vec![vec![String::from("0123456789abcdef0123456789abcdef"); 4]; count];
        let start = Instant::now();
        for _ in 0..iterations {
            let copy = value.clone();
            sum += copy.len() as u64 + copy.last().unwrap()[3].len() as u64;
            black_box(&copy);
        }
        start.elapsed()
    };
    println!("{{\"mode\":\"{mode}\",\"count\":{count},\"iterations\":{iterations},\"sum\":{sum},\"seconds\":{}}}", elapsed.as_secs_f64());
    Ok(())
}
