//! `process_audio` drives the transport's clock: a block advances the beat, and
//! a loop wraps it.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_buffers::MaxBlock;
use tutti_engine::{AudioCallbackState, Engine, process_audio};
use tutti_graph::NodeKey;
use tutti_graph::{Edge, InPort, OutPort, PortSource};
use tutti_graph::{Editor, Prepare};
use tutti_nodes::testing::Osc;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_timeline::{MotionCommand, Timeline, Transport};
use tutti_units::{Beat, BeatDuration};
use tutti_units::{Hz, Q};
use tutti_units::{SampleRate, Samples};

/// Build an engine + transport pair whose graph actually renders.
///
/// An empty graph renders silence without running a node, which would
/// leave the transport assertions below reading a playhead nothing
/// exercised. A sine through a filter into the output gives the render
/// path real nodes to run, buffers to hand between them, and a fold to
/// the device width. The playhead is the engine's own clock; there is no
/// clock node in the graph.
fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
    let transport = Transport::new(SampleRate::of(sample_rate));

    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(sample_rate),
        MaxBlock::of(Samples::new(512)),
    ));
    let (source, filter) = (NodeKey(1), NodeKey(2));
    ed.insert(source, "sine", Osc::sine(Hz::new(220.0)));
    ed.insert(
        filter,
        "filter",
        SvfFilterNode::<f32>::from(
            SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
                .freq(Hz::new(2_000.0))
                .q(Q::new(0.7)),
        ),
    );
    ed.spec_mut().topology.edges.insert(
        InPort {
            node: filter,
            port: 0,
        },
        vec![Edge::Direct(PortSource::Node(OutPort {
            node: source,
            port: 0,
        }))],
    );
    // The filter's single output on both device channels.
    ed.spec_mut().topology.set_outputs(vec![
        PortSource::Node(OutPort {
            node: filter,
            port: 0,
        });
        2
    ]);
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec, tutti_engine::BlockDates::Os)
        .expect("within the limits");
    let state = AudioCallbackState::new(engine);
    (transport, state)
}

#[test]
fn test_transport_advances_with_graph() {
    let sample_rate = 44100.0;
    let (transport, state) = build_callback_state(sample_rate);

    transport.settings().set_tempo(tutti_units::Bpm::of(120.0));
    let _ = transport.motion().try_send(MotionCommand::Play);

    let frames = 256;
    let mut output = vec![0.0f32; frames * 2];
    process_audio(
        &state,
        &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap(),
    );

    let expected_beat = Beat::new(256.0 * (120.0 / 60.0) / 44100.0);
    let actual_beat = transport.beat();
    assert!(
        (actual_beat - expected_beat).abs() < BeatDuration::new(1e-6),
        "expected {expected_beat:?}, got {actual_beat:?}"
    );
}

#[test]
fn test_transport_loop_wrapping() {
    let sample_rate = 44100.0;
    let (transport, state) = build_callback_state(sample_rate);

    transport.settings().set_tempo(tutti_units::Bpm::of(120.0));
    transport
        .motion()
        .schedule(
            tutti_units::At::NextBlock,
            tutti_timeline::LoopEdit::over(
                tutti_music::LoopRange::new(Beat::new(0.0), Beat::new(4.0)).expect("ordered"),
            ),
        )
        .expect("room");
    let _ = transport.motion().try_send(MotionCommand::Play);
    let _ = transport
        .motion()
        .try_send(MotionCommand::locate(Beat::new(3.99)));

    let frames = 1024;
    let mut output = vec![0.0f32; frames * 2];
    process_audio(
        &state,
        &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO).unwrap(),
    );

    // Seeked to 3.99 at the first frame, rolling at 2 beats a second: one
    // wrap past 4.0, so the beat is 3.99 plus the block, less the loop's
    // length. No seek would leave it at the block's own length, and a wrap
    // counted twice or not at all would miss it.
    let beat = transport.beat();
    let expected = Beat::new(3.99 + 1024.0 * (120.0 / 60.0) / 44100.0 - 4.0);
    assert!(
        (beat - expected).abs() < BeatDuration::new(1e-6),
        "expected {expected:?} after one wrap, got {beat:?}"
    );
}
