#![no_main]

use libfuzzer_sys::fuzz_target;
use stratava_fuzz::sidecar::run_state_machine;

fuzz_target!(|data: &[u8]| run_state_machine(data));
