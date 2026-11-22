//! A plugin's per-block status in the plugin node: CLAP's
//! *sleep* is the node's `report_tail(Tail::None)` and `Status::Idle`, and
//! nothing else narrows or parks.
//!
//! The plugin is `tutti_plugin_types::fake`'s: a voice that sounds `ring`
//! frames after each trigger (a non-silent input frame, a MIDI event at its
//! frame) and says what CLAP's process status would, hosted through
//! `client_over` as a served plugin is. No format and no server is involved.

use std::sync::Arc;
use tutti_buffers::{Float, FloatWidth, Planes};

use tutti_fixtures::main_thread;

use tutti_buffers::MaxBlock;
use tutti_fixtures::midi;
use tutti_graph::contract::{self, Direct, assert_declared_tail, drive_input};
use tutti_graph::{
    Editor, Env, EventCapacity, EventEdge, EventInletNode, EventInputPort, EventOutputPort,
    Executor, Node, NodeKey, OutputPort, PlayClock, PortSource, Prepare as GraphPrepare, Status,
    TransportChanges, TransportPosition, Unforkable,
};
use tutti_plugin_host::{ClientOverError, PluginNode, client_over};
use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
use tutti_plugin_types::{
    ChannelLayout, Control, Loaded, MAX_BLOCK_CHANNELS, PluginBus, PluginTooWide, Prepare,
};
use tutti_tempo::TempoMap;
use tutti_timeline::TempoReader;
use tutti_ump::{MidiChannel, U7};
use tutti_units::{Bpm, Frame, SampleRate, Samples, Tail};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

/// The fake's node: a voice of `ring` frames declaring `tail` as its ceiling.
fn node(ring: Samples, tail: Samples) -> (PluginNode<FakeLoaded>, FakePlugin) {
    node_at(ring, tail, FloatWidth::F32)
}

/// [`node`], activated at `width` and hosted at the graph float `F`.
fn node_at<F: Float>(
    ring: Samples,
    tail: Samples,
    width: FloatWidth,
) -> (PluginNode<FakeLoaded, F>, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    fake.ring_out(ring.get());
    fake.set_tail(tail);
    let main = main_thread();
    let wiring = loaded.wiring(main);
    let descriptor = loaded.info().clone();
    let prepare =
        Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block")).with_format(width);
    let Ok((processor, controller)) = loaded.activate(main, prepare) else {
        panic!("the fake activates");
    };
    let client = client_over::<FakeLoaded, F>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        [],
        0,
        None,
    )
    .expect("the fake is within a block's width");
    let map = TempoMap::constant(Bpm::of(120.0)).expect("a tempo");
    (client.bind(TempoReader::fixed(Arc::new(map))), fake)
}

fn env(frame: u64) -> Env {
    Env {
        frame: Frame::new(frame),
        sample_rate: SR,
        block_len: Samples::new(BLOCK),
        transport: TransportPosition::default(),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    }
}

/// An impulse on both input channels at the block's first frame, or silence.
fn input(impulse: bool) -> Vec<f32> {
    let mut v = vec![0.0f32; BLOCK];
    v[0] = f32::from(u8::from(impulse));
    v
}

fn drive(node: &mut dyn Node, frame: u64, impulse: bool) -> contract::Driven {
    let x = input(impulse);
    drive_input(node, &env(frame), Planes::new(&[&x, &x]).unwrap(), &[], &[])
}

fn processed(fake: &FakePlugin) -> usize {
    fake.calls()
        .iter()
        .filter(|c| matches!(c, Call::Process(_)))
        .count()
}

/// **A plugin that sleeps reports no tail and idles; one that sounds says
/// nothing.** An impulse starts a 100-frame voice: blocks 0 and 1 sound and
/// leave the ceiling standing (`Modified`, nothing reported), block 2 is the
/// plugin's *sleep* (`Idle`, `Tail::None`).
#[test]
fn a_sleeping_plugin_reports_no_tail_and_idles() {
    let (node, _fake) = node(Samples::new(100), Samples::new(128));
    let mut node = contract::prepared(node, SR, BLOCK);
    for (block, impulse) in [(0u64, true), (1, false)] {
        let d = drive(&mut node, block * BLOCK as u64, impulse);
        assert_eq!(d.status, Some(Status::Modified), "block {block} sounds");
        assert_eq!(d.reported_tail, None, "block {block}: the ceiling stands");
        assert!(d.audio[0].iter().any(|&x| x != 0.0), "block {block}");
    }
    let d = drive(&mut node, 2 * BLOCK as u64, false);
    assert_eq!(d.status, Some(Status::Idle), "asleep");
    assert_eq!(d.reported_tail, Some(Tail::None));
    assert!(d.audio.iter().flatten().all(|&x| x == 0.0));
}

/// **The honest-tail contract holds for the node over a voice of 100 frames
/// declaring a 128-frame ceiling**: after the impulse the node is silent past
/// what it reports (its ceiling, since the sounding blocks say nothing).
#[test]
fn the_plugin_node_passes_the_honest_tail_contract() {
    let (node, _fake) = node(Samples::new(100), Samples::new(128));
    let mut node = contract::prepared(node, SR, BLOCK);
    assert_declared_tail(&mut node, SR);
}

/// **A plugin that says sleep over sound is not believed**: its audio is
/// heard, the node reports nothing and is not idle.
#[test]
fn a_plugin_that_sleeps_over_sound_is_not_idled() {
    let (node, fake) = node(Samples::new(100), Samples::new(128));
    fake.sleep_over_sound(true);
    let mut node = contract::prepared(node, SR, BLOCK);
    let d = drive(&mut node, 0, true);
    assert_eq!(d.status, Some(Status::Modified));
    assert_eq!(d.reported_tail, None);
    assert!(d.audio[0].contains(&0.5), "the sound is heard");
}

/// A voice of 100 frames behind the executor: an event inlet into the node's
/// event input, the node's two channels to the outputs.
fn graph(fake_ring: Samples) -> (Editor, Executor, tutti_graph::EventInletWriter, FakePlugin) {
    let (node, fake) = node(fake_ring, Samples::new(128));
    let (mut ed, mut exec) = Editor::new(GraphPrepare::new(SR, MaxBlock::of(Samples::new(BLOCK))));
    let (inlet, writer) = EventInletNode::pair(EventCapacity::of(64));
    let (inlet_key, plugin) = (NodeKey(1), NodeKey(2));
    ed.insert(inlet_key, "inlet", inlet);
    ed.insert(plugin, "fake", Unforkable(node));
    ed.spec_mut().connect_events(
        EventInputPort {
            node: plugin,
            port: 0,
        },
        EventEdge::Direct(EventOutputPort {
            node: inlet_key,
            port: 0,
        }),
    );
    ed.spec_mut()
        .topology
        .set_outputs((0..2).map(|port| PortSource::Node(OutputPort::new(plugin, port))));
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    (ed, exec, writer, fake)
}

fn render(exec: &mut Executor) -> [Vec<f32>; 2] {
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[],
        &mut [&mut l, &mut r],
    );
    [l, r]
}

fn note() -> tutti_ump::Ump {
    midi::m1_note_on(MidiChannel::FIRST, 60, U7::of(100))
}

/// **A parked plugin is woken by a note on its frame**, at a block's first
/// frame and inside a block. The plugin sleeps, so the executor parks the node
/// (its `process` stops being called); a note then lands at the first frame of
/// block 8 and, after the voice has rung out and the node parked again, at
/// frame 17 of block 20. Each is heard from exactly its frame, 100 frames of
/// 0.5, and silent before.
#[test]
fn a_parked_plugin_is_woken_by_a_note_on_its_frame() {
    let (_ed, mut exec, mut tx, fake) = graph(Samples::new(100));
    let mut heard = Vec::new();
    let mut calls = Vec::new();
    let (first, second) = (8 * BLOCK as u64, 20 * BLOCK as u64 + 17);
    tx.push(Frame::new(first), note()).expect("room");
    tx.push(Frame::new(second), note()).expect("room");
    for _ in 0..40 {
        let [l, r] = render(&mut exec);
        assert_eq!(l, r);
        heard.extend(l);
        calls.push(processed(&fake));
    }
    let sounding: Vec<usize> = (0..heard.len()).filter(|&i| heard[i] != 0.0).collect();
    let want: Vec<usize> = (first as usize..first as usize + 100)
        .chain(second as usize..second as usize + 100)
        .collect();
    assert_eq!(
        sounding, want,
        "each note exactly on its frame, 100 frames long"
    );
    assert!(heard.iter().all(|&x| x == 0.0 || x == 0.5));
    // Parked between: the plugin was not called for blocks 5..8 (asleep since
    // block 0, parked from block 1) nor across the quiet blocks after the
    // first note rang out (blocks 12..19).
    assert_eq!(calls[2], calls[7], "blocks 3..7 were skipped");
    assert_eq!(calls[14], calls[19], "blocks 15..19 were skipped");
    assert!(calls[8] > calls[7], "block 8 woke it");
}

/// The tail the executor parks on is the declared ceiling for a plugin that
/// never reports: a plugin node with a ceiling is not parked while its voice
/// rings, because it says *continue* then, and a sleeping one is parked at
/// once even though the ceiling is 128 frames.
#[test]
fn a_sleep_parks_before_the_ceiling_has_elapsed() {
    let (_ed, mut exec, _tx, fake) = graph(Samples::new(100));
    for _ in 0..10 {
        render(&mut exec);
    }
    // Called for the first block(s) only: asleep from the start, so parked
    // long before the 128-frame ceiling's two blocks could matter.
    assert!(processed(&fake) <= 2, "{} calls", processed(&fake));
}

/// **A plugin wider than a block serves gets no node.** Its wiring, as the
/// plugin reports it, has one channel more than `MAX_BLOCK_CHANNELS` in a
/// direction: `client_over` refuses it by name instead of handing the plugin
/// the first channels only.
#[test]
fn a_plugin_wider_than_a_block_is_refused_by_name() {
    let (_fake, loaded) = FakePlugin::new();
    let main = main_thread();
    let mut wiring = loaded.wiring(main);
    let descriptor = loaded.info().clone();
    let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
    let Ok((processor, controller)) = loaded.activate(main, prepare) else {
        panic!("the fake activates");
    };
    wiring.outputs = vec![PluginBus::main(ChannelLayout::clamped(
        MAX_BLOCK_CHANNELS + 1,
    ))];
    let refused = client_over::<FakeLoaded, f32>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        [],
        0,
        None,
    );
    let Err(error) = refused else {
        panic!("a plugin too wide has no client");
    };
    let wide = PluginTooWide {
        direction: "output",
        channels: MAX_BLOCK_CHANNELS + 1,
    };
    assert_eq!(error, ClientOverError::TooWide(wide));
    assert!(matches!(
        error.into_bridge(),
        tutti_plugin_types::IpcError::TooWide(got) if got == wide
    ));
}

/// **An `f64` graph hosts a plugin that processes 64-bit audio natively**: the
/// node is a `Node<f64>`, and the voice the fake starts on the impulse sounds
/// 0.5 for its ring of 100 frames, so a 64-frame block is 0.5 throughout and
/// the next one 0.5 for 36 frames and silence after.
#[test]
fn a_64_bit_graph_renders_a_64_bit_plugin_natively() {
    let (node, fake) = node_at::<f64>(Samples::new(100), Samples::new(128), FloatWidth::F64);
    let mut unit = Direct::with_format(node, SR, BLOCK);
    for channel in 0..2 {
        let mut input = unit.inputs_mut();
        let mut plane = input.plane_mut(channel);
        let samples = plane.samples_mut();
        samples.fill(0.0);
        samples[0] = 1.0;
    }
    assert_eq!(unit.block(), Status::Modified);
    assert!(unit.output().plane(0).samples().iter().all(|&x| x == 0.5));
    for channel in 0..2 {
        unit.inputs_mut().plane_mut(channel).samples_mut().fill(0.0);
    }
    unit.block();
    let second = unit.output().plane(0).samples().to_vec();
    assert!(second[..36].iter().all(|&x| x == 0.5), "36 frames remain");
    assert!(second[36..].iter().all(|&x| x == 0.0), "then silence");
    assert_eq!(processed(&fake), 2);
    // What the host sent, as the plugin recorded it: activated at 64-bit, and
    // both blocks of the sample type `f64`, none of `f32`.
    assert_eq!(fake.activated_width(), Some(FloatWidth::F64));
    assert_eq!(fake.blocks_at(FloatWidth::F64), 2);
    assert_eq!(fake.blocks_at(FloatWidth::F32), 0);
}

/// **A client is for the width its plugin was activated at**: one activated at
/// 32-bit is no 64-bit client, and the refusal names both widths.
#[test]
fn a_client_over_a_plugin_of_another_width_is_refused_by_name() {
    let (_fake, loaded) = FakePlugin::new();
    let main = main_thread();
    let wiring = loaded.wiring(main);
    let descriptor = loaded.info().clone();
    let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
    let Ok((processor, controller)) = loaded.activate(main, prepare) else {
        panic!("the fake activates");
    };
    let refused = client_over::<FakeLoaded, f64>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        [],
        0,
        None,
    );
    let Err(error) = refused else {
        panic!("a plugin of another width has no client");
    };
    assert_eq!(
        error,
        ClientOverError::Width {
            activated: FloatWidth::F32,
            hosted: FloatWidth::F64,
        }
    );
    assert!(matches!(
        error.into_bridge(),
        tutti_plugin_types::IpcError::WidthMismatch {
            activated: FloatWidth::F32,
            hosted: FloatWidth::F64,
        }
    ));
}
