use varlit::varlit_format;

varlit_format! {
    pub format TooLoose {
        magic: b"LOOSE";
        version: 1;
        keyed_storage: in_place(free_space = 9%);
        blocks {
            variable Item(id = 1, key = [id]) { id: u64 }
        }
    }
}

fn main() {}
