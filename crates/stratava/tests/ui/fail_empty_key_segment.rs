use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 416, kind = "variable", key = "id,")]
struct EmptyKeySegment {
    #[stratava(field_id = 1)]
    id: u32,
}

fn main() {}
