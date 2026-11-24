//! What a host write to a `REQUIRES_PROCESS` parameter reaches while no block has
//! run, for the shared CLAP plugin in process and served, read back through the
//! plugin's own record of what the host sent.
//!
//! A `REQUIRES_PROCESS` parameter of an active plugin is not flushed: the record
//! has no flush for it before a block, and the first rendered block delivers it,
//! in process. (The shared plugin declares a bypass parameter, so its node
//! bypasses itself and is never parked: a parked node is the read-after-write
//! suite's, on the probe.)
//!
//! Needs `cargo build -p tutti-plugin-server` first.

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_fixtures::{main_thread, pumped_work};
use tutti_graph::{GraphBuilder, IntoNode, Prepare, Renderer};
use tutti_plugin::PluginClient;
use tutti_plugin_contract::spec::Param;
use tutti_plugin_contract::{Clap, Format, Kind, Transcript, clap_path};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::IpcConfig;
use tutti_plugin_types::{PluginRef, StateUse};
use tutti_units::{Normalized, SampleRate, Samples};

const WRITTEN: f64 = 0.25;
const BLOCK: usize = 64;

fn tempo() -> tutti_timeline::TempoReader {
    tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo")
}

/// `node` as the only node of a graph, and its key.
fn graph<N: IntoNode<f32>>(node: N) -> Renderer {
    let outputs = ChannelLayout::from_count(2);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, outputs);
    let (key, _controls) = g.add_with_controls(node);
    g.pipe_output(key);
    g.renderer(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(BLOCK)),
    ))
    .expect("a one-plugin graph compiles")
}

/// What the plugin was told, from a saved state.
fn told(handle: &PluginHandle) -> Transcript {
    let state = handle.save_state(StateUse::Project).expect("saves");
    Transcript::parse(&Clap::extract_blob(&state))
}

/// **A `REQUIRES_PROCESS` write of an active plugin is not flushed**: a save
/// before any block finds none, and the first rendered block delivers it, as a
/// value in process.
fn a_requires_process_write_waits_for_a_block(renderer: &mut Renderer, handle: &PluginHandle) {
    handle
        .send_parameter(Param::Aux.id(), Normalized::of(WRITTEN))
        .expect("the plugin lists the parameter");
    let t = told(handle);
    assert_eq!(
        t.of(Kind::Flush).count(),
        0,
        "nothing was flushed: {:?}",
        t.events
    );
    assert!(
        t.of(Kind::Param).all(|e| e.id != Param::Aux.id().get()),
        "the write waits for a block: {:?}",
        t.events
    );
    let _ = renderer.render(BLOCK);
    let t = told(handle);
    assert_eq!(t.of(Kind::Flush).count(), 0, "still nothing flushed");
    assert!(
        t.of(Kind::Param)
            .any(|e| e.id == Param::Aux.id().get() && e.value == WRITTEN),
        "the first block delivered it: {:?}",
        t.events
    );
}

fn in_process(case: fn(&mut Renderer, &PluginHandle)) {
    let _lock = tutti_fixtures::machine_lock();
    let (client, handle) = tutti_plugin_host::load_client::<tutti_clap_host::ClapLoaded, f32>(
        main_thread(),
        &pumped_work(),
        &PluginRef::file(clap_path()),
        SampleRate::SR_48K,
    )
    .expect("the shared plugin loads in process");
    let mut renderer = graph(client.bind(tempo()));
    case(&mut renderer, &handle);
    std::mem::forget((renderer, handle));
}

fn served(case: fn(&mut Renderer, &PluginHandle)) {
    let _lock = tutti_fixtures::machine_lock();
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let client = PluginClient::new(
        IpcConfig::new(),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &tutti_plugin_db::Loadable::uncatalogued(PluginRef::file(clap_path())),
        SampleRate::SR_48K,
    )
    .expect("the server loads the shared plugin");
    let handle = client.handle();
    let mut renderer = graph(client.bind(tempo()));
    case(&mut renderer, &handle);
}

#[test]
fn a_requires_process_write_waits_for_a_block_in_process() {
    in_process(a_requires_process_write_waits_for_a_block);
}

#[test]
fn a_requires_process_write_waits_for_a_block_out_of_process() {
    served(a_requires_process_write_waits_for_a_block);
}
