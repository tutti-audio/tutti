//! Measurement accuracy against analytic figures: loudness, pitch and tonal analysis.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod loudness_accuracy;
mod pitch_accuracy;
mod tonal;
