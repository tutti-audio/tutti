//! **A CLAP node declares the event-input bound its list holds**, in process:
//! the node's own MIDI intake (256 events), which is what the activation's
//! input list is built for, at every block the graph is prepared for.

#![cfg(feature = "clap")]

use crate::clap_probe::SAMPLE_RATE;
use tutti_fixtures::{machine_lock, main_thread};

use tutti_buffers::MaxBlock;
use tutti_clap_host::ClapLoaded;
use tutti_graph::{Node, Prepare};
use tutti_units::{SampleRate, Samples};

/// The node's declared bound after a `prepare` at a graph block of `frames`.
fn declared_at(node: &mut impl Node, frames: usize) -> Option<u32> {
    node.prepare(&Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples::new(frames)),
    ));
    node.shape().event_input_capacity.map(|n| n.get())
}

#[test]
fn the_declared_bound_is_what_the_list_holds_and_the_intake_takes() {
    let _lock = machine_lock();
    let (client, _handle) = tutti_plugin_host::load_client::<ClapLoaded, f32>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let mut node = client
        .bind(tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo"));
    // The activation's list holds 256 events (the intake's size), whatever
    // the block the graph is prepared for.
    for frames in [64, 2048, 8192] {
        assert_eq!(declared_at(&mut node, frames), Some(256), "{frames} frames");
    }
}
