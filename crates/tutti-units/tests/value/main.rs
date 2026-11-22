//! The value types' range contracts.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod beats_per_cycle_range;
mod bits_range;
mod bpm_range;
mod distance_range;
mod modulation_range;
mod normalized_range;
mod playback_rate_range;
mod sample_rate_range;
