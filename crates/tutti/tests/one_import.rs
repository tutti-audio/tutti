//! The headline claim, compiled and run: **one dependency, one `use` line,
//! a session that renders.**
//!
//! Device-free, so it runs on every platform in CI: a session, one node, one
//! block, and an assertion that the block is not silence. The vocabulary tests
//! below prove each group of names arrives at the root.

use tutti::graph::Node;
use tutti::{
    At, BarNumber, Beat, Bpm, Cents, ChannelLayout, Db, Frame, Meter, MeterMap, MotionCommand,
    SampleRate, Seconds, TransportCommand,
};
use tutti_nodes::testing::Const;
use tutti_timeline::Transport;

/// Everything below is reached through `tutti::` and nothing else. If the
/// umbrella stopped naming something, this stops compiling.
#[cfg(feature = "session")]
#[test]
fn one_import_renders_a_block() {
    use tutti::graph::ForkByClone;
    use tutti::{Hz, InterleavedMut, Root, Session};
    use tutti_nodes::testing::Osc;

    let mut session = Session::new(SampleRate::SR_48K).expect("builds");
    let (tone, _) = session.graph().add(ForkByClone(Osc::sine(Hz::new(440.0))));
    session
        .graph()
        .connect(tone, Root)
        .expect("both ends exist")
        .unfaded();
    assert!(session.update().is_empty(), "the commit raised no event");

    let mut out = vec![0.0f32; 256 * 2];
    session.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());

    assert!(
        out.iter().any(|&s| s != 0.0),
        "the graph must actually render — an assertion over silence would \
         pass against an umbrella that named nothing useful"
    );
}

/// The measurement vocabulary is at the crate's root, converting as
/// documented. If the root ever narrowed, this names which type went missing
/// rather than failing somewhere downstream.
#[test]
fn the_measurement_vocabulary_arrives_and_converts() {
    assert!((Db::new(-6.0).to_amplitude().get() - 0.501).abs() < 0.01);
    assert_eq!(
        Seconds::new(1.0).to_samples(SampleRate::SR_48K).get(),
        48_000
    );
    assert_eq!(Cents::new(1200.0).to_semitones().get(), 12.0);
    assert_eq!(ChannelLayout::STEREO.count(), 2);
}

/// The nodes and the node contract are reachable without naming their crates.
#[test]
fn the_dsp_library_and_the_node_contract_are_reachable() {
    // A `tutti-nodes` unit, built and driven through `tutti::` alone.
    let _lfo = tutti::nodes::LfoNode::new(tutti::nodes::LfoShape::Sine);
    // The node contract, `tutti::graph::Node`, on a node whose crate is a
    // dev-dependency.
    let node = Const::frame(&[0.5, 0.5]);
    let shape = Node::shape(&node);
    assert_eq!(shape.audio_out(), ChannelLayout::STEREO);
    assert_eq!(shape.audio_in(), ChannelLayout::EMPTY);
}

/// Offline export, end to end, through a session.
#[cfg(all(feature = "session", feature = "export"))]
#[test]
fn a_session_bounces_to_buffers_through_the_umbrella() {
    use tutti::graph::ForkByClone;
    use tutti::{Bounce, Range, Root, Session};

    let mut session = Session::new(SampleRate::SR_48K).expect("builds");
    let (node, _) = session.graph().add(ForkByClone(Const::frame(&[0.5, 0.5])));
    session
        .graph()
        .connect(node, Root)
        .expect("both ends exist")
        .unfaded();
    let planes = session
        .bounce(
            Bounce::project()
                .master_buffers()
                .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.01))),
        )
        .expect("a DC graph renders")
        .into_master()
        .into_planes();
    assert_eq!(planes.len(), 2);
    assert!(planes[0].iter().all(|&s| (s - 0.5).abs() < 1e-6));
}

/// A timed transport program is spelled from the root alone: `At`,
/// `TransportCommand`, and the time-signature trait with its `BarNumber`.
#[test]
fn a_timed_transport_program_needs_only_the_root() {
    let transport = Transport::new(SampleRate::SR_48K);
    transport
        .motion()
        .schedule(At::Frame(Frame::new(128)), MotionCommand::Play)
        .expect("room");
    transport
        .motion()
        .schedule(
            At::Beat(Beat::new(16.0)),
            TransportCommand::Motion(MotionCommand::stop()),
        )
        .expect("room");
    assert_eq!(transport.motion().scheduled_outstanding(), 2);

    let map = MeterMap::default();
    let bar: BarNumber = map.bar_numbers()[0];
    assert_eq!(Meter::bar_start(&map, bar), Beat::new(0.0));
}

/// A tempo map is built from `tutti::tempo` alone and published to the
/// transport's settings (what the session's tempo target writes): 120 BPM, then a time ramp to 180 over four
/// beats, lasts 1.6 s, so beat 4 is frame 76 800 at 48 kHz.
#[test]
fn a_tempo_map_is_built_and_published_through_the_facade() {
    let map = tutti::tempo::TempoMap::builder(Bpm::of(120.0))
        .ramp_time_to(Beat::new(4.0), Bpm::of(180.0))
        .build()
        .expect("a map");
    let rate = SampleRate::SR_48K;
    assert_eq!(
        map.view(rate).frame_at_or_after(Beat::new(4.0)),
        Frame::new(76_800)
    );
    let transport = Transport::new(rate);
    transport.settings().set_tempo_map(map);
}

/// A peer state's media type is named from the facade: `PeerState::media_type`
/// returns `tutti::midi::property::MediaType`, whatever MIDI features are on.
#[cfg(all(feature = "session", feature = "plugin"))]
#[test]
fn a_peer_states_media_type_is_named_through_the_facade() {
    use tutti::midi::property::MediaType;
    use tutti::{PeerState, PluginRef, SavedPeer};

    let plugin = PluginRef::file("/p/synth.clap");
    let media_type = MediaType::new("application/x-clap-state").expect("a media type");
    let state = PeerState::new(SavedPeer::Plugin(plugin), media_type.clone(), vec![1, 2]);
    let read: &MediaType = state.media_type();
    assert_eq!(read, &media_type);
}
