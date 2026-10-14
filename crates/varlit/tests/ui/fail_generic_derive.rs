use varlit::VarlitBlock;

#[derive(VarlitBlock)]
#[varlit(id = 1, kind = "fixed")]
struct GenericBlock<T> {
    value: T,
}

fn main() {}
