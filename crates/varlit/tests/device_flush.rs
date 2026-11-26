//! `DeviceFlush` chooses whether a durable sync also flushes the drive's
//! write cache. It is runtime policy: declared in the DSL or set on a spec,
//! never stored in the file, and every handle kind honours it.

use varlit::{
    DeviceFlush, StreamOptions, VarlitEmpty, VarlitStreamReader, VarlitStreamWriter, varlit_format,
};

varlit_format! {
    pub format OsFlush {
        magic: b"OSFLUSH";
        version: 1;
        endian: little;
        schema_hash: computed;
        device_flush: os;
        index: block_offset_chain;
        commit: transaction_marker(explicit);
        blocks { fixed Pt(id = 1) { x: u32, } }
    }
}

varlit_format! {
    pub format DefaultFlush {
        magic: b"DFFLUSH";
        version: 1;
        endian: little;
        schema_hash: computed;
        blocks { fixed Q(id = 1) { x: u32, } }
    }
}

#[test]
fn the_dsl_declares_it_and_the_default_flushes_the_drive_cache() {
    assert_eq!(OsFlush::spec().device_flush, DeviceFlush::Os);
    assert_eq!(DefaultFlush::spec().device_flush, DeviceFlush::Full);
}

#[test]
fn it_is_not_part_of_the_schema() {
    let os = OsFlush::spec();
    assert_eq!(
        os.computed_schema_hash(),
        os.with_device_flush(DeviceFlush::Full)
            .computed_schema_hash()
    );
}

/// A native file written under `os` is byte for byte the file written under
/// `full`, and either setting opens what the other wrote.
#[test]
fn native_files_are_identical_under_both_settings() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let write = |flush: DeviceFlush, name: &str| -> varlit::Result<std::path::PathBuf> {
        let path = dir.path().join(name);
        let mut writer = OsFlush::spec()
            .with_device_flush(flush)
            .create_writer(&path)?;
        for x in 0..4 {
            writer.push(&Pt { x })?;
        }
        writer.commit_durable()?;
        writer.sync()?;
        Ok(path)
    };
    let os = write(DeviceFlush::Os, "os.varlit")?;
    let full = write(DeviceFlush::Full, "full.varlit")?;
    assert_eq!(std::fs::read(&os)?, std::fs::read(&full)?);
    let reader = OsFlush::spec()
        .with_device_flush(DeviceFlush::Full)
        .open_readonly(&os)?;
    let (mut scratch, mut out, mut values) = (Vec::new(), Pt::empty(), Vec::new());
    for entry in reader.block_chain(<Pt as varlit::VarlitBlock>::ID)? {
        reader.decode_block_into(&entry?, &mut scratch, &mut out)?;
        values.push(out.x);
    }
    values.reverse();
    assert_eq!(values, [0, 1, 2, 3]);
    Ok(())
}

#[test]
fn stream_writers_sync_and_recover_under_os() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varlit");
    let spec = DefaultFlush::spec().with_device_flush(DeviceFlush::Os);
    let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
    writer.push_info(&Q { x: 1 })?;
    writer.sync()?;
    writer.push_info(&Q { x: 2 })?;
    drop(writer);
    // Recovery keeps the synced record and cuts the unsynced one.
    drop(VarlitStreamWriter::open(
        spec,
        &path,
        StreamOptions::default(),
    )?);
    let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    let mut blocks = reader.blocks::<Q>()?;
    let mut out = Q::empty();
    assert!(blocks.next_into(&mut out)?);
    assert_eq!(out, Q { x: 1 });
    assert!(!blocks.next_into(&mut out)?);
    Ok(())
}
