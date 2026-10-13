#![no_main]

use libfuzzer_sys::fuzz_target;
use stratava_fuzz::digest::run_digest;

fuzz_target!(|data: &[u8]| run_digest(data));
