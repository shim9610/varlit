use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(kind = "fixed")]
struct MissingId {
    value: u32,
}

fn main() {}
