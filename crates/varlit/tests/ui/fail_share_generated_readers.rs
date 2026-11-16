use std::sync::Arc;
use varlit::varlit_format;

varlit_format! {
    pub format ThreadOwned {
        magic: b"THRD";
        version: 1;
        blocks {
            fixed Item(id = 1, key = [key]) { key: u64 }
        }
    }
}

fn stream(reader: Arc<ThreadOwnedStreamReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
