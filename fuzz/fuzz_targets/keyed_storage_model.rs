#![no_main]

use libfuzzer_sys::fuzz_target;
use varlit_fuzz::keyed::run;

fuzz_target!(|data: &[u8]| run(data));
