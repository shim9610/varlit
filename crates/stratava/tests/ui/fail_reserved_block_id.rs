use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 4294967040, kind = "fixed")]
struct Reserved {
    value: u32,
}

fn main() {}
