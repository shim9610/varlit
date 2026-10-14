use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 403, kind = "variable", key = "id,id")]
struct DuplicateKey {
    #[varlit(field_id = 1)]
    id: u64,
}

fn main() {}
