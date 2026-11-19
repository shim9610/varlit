#![no_main]

use libfuzzer_sys::fuzz_target;
use varlit_fuzz::keyed::run_corrupt;

fuzz_target!(|data: &[u8]| run_corrupt(data));
