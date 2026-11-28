//! A sampler voice: gain, loops, seeks, commands and the two tiers.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

mod block_length_invariance;
mod disk_placement;
mod f64_graph;
mod file_formats;
mod frame_exact_entry;
mod gain;
mod handle_slot_race;
mod loops_and_handles;
mod node_config;
mod reverse_and_loop;
mod rt_no_alloc;
mod stretched_onset;
mod stretched_seek;
mod tier_parity;
mod vocoder_voice;
mod voice_gain_survives_commit;
mod voice_node_commands;
mod wrap_fade_modes;

/// Aborts inside an `RtScope`; inert elsewhere.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;
