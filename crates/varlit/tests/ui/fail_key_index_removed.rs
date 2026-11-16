use varlit::varlit_format;

// `key_index` was removed with the indexed primary; a format that still
// declares it is refused with a pointer to the design that replaces it.
varlit_format! {
    pub format KeyIndexRemoved {
        magic: b"KIRM";
        version: 1;
        blocks {
            variable Item(id = 1, key = [id], key_index = disk) {
                id: u64,
            }
        }
    }
}

fn main() {}
