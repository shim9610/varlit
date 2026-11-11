//! Runnable companion to docs/scalable-io.md; this is an application wire layout.
use varlit::{DiskIndexOptions, VarlitEmpty, varlit_format};
varlit_format! {
    pub format PackedCapture {
        magic: b"PACK";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable ItemBlock(id = 1, key = [group], key_index = disk) {
                group: u64,
                data: Vec<u8>,
            }
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("varlit-group-example-{}", std::process::id()));
    std::fs::create_dir(&root)?;
    let path = root.join("capture");
    // Version 1 application payload: exactly 512 little-endian u64 items.
    let data = (0..512u64).flat_map(u64::to_le_bytes).collect::<Vec<_>>();
    let mut writer = PackedCapture::create_indexed_writer(&path, DiskIndexOptions::default())?;
    writer.push_item_block(&ItemBlock { group: 7, data })?;
    writer.sync()?;
    // Independent reader while the writer remains open; no live-file mmap.
    let reader = PackedCapture::open_indexed_reader(&path, DiskIndexOptions::default())?;
    let mut block = ItemBlock::empty();
    if !reader.get_item_block(&7, &mut block)? {
        return Err("missing group".into());
    }
    if block.data.len() != 512 * 8 {
        return Err("invalid application layout".into());
    }
    let slot = 37;
    let item = block
        .data
        .get(slot * 8..(slot + 1) * 8)
        .ok_or("invalid slot")?;
    assert_eq!(u64::from_le_bytes(item.try_into()?), 37);
    drop(reader);
    drop(writer);
    std::fs::remove_dir_all(root)?;
    Ok(())
}
