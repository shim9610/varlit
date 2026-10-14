use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 412, kind = "fixed")]
struct TupleStruct(u32);

fn main() {}
