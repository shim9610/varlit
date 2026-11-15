//! A reader following a file that another handle is still writing sees the
//! writer's next record half written at the end of the file. Under a format
//! with no commit marker that is the writer's next record, not damage: the
//! follow stops before it and picks it up on a later call. It used to fail
//! with `CorruptTail` as soon as a poll landed mid-record.
#![cfg(feature = "integrity")]

use std::sync::atomic::{AtomicBool, Ordering};

use varlit::{MatrixDimensions, MatrixKey, VarlitEmpty, VarlitFile, varlit_format};

varlit_format! {
    pub format FollowedMatrix {
        magic: b"VFOLLOWM"; version: 1; integrity: crc32_with_header;
        dims { entry: u32, slot: u32, }
        commit: cell_bitmap { keyspace = [entry, slot]; categories = [data, head]; };
        blocks {
            matrix Cell(id = 11, dims = [entry, slot], category = data) { id: u64, body: [u8; 32] }
            matrix Root(id = 13, dims = [entry, slot], category = head) { rows: u64 }
        }
    }
}

varlit_format! {
    pub format FollowedLog {
        magic: b"VFOLLOWL"; version: 1; integrity: crc32_with_header;
        blocks { variable Line(id = 1) { id: u64, bytes: Vec<u8>, } }
    }
}

fn key(n: u64) -> MatrixKey {
    MatrixKey::new(n, 0)
}

#[test]
fn a_matrix_reader_follows_a_writer_mid_record() -> varlit::Result<()> {
    const ROWS: u64 = 20_000;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("followed.varlit");
    let mut writer = FollowedMatrix::spec().create_with_dims(
        &path,
        MatrixDimensions::from_pairs([("entry", ROWS), ("slot", 1)]),
    )?;
    writer.write_matrix_cell(key(0), &Root { rows: 0 })?;
    writer.commit_matrix_cell::<Root>(key(0))?;
    writer.sync()?;
    // Opened before any further write, so only `follow` meets a live tail.
    let mut reader = VarlitFile::open_readonly(FollowedMatrix::spec(), &path)?;
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let following = scope.spawn(|| -> varlit::Result<u64> {
            let mut root = Root::empty();
            let mut seen = 0;
            loop {
                let finished = done.load(Ordering::Acquire);
                reader.follow()?;
                reader.read_matrix_cell::<Root>(key(0), &mut root)?;
                assert!(root.rows >= seen, "the published row count went back");
                seen = root.rows;
                if finished {
                    return Ok(seen);
                }
            }
        });
        for n in 1..ROWS {
            writer.write_matrix_cell(
                key(n),
                &Cell {
                    id: n,
                    body: [7; 32],
                },
            )?;
            writer.commit_matrix_cell::<Cell>(key(n))?;
            if n % 100 == 0 {
                writer.write_matrix_cell(key(0), &Root { rows: n })?;
                writer.commit_matrix_cell::<Root>(key(0))?;
                writer.sync()?;
            }
        }
        writer.sync()?;
        done.store(true, Ordering::Release);
        let seen = following.join().expect("reader thread")?;
        // The last root is written at the last multiple of 100 below ROWS.
        assert_eq!(seen, (ROWS - 1) / 100 * 100);
        Ok(())
    })
}

#[test]
fn a_record_reader_follows_a_writer_mid_record() -> varlit::Result<()> {
    const LINES: u64 = 3_000;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("followed-log.varlit");
    let mut writer = FollowedLog::spec().create(&path)?;
    writer.sync()?;
    let mut reader = VarlitFile::open_readonly(FollowedLog::spec(), &path)?;
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let following = scope.spawn(|| -> varlit::Result<()> {
            loop {
                let finished = done.load(Ordering::Acquire);
                reader.follow()?;
                if finished {
                    return Ok(());
                }
            }
        });
        let mut line = Line::empty();
        for n in 0..LINES {
            line.id = n;
            line.bytes.clear();
            // Some lines span several write calls' worth of bytes.
            line.bytes.resize(64 + (n as usize % 7) * 40_000, n as u8);
            writer.push(&line)?;
            writer.flush()?;
        }
        writer.sync()?;
        done.store(true, Ordering::Release);
        following.join().expect("reader thread")
    })?;
    let mut entries = Vec::new();
    reader.block_entries_into::<Line>(&mut entries)?;
    assert_eq!(entries.len() as u64, LINES);
    Ok(())
}
