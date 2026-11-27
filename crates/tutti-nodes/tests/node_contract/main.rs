//! What every node owes the graph: cost class, mix, mode, latency, forks and the General MIDI effects.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod contract;
mod cost_classes;
mod general_midi_effects;
mod isolate_snapshots;
mod kernel_nonfinite;
mod matrix_tail;
mod node_config;
mod node_mix;
mod node_mode;
mod reported_latency;
