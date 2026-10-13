use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 415, kind = "variable", key = "id-number")]
struct InvalidKey {
    #[stratava(field_id = 1)]
    id: u32,
}

fn main() {}
