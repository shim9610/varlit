//! Equal-work standalone decoder benchmark; compile unchanged on both revisions.
use std::{hint::black_box, time::Instant};
use varlit_core::{CompressionLevel, Error, Result};
#[allow(dead_code)]
#[path = "../src/compression_buffers.rs"]
mod compression_buffers;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args
        .get(1)
        .ok_or("known, unknown or concatenated mode required")?;
    let length: usize = args.get(2).ok_or("byte count required")?.parse()?;
    let iterations: usize = args.get(3).ok_or("iterations required")?.parse()?;
    let bytes: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
    let mut encoder = zstd::bulk::Compressor::new(1)?;
    encoder.include_contentsize(mode != "unknown")?;
    let encoded = if mode == "concatenated" {
        let mut encoded = Vec::new();
        for chunk in bytes.chunks(1048576) {
            encoded.extend_from_slice(&encoder.compress(chunk)?);
        }
        encoded
    } else {
        encoder.compress(&bytes)?
    };
    let encoded_crc = crc32fast::hash(&encoded);
    let start = Instant::now();
    for _ in 0..iterations {
        let decoded = compression_buffers::decompress(&encoded, length)?;
        assert!(decoded == bytes, "decoded bytes differ");
        black_box(decoded);
    }
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "{{\"mode\":\"{mode}\",\"bytes\":{length},\"iterations\":{iterations},\"encoded_crc\":{encoded_crc},\"seconds\":{seconds}}}"
    );
    Ok(())
}
