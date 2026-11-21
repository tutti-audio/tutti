//! The file formats: dither, decode oracles, resampling and round trips.

#[path = "../support/mod.rs"]
mod support;

mod dither_stats;
mod generic;
mod noise_shaping;
mod oracle_decode;
mod oracle_resample;
mod roundtrip;
