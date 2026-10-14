use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 414, kind = "variable", key = "")]
struct EmptyKey {
    #[varlit(field_id = 1)]
    id: u32,
}

fn main() {}
