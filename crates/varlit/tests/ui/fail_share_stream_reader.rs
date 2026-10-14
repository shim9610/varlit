use std::sync::Arc;
use varlit::VarlitStreamReader;

fn share(reader: Arc<VarlitStreamReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
