use stratava::StratavaBlock;

#[derive(StratavaBlock)]
#[stratava(id = 1, kind = "fixed")]
struct GenericBlock<T> {
    value: T,
}

fn main() {}
