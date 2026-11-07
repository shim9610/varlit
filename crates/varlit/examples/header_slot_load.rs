//! Fixed-work reads of an editable header; includes checksum and decoding.
use std::{hint::black_box, time::Instant};
use varlit::{VarlitBlock, varlit_format};
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Meta {
    value: u64,
}
varlit_format! {
    pub struct Header {
        magic: b"HDRLOAD1"; version: 1; endian: little;
        header_slots { capacity: 256; integrity: rolling; blocks: [Meta]; }
        blocks: [Meta];
    }
}
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Meta0 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "fixed")]
struct Meta1 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 3, version = 1, kind = "fixed")]
struct Meta2 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 4, version = 1, kind = "fixed")]
struct Meta3 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 5, version = 1, kind = "fixed")]
struct Meta4 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 6, version = 1, kind = "fixed")]
struct Meta5 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 7, version = 1, kind = "fixed")]
struct Meta6 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 8, version = 1, kind = "fixed")]
struct Meta7 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 9, version = 1, kind = "fixed")]
struct Meta8 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 10, version = 1, kind = "fixed")]
struct Meta9 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 11, version = 1, kind = "fixed")]
struct Meta10 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 12, version = 1, kind = "fixed")]
struct Meta11 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 13, version = 1, kind = "fixed")]
struct Meta12 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 14, version = 1, kind = "fixed")]
struct Meta13 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 15, version = 1, kind = "fixed")]
struct Meta14 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 16, version = 1, kind = "fixed")]
struct Meta15 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 17, version = 1, kind = "fixed")]
struct Meta16 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 18, version = 1, kind = "fixed")]
struct Meta17 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 19, version = 1, kind = "fixed")]
struct Meta18 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 20, version = 1, kind = "fixed")]
struct Meta19 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 21, version = 1, kind = "fixed")]
struct Meta20 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 22, version = 1, kind = "fixed")]
struct Meta21 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 23, version = 1, kind = "fixed")]
struct Meta22 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 24, version = 1, kind = "fixed")]
struct Meta23 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 25, version = 1, kind = "fixed")]
struct Meta24 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 26, version = 1, kind = "fixed")]
struct Meta25 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 27, version = 1, kind = "fixed")]
struct Meta26 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 28, version = 1, kind = "fixed")]
struct Meta27 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 29, version = 1, kind = "fixed")]
struct Meta28 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 30, version = 1, kind = "fixed")]
struct Meta29 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 31, version = 1, kind = "fixed")]
struct Meta30 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 32, version = 1, kind = "fixed")]
struct Meta31 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 33, version = 1, kind = "fixed")]
struct Meta32 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 34, version = 1, kind = "fixed")]
struct Meta33 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 35, version = 1, kind = "fixed")]
struct Meta34 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 36, version = 1, kind = "fixed")]
struct Meta35 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 37, version = 1, kind = "fixed")]
struct Meta36 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 38, version = 1, kind = "fixed")]
struct Meta37 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 39, version = 1, kind = "fixed")]
struct Meta38 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 40, version = 1, kind = "fixed")]
struct Meta39 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 41, version = 1, kind = "fixed")]
struct Meta40 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 42, version = 1, kind = "fixed")]
struct Meta41 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 43, version = 1, kind = "fixed")]
struct Meta42 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 44, version = 1, kind = "fixed")]
struct Meta43 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 45, version = 1, kind = "fixed")]
struct Meta44 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 46, version = 1, kind = "fixed")]
struct Meta45 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 47, version = 1, kind = "fixed")]
struct Meta46 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 48, version = 1, kind = "fixed")]
struct Meta47 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 49, version = 1, kind = "fixed")]
struct Meta48 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 50, version = 1, kind = "fixed")]
struct Meta49 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 51, version = 1, kind = "fixed")]
struct Meta50 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 52, version = 1, kind = "fixed")]
struct Meta51 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 53, version = 1, kind = "fixed")]
struct Meta52 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 54, version = 1, kind = "fixed")]
struct Meta53 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 55, version = 1, kind = "fixed")]
struct Meta54 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 56, version = 1, kind = "fixed")]
struct Meta55 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 57, version = 1, kind = "fixed")]
struct Meta56 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 58, version = 1, kind = "fixed")]
struct Meta57 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 59, version = 1, kind = "fixed")]
struct Meta58 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 60, version = 1, kind = "fixed")]
struct Meta59 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 61, version = 1, kind = "fixed")]
struct Meta60 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 62, version = 1, kind = "fixed")]
struct Meta61 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 63, version = 1, kind = "fixed")]
struct Meta62 {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 64, version = 1, kind = "fixed")]
struct Meta63 {
    value: u64,
}
varlit_format! { pub struct ManyHeader { magic: b"HDRLOAD2"; version: 1; endian: little; header_slots { capacity: 4096; integrity: rolling; blocks: [Meta0, Meta1, Meta2, Meta3, Meta4, Meta5, Meta6, Meta7, Meta8, Meta9, Meta10, Meta11, Meta12, Meta13, Meta14, Meta15, Meta16, Meta17, Meta18, Meta19, Meta20, Meta21, Meta22, Meta23, Meta24, Meta25, Meta26, Meta27, Meta28, Meta29, Meta30, Meta31, Meta32, Meta33, Meta34, Meta35, Meta36, Meta37, Meta38, Meta39, Meta40, Meta41, Meta42, Meta43, Meta44, Meta45, Meta46, Meta47, Meta48, Meta49, Meta50, Meta51, Meta52, Meta53, Meta54, Meta55, Meta56, Meta57, Meta58, Meta59, Meta60, Meta61, Meta62, Meta63]; } blocks: [Meta0, Meta1, Meta2, Meta3, Meta4, Meta5, Meta6, Meta7, Meta8, Meta9, Meta10, Meta11, Meta12, Meta13, Meta14, Meta15, Meta16, Meta17, Meta18, Meta19, Meta20, Meta21, Meta22, Meta23, Meta24, Meta25, Meta26, Meta27, Meta28, Meta29, Meta30, Meta31, Meta32, Meta33, Meta34, Meta35, Meta36, Meta37, Meta38, Meta39, Meta40, Meta41, Meta42, Meta43, Meta44, Meta45, Meta46, Meta47, Meta48, Meta49, Meta50, Meta51, Meta52, Meta53, Meta54, Meta55, Meta56, Meta57, Meta58, Meta59, Meta60, Meta61, Meta62, Meta63]; } }
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let reads: u64 = args[1].parse()?;
    if let Some(mode) = args.get(2) {
        return many(reads, mode == "shuffled");
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("header");
    let mut writer = Header::create(&path)?;
    writer.write_header_block(&Meta { value: 7 })?;
    drop(writer);
    let reader = Header::open_readonly(&path)?;
    let start = Instant::now();
    let mut total = 0;
    for _ in 0..reads {
        total += black_box(reader.read_header_block::<Meta>()?.unwrap().value);
    }
    assert_eq!(total, 7 * reads);
    println!(
        "{{\"reads\":{reads},\"seconds\":{},\"total\":{total}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn many(reads: u64, shuffled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("header");
    let mut writer = ManyHeader::create(&path)?;
    writer.write_header_block(&Meta0 { value: 7 })?;
    writer.write_header_block(&Meta1 { value: 7 })?;
    writer.write_header_block(&Meta2 { value: 7 })?;
    writer.write_header_block(&Meta3 { value: 7 })?;
    writer.write_header_block(&Meta4 { value: 7 })?;
    writer.write_header_block(&Meta5 { value: 7 })?;
    writer.write_header_block(&Meta6 { value: 7 })?;
    writer.write_header_block(&Meta7 { value: 7 })?;
    writer.write_header_block(&Meta8 { value: 7 })?;
    writer.write_header_block(&Meta9 { value: 7 })?;
    writer.write_header_block(&Meta10 { value: 7 })?;
    writer.write_header_block(&Meta11 { value: 7 })?;
    writer.write_header_block(&Meta12 { value: 7 })?;
    writer.write_header_block(&Meta13 { value: 7 })?;
    writer.write_header_block(&Meta14 { value: 7 })?;
    writer.write_header_block(&Meta15 { value: 7 })?;
    writer.write_header_block(&Meta16 { value: 7 })?;
    writer.write_header_block(&Meta17 { value: 7 })?;
    writer.write_header_block(&Meta18 { value: 7 })?;
    writer.write_header_block(&Meta19 { value: 7 })?;
    writer.write_header_block(&Meta20 { value: 7 })?;
    writer.write_header_block(&Meta21 { value: 7 })?;
    writer.write_header_block(&Meta22 { value: 7 })?;
    writer.write_header_block(&Meta23 { value: 7 })?;
    writer.write_header_block(&Meta24 { value: 7 })?;
    writer.write_header_block(&Meta25 { value: 7 })?;
    writer.write_header_block(&Meta26 { value: 7 })?;
    writer.write_header_block(&Meta27 { value: 7 })?;
    writer.write_header_block(&Meta28 { value: 7 })?;
    writer.write_header_block(&Meta29 { value: 7 })?;
    writer.write_header_block(&Meta30 { value: 7 })?;
    writer.write_header_block(&Meta31 { value: 7 })?;
    writer.write_header_block(&Meta32 { value: 7 })?;
    writer.write_header_block(&Meta33 { value: 7 })?;
    writer.write_header_block(&Meta34 { value: 7 })?;
    writer.write_header_block(&Meta35 { value: 7 })?;
    writer.write_header_block(&Meta36 { value: 7 })?;
    writer.write_header_block(&Meta37 { value: 7 })?;
    writer.write_header_block(&Meta38 { value: 7 })?;
    writer.write_header_block(&Meta39 { value: 7 })?;
    writer.write_header_block(&Meta40 { value: 7 })?;
    writer.write_header_block(&Meta41 { value: 7 })?;
    writer.write_header_block(&Meta42 { value: 7 })?;
    writer.write_header_block(&Meta43 { value: 7 })?;
    writer.write_header_block(&Meta44 { value: 7 })?;
    writer.write_header_block(&Meta45 { value: 7 })?;
    writer.write_header_block(&Meta46 { value: 7 })?;
    writer.write_header_block(&Meta47 { value: 7 })?;
    writer.write_header_block(&Meta48 { value: 7 })?;
    writer.write_header_block(&Meta49 { value: 7 })?;
    writer.write_header_block(&Meta50 { value: 7 })?;
    writer.write_header_block(&Meta51 { value: 7 })?;
    writer.write_header_block(&Meta52 { value: 7 })?;
    writer.write_header_block(&Meta53 { value: 7 })?;
    writer.write_header_block(&Meta54 { value: 7 })?;
    writer.write_header_block(&Meta55 { value: 7 })?;
    writer.write_header_block(&Meta56 { value: 7 })?;
    writer.write_header_block(&Meta57 { value: 7 })?;
    writer.write_header_block(&Meta58 { value: 7 })?;
    writer.write_header_block(&Meta59 { value: 7 })?;
    writer.write_header_block(&Meta60 { value: 7 })?;
    writer.write_header_block(&Meta61 { value: 7 })?;
    writer.write_header_block(&Meta62 { value: 7 })?;
    writer.write_header_block(&Meta63 { value: 7 })?;
    if shuffled {
        writer.write_header_block(&Meta63 { value: 7 })?;
        writer.write_header_block(&Meta62 { value: 7 })?;
        writer.write_header_block(&Meta61 { value: 7 })?;
        writer.write_header_block(&Meta60 { value: 7 })?;
        writer.write_header_block(&Meta59 { value: 7 })?;
        writer.write_header_block(&Meta58 { value: 7 })?;
        writer.write_header_block(&Meta57 { value: 7 })?;
        writer.write_header_block(&Meta56 { value: 7 })?;
        writer.write_header_block(&Meta55 { value: 7 })?;
        writer.write_header_block(&Meta54 { value: 7 })?;
        writer.write_header_block(&Meta53 { value: 7 })?;
        writer.write_header_block(&Meta52 { value: 7 })?;
        writer.write_header_block(&Meta51 { value: 7 })?;
        writer.write_header_block(&Meta50 { value: 7 })?;
        writer.write_header_block(&Meta49 { value: 7 })?;
        writer.write_header_block(&Meta48 { value: 7 })?;
        writer.write_header_block(&Meta47 { value: 7 })?;
        writer.write_header_block(&Meta46 { value: 7 })?;
        writer.write_header_block(&Meta45 { value: 7 })?;
        writer.write_header_block(&Meta44 { value: 7 })?;
        writer.write_header_block(&Meta43 { value: 7 })?;
        writer.write_header_block(&Meta42 { value: 7 })?;
        writer.write_header_block(&Meta41 { value: 7 })?;
        writer.write_header_block(&Meta40 { value: 7 })?;
        writer.write_header_block(&Meta39 { value: 7 })?;
        writer.write_header_block(&Meta38 { value: 7 })?;
        writer.write_header_block(&Meta37 { value: 7 })?;
        writer.write_header_block(&Meta36 { value: 7 })?;
        writer.write_header_block(&Meta35 { value: 7 })?;
        writer.write_header_block(&Meta34 { value: 7 })?;
        writer.write_header_block(&Meta33 { value: 7 })?;
        writer.write_header_block(&Meta32 { value: 7 })?;
        writer.write_header_block(&Meta31 { value: 7 })?;
        writer.write_header_block(&Meta30 { value: 7 })?;
        writer.write_header_block(&Meta29 { value: 7 })?;
        writer.write_header_block(&Meta28 { value: 7 })?;
        writer.write_header_block(&Meta27 { value: 7 })?;
        writer.write_header_block(&Meta26 { value: 7 })?;
        writer.write_header_block(&Meta25 { value: 7 })?;
        writer.write_header_block(&Meta24 { value: 7 })?;
        writer.write_header_block(&Meta23 { value: 7 })?;
        writer.write_header_block(&Meta22 { value: 7 })?;
        writer.write_header_block(&Meta21 { value: 7 })?;
        writer.write_header_block(&Meta20 { value: 7 })?;
        writer.write_header_block(&Meta19 { value: 7 })?;
        writer.write_header_block(&Meta18 { value: 7 })?;
        writer.write_header_block(&Meta17 { value: 7 })?;
        writer.write_header_block(&Meta16 { value: 7 })?;
        writer.write_header_block(&Meta15 { value: 7 })?;
        writer.write_header_block(&Meta14 { value: 7 })?;
        writer.write_header_block(&Meta13 { value: 7 })?;
        writer.write_header_block(&Meta12 { value: 7 })?;
        writer.write_header_block(&Meta11 { value: 7 })?;
        writer.write_header_block(&Meta10 { value: 7 })?;
        writer.write_header_block(&Meta9 { value: 7 })?;
        writer.write_header_block(&Meta8 { value: 7 })?;
        writer.write_header_block(&Meta7 { value: 7 })?;
        writer.write_header_block(&Meta6 { value: 7 })?;
        writer.write_header_block(&Meta5 { value: 7 })?;
        writer.write_header_block(&Meta4 { value: 7 })?;
        writer.write_header_block(&Meta3 { value: 7 })?;
        writer.write_header_block(&Meta2 { value: 7 })?;
        writer.write_header_block(&Meta1 { value: 7 })?;
        writer.write_header_block(&Meta0 { value: 7 })?;
    }
    drop(writer);
    let reader = ManyHeader::open_readonly(&path)?;
    let start = Instant::now();
    let mut total = 0;
    for _ in 0..reads {
        total += black_box(reader.read_header_block::<Meta63>()?.unwrap().value);
    }
    assert_eq!(total, 7 * reads);
    println!(
        "{{\"reads\":{reads},\"slots\":64,\"seconds\":{},\"total\":{total}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
