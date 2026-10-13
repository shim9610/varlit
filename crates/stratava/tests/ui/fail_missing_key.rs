use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 404, kind = "variable", key = "missing")]
struct MissingKey {
    #[stratava(field_id = 1)]
    id: u64,
}

fn main() {}
