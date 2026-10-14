use varlit::VarlitBlock;

#[derive(Clone, VarlitBlock)]
#[varlit(id = 417, kind = "fixed")]
struct FixedDefault {
    #[varlit(default)]
    value: u32,
}

fn main() {}
