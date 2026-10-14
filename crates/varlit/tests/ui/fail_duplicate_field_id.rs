use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 401, kind = "variable")]
struct BadFields {
    #[varlit(field_id = 1)]
    left: u32,
    #[varlit(field_id = 1)]
    right: u32,
}

fn main() {}
