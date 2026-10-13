use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 417, kind = "fixed")]
struct FixedDefault {
    #[stratava(default)]
    value: u32,
}

fn main() {}
