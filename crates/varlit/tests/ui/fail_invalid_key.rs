use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 415, kind = "variable", key = "id-number")]
struct InvalidKey {
    #[varlit(field_id = 1)]
    id: u32,
}

fn main() {}
