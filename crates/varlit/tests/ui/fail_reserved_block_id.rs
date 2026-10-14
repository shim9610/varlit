use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 4294967040, kind = "fixed")]
struct Reserved {
    value: u32,
}

fn main() {}
