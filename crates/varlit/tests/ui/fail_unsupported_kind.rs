use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 410, kind = "stream")]
struct UnsupportedKind {
    value: u32,
}

fn main() {}
