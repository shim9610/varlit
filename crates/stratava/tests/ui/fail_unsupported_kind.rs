use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 410, kind = "stream")]
struct UnsupportedKind {
    value: u32,
}

fn main() {}
