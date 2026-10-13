use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 401, kind = "variable")]
struct BadFields {
    #[stratava(field_id = 1)]
    left: u32,
    #[stratava(field_id = 1)]
    right: u32,
}

fn main() {}
