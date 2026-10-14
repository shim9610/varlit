use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 402, kind = "variable")]
struct BadZero {
    #[varlit(field_id = 0)]
    value: u32,
}

fn main() {}
