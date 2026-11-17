#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| tutti_fuzz::run("ci_decode", data));
