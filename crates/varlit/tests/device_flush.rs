use varlit::{DeviceFlush, varlit_format};
varlit_format! {
    pub format OsFlush {
        magic: b"OSFLUSH";
        version: 1;
        endian: little;
        device_flush: os;
        commit: transaction_marker(explicit);
        blocks { fixed Pt(id = 1) { x: u32, } }
    }
}
varlit_format! {
    pub format FullFlush {
        magic: b"FLFLUSH";
        version: 1;
        endian: little;
        blocks { fixed Q(id = 1) { x: u32, } }
    }
}
#[test]
fn dsl() -> varlit::Result<()> {
    assert_eq!(OsFlush::spec().device_flush, DeviceFlush::Os);
    assert_eq!(FullFlush::spec().device_flush, DeviceFlush::Full);
    assert_eq!(OsFlush::spec().computed_schema_hash(), OsFlush::spec().with_device_flush(DeviceFlush::Full).computed_schema_hash());
    let d = std::env::temp_dir().join(format!("zzf{}", std::process::id())); std::fs::create_dir_all(&d).unwrap();
    let mut w = OsFlush::create_writer(d.join("a.v"))?;
    w.push_pt(&Pt{x:1})?; w.commit_durable()?; w.sync()?;
    std::fs::remove_dir_all(&d).ok(); Ok(())
}
