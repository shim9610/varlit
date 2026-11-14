//! Equal requested-byte work for full materialization and range streaming.
use std::{hint::black_box, time::Instant};
use varlit_core::{ChunkedBytes, CompressionLevel};

fn byte(at: usize, random: bool) -> u8 {
    if random {
        let mut n = (at as u64).wrapping_add(0x9e3779b97f4a7c15);
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        (n ^ (n >> 31)) as u8
    } else {
        (at % 65521) as u8 ^ ((at / 4096) % 251) as u8
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = &args[1];
    let path = &args[2];
    let length: usize = args[3].parse()?;
    if mode == "prepare" {
        let chunk: usize = args[4].parse()?;
        let random = args.get(5).is_some_and(|arg| arg == "random");
        let data: Vec<_> = (0..length).map(|at| byte(at, random)).collect();
        let encoded = ChunkedBytes::from_zstd_chunks(&data, chunk, CompressionLevel::Fast)?;
        std::fs::write(path, encoded.encoded())?;
        return Ok(());
    }
    let start: usize = args[4].parse()?;
    let count: usize = args[5].parse()?;
    let iterations: usize = args[6].parse()?;
    let readers: usize = args[7].parse()?;
    let random = args.get(8).is_some_and(|arg| arg == "random");
    let chunks = ChunkedBytes::from_encoded(std::fs::read(path)?)?;
    let mut expected = crc32fast::Hasher::new();
    let mut reference = [0; 32768];
    for at in (start..start + count).step_by(reference.len()) {
        let n = reference.len().min(start + count - at);
        for (i, b) in reference[..n].iter_mut().enumerate() {
            *b = byte(at + i, random);
        }
        expected.update(&reference[..n]);
    }
    let expected = expected.finalize();
    let begin = Instant::now();
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for _ in 0..readers {
            joins.push(scope.spawn(|| -> varlit_core::Result<()> {
                let mut buffer = [0; 32768];
                for _ in 0..iterations {
                    let mut actual = crc32fast::Hasher::new();
                    let mut delivered = 0;
                    if mode.starts_with("stream") {
                        let active = if mode == "stream-small" {
                            64
                        } else {
                            buffer.len()
                        };
                        chunks.stream_range(
                            start as u64..(start + count) as u64,
                            &mut buffer[..active],
                            8 * 1024 * 1024,
                            |at, bytes| {
                                assert_eq!(at, (start + delivered) as u64);
                                actual.update(bytes);
                                delivered += bytes.len();
                                Ok(())
                            },
                        )?;
                    } else {
                        let decoded = chunks.decode_to_vec_limited(length as u64)?;
                        actual.update(&decoded[start..start + count]);
                        delivered = count;
                        black_box(&decoded);
                    }
                    assert_eq!(delivered, count);
                    assert_eq!(actual.finalize(), expected);
                }
                Ok(())
            }));
        }
        for join in joins {
            join.join().unwrap().unwrap();
        }
    });
    println!(
        "{{\"mode\":\"{mode}\",\"length\":{length},\"start\":{start},\"count\":{count},\"iterations\":{iterations},\"readers\":{readers},\"crc\":{expected},\"seconds\":{}}}",
        begin.elapsed().as_secs_f64()
    );
    Ok(())
}
