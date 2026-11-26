//! The file formats: dither, decode oracles, resampling and round trips.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../support/mod.rs"]
mod support;

mod dither_stats;
mod noise_shaping;
mod oracle_decode;
mod oracle_resample;
mod roundtrip;
