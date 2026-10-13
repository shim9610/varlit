use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 411, kind = "fixed", endian = "middle")]
struct UnsupportedEndian {
    value: u32,
}

fn main() {}
