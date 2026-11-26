#![no_main]

use libfuzzer_sys::fuzz_target;
use varlit_fuzz::stream::run_damage;

fuzz_target!(|data: &[u8]| run_damage(data));
