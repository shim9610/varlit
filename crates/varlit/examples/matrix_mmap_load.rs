//! Fixed-work matrix mmap benchmark. Both formats use identical access patterns.
//! Usage: matrix_mmap_load small|large CELLS READS whole|cell
use std::{hint::black_box, time::Instant};
use varlit::{MatrixDimensions, MatrixKey, VarlitMatrixBlock, varlit_format};
varlit_format! {
    pub format Narrow {
        magic: b"MMN1";
        version: 1;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Small(id = 1, dims = [row, col], category = data) { value: u32, } }
    }
}
varlit_format! {
    pub format Wide {
        magic: b"MMW1";
        version: 1;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Large(id = 1, dims = [row, col], category = data) { value: [u8; 4096], } }
    }
}
fn run<T: VarlitMatrixBlock>(
    spec: varlit::FormatSpec,
    value: T,
    cells: u64,
    reads: u64,
    whole: bool,
) -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix");
    let mut writer = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", cells), ("col", 1)]),
    )?;
    for row in 0..cells {
        let key = MatrixKey::new(row, 0);
        writer.write_matrix_cell(key, &value)?;
        writer.commit_matrix_cell::<T>(key)?;
    }
    writer.sync()?;
    drop(writer);
    let reader = spec.open_reader(&path)?;
    // SAFETY: no writers remain; the file lives until both mappings are dropped.
    let mapped = unsafe { reader.mmap_matrix()? };
    let start = Instant::now();
    let mut total = 0u64;
    for i in 0..reads {
        let key = MatrixKey::new(i.wrapping_mul(104729) % cells, 0);
        let bytes = if whole {
            black_box(mapped.cell_payload_window::<T>(key)?)
        } else {
            let window = unsafe { reader.mmap_matrix_cell::<T>(key)? };
            total += u64::from(black_box(window.as_bytes())[0]);
            continue;
        };
        total += u64::from(bytes[0]);
    }
    let seconds = start.elapsed().as_secs_f64();
    assert_eq!(total, reads * 7);
    println!(
        "{{\"cells\":{cells},\"reads\":{reads},\"stride\":{},\"whole\":{whole},\"seconds\":{seconds},\"total\":{total}}}",
        T::SLOT_STRIDE
    );
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 5);
    let cells = args[2].parse()?;
    let reads = args[3].parse()?;
    assert!(cells > 0 && reads > 0);
    let whole = match args[4].as_str() {
        "whole" => true,
        "cell" => false,
        _ => panic!("whole|cell"),
    };
    match args[1].as_str() {
        "small" => run(Narrow::spec(), Small { value: 7 }, cells, reads, whole)?,
        "large" => run(
            Wide::spec(),
            Large { value: [7; 4096] },
            cells,
            reads,
            whole,
        )?,
        _ => panic!("small|large"),
    }
    Ok(())
}
