use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 411, kind = "fixed", endian = "middle")]
struct UnsupportedEndian {
    value: u32,
}

fn main() {}
