//! `Session::new`: the full build, with the engine, bounces what it holds.

use crate::bounced::bounced;
use tutti_buffers::InterleavedMut;
use tutti_buffers::{Bed, ChannelLayout, ChannelSet, Placement};
use tutti_fixtures::WavePlanes;
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::ToMemory;
use tutti_session::{Bounce, BounceLatency, BounceTail, MainOutput, Range, Session};
use tutti_units::{SampleRate, Seconds};

/// **A session from `Session::new` is a full session.** A constant wired to
/// the main output bounces to buffers holding that constant on every frame, and
/// `render`, which only a session with an engine has, answers instead of
/// panicking.
#[test]
fn a_session_from_new_bounces_a_node_and_renders() {
    let mut session = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds");
    let node = session
        .graph()
        .add(ForkByClone(Const::new(0.25, ChannelLayout::STEREO)));
    session.graph().connect(node, MainOutput).unwrap().unfaded();
    let planes = bounced(
        &mut session,
        Bounce::new()
            .main_output(ToMemory::new())
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.05)))
            .latency(BounceLatency::Keep)
            .tail(BounceTail::None),
    )
    .expect("the bounce renders")
    .into_main_output()
    .to_planes();
    assert_eq!(planes.len(), 2);
    assert!(planes.iter().all(|p| !p.is_empty()));
    assert!(planes.iter().flatten().all(|&x| x == 0.25));

    let mut block = [0.0f32; 128];
    session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
}

/// **A bounce into memory is a `Wave` placed as the destination says.** A plain `ToMemory`
/// holds the engine's order for the output's width, reported as a default placement; one
/// given the stereo bed's channels holds exactly that set, reported as stated.
#[test]
fn a_bounce_into_memory_carries_the_channels_it_was_asked_for() {
    let stereo = ChannelSet::from(Bed::Stereo.topology());
    let wave = |destination: ToMemory| {
        let mut session = Session::builder()
            .outputs_floor(ChannelLayout::STEREO)
            .build_detached(SampleRate::SR_48K)
            .expect("a detached session builds");
        let node = session
            .graph()
            .add(ForkByClone(Const::new(0.25, ChannelLayout::STEREO)));
        session.graph().connect(node, MainOutput).unwrap().unfaded();
        bounced(
            &mut session,
            Bounce::new()
                .main_output(destination)
                .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.05)))
                .latency(BounceLatency::Keep)
                .tail(BounceTail::None),
        )
        .expect("the bounce renders")
        .into_main_output()
    };

    let plain = wave(ToMemory::new());
    assert_eq!(plain.channel_set(), stereo);
    assert_eq!(plain.placement(), Placement::Default);

    let stated = wave(ToMemory::new().channels(stereo));
    assert_eq!(stated.channel_set(), stereo);
    assert_eq!(stated.placement(), Placement::Stated);
}
