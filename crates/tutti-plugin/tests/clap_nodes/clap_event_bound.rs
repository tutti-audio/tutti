//! **A CLAP node declares the event-input bound its list was built with**, in
//! process: the density's figure for the graph's block, never past what the
//! activation's input list holds, so the graph's cut is the only cut.
//!
//! The load activates at the loader's largest block (4096 frames), so the
//! list holds 4096 events (one per frame, `EventDensity::per_frame(1)`).

#![cfg(feature = "clap")]

use crate::clap_probe::SAMPLE_RATE;
use tutti_fixtures::{machine_lock, main_thread};

use tutti_clap_host::ClapLoaded;
use tutti_graph::{Node, Prepare};
use tutti_plugin_types::Load;
use tutti_types::{MaxBlock, SampleRate, Samples};

/// The node's declared bound after a `prepare` at a graph block of `frames`.
fn declared_at(node: &mut impl Node, frames: usize) -> Option<u32> {
    node.prepare(&Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples(frames)),
    ));
    node.shape().event_input_capacity.map(|n| n.get())
}

#[test]
fn the_declared_bound_is_the_density_up_to_the_activations_room() {
    let _lock = machine_lock();
    let (client, _handle) = tutti_plugin_host::load_client::<ClapLoaded>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        tutti_fixtures::clap_probe(),
        &Load::new(),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let mut node = client.bind(
        tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
    );
    assert_eq!(declared_at(&mut node, 64), Some(256), "the density's floor");
    assert_eq!(declared_at(&mut node, 2048), Some(2048), "one a frame");
    assert_eq!(
        declared_at(&mut node, 8192),
        Some(4096),
        "a graph block past the activation's is held to the activation's room"
    );
}
