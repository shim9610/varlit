#![no_main]
use libfuzzer_sys::fuzz_target;
#[allow(dead_code)]
#[path = "../../crates/varlit/tests/support/matrix_reuse_model.rs"]
mod model;
fuzz_target!(|data: &[u8]| {
    model::run(data, true);
});
