use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 402, kind = "variable")]
struct BadZero {
    #[stratava(field_id = 0)]
    value: u32,
}

fn main() {}
