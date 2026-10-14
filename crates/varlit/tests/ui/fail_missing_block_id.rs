use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(kind = "fixed")]
struct MissingId {
    value: u32,
}

fn main() {}
