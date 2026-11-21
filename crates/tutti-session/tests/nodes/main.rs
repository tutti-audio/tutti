//! Nodes through the session: mix, mode, multiband params, reverbs and the sampler. The binary installs
//! `RtAllocCheck`, so an allocation inside an `assert_no_alloc` closure aborts.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

mod bus_strip;
mod multiband_params;
mod node_mix;
mod node_mode;
mod param_units;
mod reverb_nodes;
mod silenced_event;
// A sampler node the graph adds reads the session's tempo map (doc 020, stage 5).
#[cfg(feature = "sampler")]
mod sampler_tempo;
// A clip is a placed sample that is a graph node (the sampler builder).
#[cfg(feature = "sampler")]
mod clip_node;
// A stream or a clip opened on the spawner lands in `update()` (doc 030, A5 and A6).
#[cfg(feature = "sampler")]
mod sampler_loads;
// A load is a value and its handle the app's (`load_audio`).
#[cfg(feature = "sampler")]
mod audio_loads;
// A load and a disk stream on an `f64` session.
#[cfg(feature = "sampler")]
mod f64_loads;
// A voice of either tier arrives where the compiler puts it (doc 025 stage 5).
#[cfg(feature = "sampler")]
mod tier_alignment;
// A sampler voice's window written through the session's verbs.
#[cfg(feature = "sampler")]
mod sampler_values;
