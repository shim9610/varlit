//! Fixed-work matrix publication with following readers and a retained snapshot.
use std::{sync::Barrier, time::Instant};
use varlit::{MatrixDimensions, MatrixKey, varlit_format};
varlit_format! {
    pub format OwnershipMatrix {
        magic: b"OWNM";
        version: 1;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Cell(id = 1, dims = [row, col], category = data) { payload: [u8; 4096], } }
    }
}
fn value(row: u64, epoch: u64) -> Cell {
    let mut payload = [epoch as u8; 4096];
    payload[..8].copy_from_slice(&row.to_le_bytes());
    payload[8..16].copy_from_slice(&epoch.to_le_bytes());
    Cell { payload }
}
fn main() -> varlit::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let durable = args.get(5).is_some_and(|v| v == "durable");
    let rows: u64 = args[1].parse().unwrap();
    let epochs: u64 = args[2].parse().unwrap();
    let readers: usize = args[3].parse().unwrap();
    let checks: u64 = args[4].parse().unwrap();
    assert!(rows >= epochs * 64 && rows > 0 && readers > 0);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("matrix");
    let spec = OwnershipMatrix::spec();
    let mut writer = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", rows), ("col", 1)]),
    )?;
    for row in 0..rows {
        let key = MatrixKey::new(row, 0);
        writer.write_matrix_cell(key, &value(row, 0))?;
        writer.commit_matrix_cell::<Cell>(key)?;
    }
    writer.sync()?;
    let barrier = Barrier::new(readers + 2); // writer + followers + pin
    let begin = Instant::now();
    std::thread::scope(|scope| -> varlit::Result<()> {
        let mut joins = Vec::new();
        for id in 0..=readers {
            let barrier = &barrier;
            let path = &path;
            joins.push(scope.spawn(move || -> varlit::Result<u64> {
                let mut reader = spec.open_reader(path)?;
                let mut sum = 0u64;
                barrier.wait();
                for epoch in 1..=epochs {
                    barrier.wait();
                    if id != readers {
                        reader.follow()?;
                    }
                    for check in 0..checks {
                        let row = (check * 104729 + epoch * 31 + id as u64 * 17) % rows;
                        let version = if id == readers || row >= epoch * 64 {
                            0
                        } else {
                            row / 64 + 1
                        };
                        let actual = reader.read_matrix_cell::<Cell>(MatrixKey::new(row, 0))?;
                        assert_eq!(actual.payload, value(row, version).payload);
                        sum = sum.wrapping_add(row ^ version);
                    }
                    barrier.wait();
                }
                Ok(sum)
            }));
        }
        barrier.wait();
        for epoch in 1..=epochs {
            for row in (epoch - 1) * 64..epoch * 64 {
                if durable {
                    writer.write_matrix_cell_durable(
                        MatrixKey::new(row, 0),
                        &value(row, epoch),
                        |_| Ok(()),
                    )?;
                } else {
                    writer.write_matrix_cell(MatrixKey::new(row, 0), &value(row, epoch))?;
                    writer.commit_matrix_cell::<Cell>(MatrixKey::new(row, 0))?;
                }
            }
            writer.sync()?;
            if epoch % 16 == 0 {
                writer.compact_matrix()?;
            }
            barrier.wait();
            barrier.wait();
        }
        let mut sum = 0u64;
        for join in joins {
            sum = sum.wrapping_add(join.join().unwrap()?);
        }
        println!(
            "{{\"rows\":{rows},\"epochs\":{epochs},\"readers\":{readers},\"checks\":{checks},\"sum\":{sum},\"seconds\":{}}}",
            begin.elapsed().as_secs_f64()
        );
        Ok(())
    })
}
