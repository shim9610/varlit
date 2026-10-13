use std::sync::Arc;
use stratava::StratavaIndexedReader;

fn share(reader: Arc<StratavaIndexedReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
