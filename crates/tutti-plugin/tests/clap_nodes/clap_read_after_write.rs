//! **A parameter read made right after a host parameter write returns the
//! written value**, with no audio block between them, for the reference CLAP
//! plugin in process and served. The plugin is active (its node is in a
//! prepared graph); the write is `PluginHandle::send_parameter`, the read is
//! the mirror's (`ParamReads::parameter_value`).
//!
//! Needs `cargo build -p tutti-plugin-server` first.

#![cfg(feature = "clap")]

use crate::clap_probe::{BLOCK, ProbeEnv, SAMPLE_RATE, load_probe};
use crate::param_probe::extra;
use crate::param_reads::ParamReads as _;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_clap_host::ClapLoaded;
use tutti_fixtures::{machine_lock, main_thread};
use tutti_graph::{GraphBuilder, NodeKey, NodeMode, Prepare, Renderer};
use tutti_units::{Normalized, SampleRate, Samples};

/// Exact in `f32`, and not the probe's default of 0.
const WRITTEN: f64 = 0.25;

fn tempo() -> tutti_timeline::TempoReader {
    tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo")
}

/// **In process, a read right after a write sees the write**, no block between.
/// The read runs on a pumped main-thread collector, as an app's main tick would.
#[test]
fn a_read_after_a_write_sees_the_write_in_process() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().extra_params(1);
    let (client, handle) = tutti_plugin_host::load_client::<ClapLoaded, f32>(
        main_thread(),
        &tutti_fixtures::pumped_work(),
        &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let outputs = 2;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(outputs));
    let (key, _controls) = g.add_with_controls(client.bind(tempo()));
    g.pipe_output(key);
    let _renderer = g
        .renderer(Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");

    assert_eq!(
        handle.parameter_value(extra(0)),
        Some(Normalized::of(0.0)),
        "the default is read before the write"
    );
    handle
        .send_parameter(extra(0), Normalized::of(WRITTEN))
        .expect("the plugin lists the parameter");
    assert_eq!(
        handle.parameter_value(extra(0)),
        Some(Normalized::of(WRITTEN)),
        "a read with no block since the write returns the written value"
    );
}

/// **Served, a read right after a write sees the write**, no block between.
#[test]
fn a_read_after_a_write_sees_the_write_out_of_process() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().extra_params(1);
    let probe = load_probe(SAMPLE_RATE);
    let outputs = probe.client.outputs();
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(outputs));
    let (key, _controls) = g.add_with_controls(probe.client.bind(tempo()));
    g.pipe_output(key);
    let _renderer = g
        .renderer(Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");

    assert_eq!(
        probe.handle.parameter_value(extra(0)),
        Some(Normalized::of(0.0)),
        "the default is read before the write"
    );
    probe
        .handle
        .send_parameter(extra(0), Normalized::of(WRITTEN))
        .expect("the plugin lists the parameter");
    assert_eq!(
        probe.handle.parameter_value(extra(0)),
        Some(Normalized::of(WRITTEN)),
        "a read with no block since the write returns the written value"
    );
}

/// Bypasses the node and renders until it is parked for good: its fade done,
/// its `process` no longer called.
fn park(renderer: &mut Renderer, key: NodeKey) {
    let editor = renderer.editor_mut();
    editor
        .set_mode(key, NodeMode::Bypassed)
        .expect("the probe has no bypass of its own, so the node bypasses");
    editor.commit().expect("the mode commits");
    for _ in 0..40 {
        let _ = renderer.render(BLOCK);
    }
}

/// **A write made to a parked node reaches the plugin in a parked block**: a
/// read after one parked block, with no rendered block, shows the write.
#[test]
fn a_write_to_a_parked_node_is_read_after_a_parked_block_in_process() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().extra_params(1);
    let (client, handle) = tutti_plugin_host::load_client::<ClapLoaded, f32>(
        main_thread(),
        &tutti_fixtures::pumped_work(),
        &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(2));
    let (key, _controls) = g.add_with_controls(client.bind(tempo()));
    g.pipe_output(key);
    let mut renderer = g
        .renderer(Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");
    park(&mut renderer, key);

    handle
        .send_parameter(extra(0), Normalized::of(WRITTEN))
        .expect("the plugin lists the parameter");
    let _ = renderer.render(BLOCK);
    assert_eq!(
        handle.parameter_value(extra(0)),
        Some(Normalized::of(WRITTEN)),
        "a read after one parked block returns the written value"
    );
}

/// **Served, a write made to a parked node is read after a parked block.**
#[test]
fn a_write_to_a_parked_node_is_read_after_a_parked_block_out_of_process() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().extra_params(1);
    let probe = load_probe(SAMPLE_RATE);
    let outputs = probe.client.outputs();
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(outputs));
    let (key, _controls) = g.add_with_controls(probe.client.bind(tempo()));
    g.pipe_output(key);
    let mut renderer = g
        .renderer(Prepare::new(
            SampleRate::of(SAMPLE_RATE),
            MaxBlock::of(Samples::new(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");
    park(&mut renderer, key);

    probe
        .handle
        .send_parameter(extra(0), Normalized::of(WRITTEN))
        .expect("the plugin lists the parameter");
    let _ = renderer.render(BLOCK);
    assert_eq!(
        probe.handle.parameter_value(extra(0)),
        Some(Normalized::of(WRITTEN)),
        "a read after one parked block returns the written value"
    );
}
