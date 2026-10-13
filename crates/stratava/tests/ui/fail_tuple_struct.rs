use stratava::StratavaBlock;

#[derive(Clone, StratavaBlock)]
#[stratava(id = 412, kind = "fixed")]
struct TupleStruct(u32);

fn main() {}
