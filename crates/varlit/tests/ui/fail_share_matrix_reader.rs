use std::sync::Arc;
use varlit::VarlitReader;

fn share(reader: Arc<VarlitReader>) {
    std::thread::spawn(move || reader.matrix_aux_len("thumbnail"));
}

fn main() {}
