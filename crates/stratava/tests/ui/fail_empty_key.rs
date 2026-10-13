use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 414, kind = "variable", key = "")]
struct EmptyKey {
    #[stratava(field_id = 1)]
    id: u32,
}

fn main() {}
