use std::sync::Arc;
use stratava::StratavaReader;

fn share(reader: Arc<StratavaReader>) {
    std::thread::spawn(move || reader.matrix_aux_len("thumbnail"));
}

fn main() {}
