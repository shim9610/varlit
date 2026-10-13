use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 403, kind = "variable", key = "id,id")]
struct DuplicateKey {
    #[stratava(field_id = 1)]
    id: u64,
}

fn main() {}
