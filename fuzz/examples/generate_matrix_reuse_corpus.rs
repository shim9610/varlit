#[allow(dead_code)]
#[path = "../../crates/varlit/tests/support/matrix_reuse_model.rs"]
mod model;
fn main() {
    for (name, publication) in [
        ("matrix_reuse_state_machine", false),
        ("matrix_reuse_publication", true),
    ] {
        let dir = std::path::Path::new("corpus").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, seed) in model::seeds(publication).iter().enumerate() {
            std::fs::write(dir.join(format!("seed-{i:03}")), seed).unwrap();
        }
    }
}
