#![no_main]
#[path = "../../crates/stratava/tests/support/native_redb.rs"]
mod oracle;
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    oracle::run(data);
});
