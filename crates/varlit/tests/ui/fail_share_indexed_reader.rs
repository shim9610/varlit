use std::sync::Arc;
use varlit::VarlitIndexedReader;

fn share(reader: Arc<VarlitIndexedReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
