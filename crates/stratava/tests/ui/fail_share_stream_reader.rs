use std::sync::Arc;
use stratava::StratavaStreamReader;

fn share(reader: Arc<StratavaStreamReader>) {
    std::thread::spawn(move || {
        let _ = reader.events();
    });
}

fn main() {}
