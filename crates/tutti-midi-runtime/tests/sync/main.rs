//! MIDI sync: the clock following the tempo map and the clock decoder.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[path = "../common/mod.rs"]
mod common;

mod clock_drift;
mod clock_follower;
mod clock_tempo_map;
mod decoder_no_alloc;
mod machine_control;
mod mtc_follow;
mod sync_out;
