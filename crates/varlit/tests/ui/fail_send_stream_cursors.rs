use varlit::{StreamEvents, StreamingBlocks, varlit_format};
varlit_format! {
    pub format Feed {
        magic: b"SEND";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}
fn send_events(cursor: StreamEvents) {
    std::thread::spawn(move || { for event in cursor { event.unwrap(); } });
}
fn send_blocks(cursor: StreamingBlocks<Sample>) {
    std::thread::spawn(move || {
        let mut cursor = cursor;
        let mut out = Sample { value: 0 };
        while cursor.next_into(&mut out).unwrap() {}
    });
}
fn main() {}
