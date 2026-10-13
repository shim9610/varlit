use stratava::stratava_format;

stratava_format! {
    pub format MissingKeyFormat {
        magic: b"KIDX";
        version: 1;
        blocks {
            fixed Item(id = 1, key_index = memory) {
                value: u64,
            }
        }
    }
}

fn main() {}
