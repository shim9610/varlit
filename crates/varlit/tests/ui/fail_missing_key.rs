use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 404, kind = "variable", key = "missing")]
struct MissingKey {
    #[varlit(field_id = 1)]
    id: u64,
}

fn main() {}
