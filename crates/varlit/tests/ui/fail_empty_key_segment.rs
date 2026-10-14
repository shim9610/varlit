use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 416, kind = "variable", key = "id,")]
struct EmptyKeySegment {
    #[varlit(field_id = 1)]
    id: u32,
}

fn main() {}
