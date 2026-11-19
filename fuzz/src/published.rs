//! The published end under damage: a native file of a transaction-marker
//! format whose `VPUB` slots are overwritten, whose tail grows by bytes no
//! writer published, or whose tail is cut, opened fresh and followed by a
//! reader that was open before.
//!
//! The oracle is what the published end promises: a reader never sees more
//! records than were committed, and bytes appended without a publication are
//! never seen at all. Damage elsewhere owes soundness only (no panic, no
//! record that was not written).

use std::fs;
use std::io::Write;

use varlit::VarlitEmpty;

use crate::{FuzzNativeFormat, FuzzPoint, published_end};

/// What the fuzz input does to the file after its records are committed.
enum Damage {
    /// Overwrite the published end's slots.
    Slots,
    /// Append bytes and publish nothing.
    UnpublishedTail,
    /// Append bytes and publish them, as a writer would.
    PublishedTail,
    /// Cut the file.
    Truncate,
}

fn point(index: u32) -> FuzzPoint {
    FuzzPoint {
        x: index,
        y: -i64::from(index),
    }
}

/// The points a handle shows, checked to be a prefix of what was written.
fn shown_prefix(file: &varlit::VarlitFile, committed: u32) -> Option<usize> {
    let mut payload = Vec::new();
    let mut out: Vec<FuzzPoint> = (0..committed as usize + 1)
        .map(|_| FuzzPoint::empty())
        .collect();
    let page = file.blocks::<FuzzPoint>(&mut payload, &mut out).ok()?;
    assert!(
        page.filled <= committed as usize,
        "a reader saw {} points, {committed} were committed",
        page.filled
    );
    for (index, shown) in out[..page.filled].iter().enumerate() {
        assert_eq!(
            *shown,
            point(index as u32),
            "a reader saw a point never written"
        );
    }
    Some(page.filled)
}

pub fn run(data: &[u8]) {
    let Some((&control, rest)) = data.split_first() else {
        return;
    };
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let path = dir.path().join("published.varlit");
    let committed = u32::from(control % 6) + 1;
    {
        let Ok(mut writer) = FuzzNativeFormat::create_writer(&path) else {
            return;
        };
        for index in 0..committed {
            writer.push_fuzz_point(&point(index)).expect("push");
            // The format commits at `flush`; `sync` makes the commit durable.
            writer.flush().expect("flush");
            writer.sync().expect("sync");
        }
    }
    let spec = FuzzNativeFormat::spec();
    let mut before = varlit::VarlitFile::open_readonly(spec, &path).expect("the fixture opens");
    assert_eq!(shown_prefix(&before, committed), Some(committed as usize));

    let damage = match (control >> 3) % 4 {
        0 => Damage::Slots,
        1 => Damage::UnpublishedTail,
        2 => Damage::PublishedTail,
        _ => Damage::Truncate,
    };
    let Ok(mut bytes) = fs::read(&path) else {
        return;
    };
    match damage {
        Damage::Slots => {
            let Some(at) = published_end::slots_at(&bytes) else {
                panic!("a transaction-marker file carries the published end");
            };
            let span = rest.len().min(published_end::SLOTS_LEN);
            bytes[at..at + span].copy_from_slice(&rest[..span]);
        }
        Damage::UnpublishedTail => bytes.extend_from_slice(rest),
        Damage::PublishedTail => {
            bytes.extend_from_slice(rest);
            published_end::publish(&mut bytes);
        }
        Damage::Truncate => {
            let cut = rest.first().map_or(bytes.len() / 2, |byte| {
                usize::from(*byte) * bytes.len() / 256
            });
            bytes.truncate(cut);
        }
    }
    // Rewrite in place, so the reader opened before keeps its handle.
    let Ok(mut file) = fs::OpenOptions::new().write(true).open(&path) else {
        return;
    };
    if file.set_len(0).is_err() || file.write_all(&bytes).is_err() {
        return;
    }
    drop(file);

    let followed = before
        .follow()
        .ok()
        .and_then(|_| shown_prefix(&before, committed));
    let fresh = varlit::VarlitFile::open_readonly(spec, &path)
        .ok()
        .and_then(|file| shown_prefix(&file, committed));
    if matches!(damage, Damage::UnpublishedTail) {
        // Bytes no writer published are not part of the file a reader reads.
        assert_eq!(
            fresh,
            Some(committed as usize),
            "unpublished bytes changed a fresh open"
        );
        assert_eq!(
            followed,
            Some(committed as usize),
            "unpublished bytes changed a follow"
        );
    }
    let _ = FuzzNativeFormat::diagnose_file(&path);
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_damage_mode_runs_on_short_inputs() {
        for control in 0u8..32 {
            for rest in [
                &[][..],
                &[0u8; 3][..],
                &[0xffu8; 64][..],
                &[0x41u8; 300][..],
            ] {
                let mut data = vec![control];
                data.extend_from_slice(rest);
                super::run(&data);
            }
        }
    }
}
