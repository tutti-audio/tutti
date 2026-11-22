//! A sampler voice in the live graph: cost class, engine clock and tempo map.

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod cost_classes;
mod graph_engine_clock;
mod loop_tempo_map;
mod tempo_map;
