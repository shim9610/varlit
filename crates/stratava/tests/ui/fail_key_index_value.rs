use stratava::stratava_format;

stratava_format! {
    pub format InvalidKeyIndexFormat {
        magic: b"KIDV";
        version: 1;
        blocks {
            variable Item(id = 1, key = [id], key_index = cached) {
                id: u64,
            }
        }
    }
}

fn main() {}
