//! The graph's main output's declared topology: what the session's output is, as the
//! engine places it on the device and an export places it in a file.
//!
//! Every figure is analytic: a `Const` writes a known value per channel, and
//! where a fold is involved the expected frame is written out from the ITU
//! rules (`MINUS_3_DB`), not read from another render.

use tutti_buffers::Float;
use tutti_buffers::MaxBlock;
use tutti_buffers::{ChannelLayout, ChannelTopology, Placement, Speaker};
use tutti_buffers::{FoldError, InterleavedMut};
#[cfg(feature = "export")]
use tutti_codec::Encoding;
use tutti_graph::{ForkByClone, MainOutputError};
use tutti_nodes::testing::Const;
use tutti_session::{
    Committed, Error, MainOutput, MainOutputTopologyStatus, OutputPort, Session, SessionBuilder,
};
#[cfg(feature = "export")]
use tutti_session::{ToFile, ToMemory};
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

/// The channels `values` hold, one per main output from channel 0.
fn feed<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    session: &mut Session<K, C>,
    values: &[f32],
) {
    let node = session.graph().add(ForkByClone(Const::frame(values)));
    for c in 0..values.len() {
        session
            .graph()
            .set_sources(MainOutput::channel(c), [OutputPort::new(node, c)])
            .unwrap()
            .unfaded();
    }
}

/// Frame 0 of the second device block (the first installs the plan), after the
/// session commits. `device` channels wide.
fn heard(session: &mut Session, device: u16) -> Vec<f32> {
    let _ = session.update();
    let layout = ChannelLayout::from(device);
    let mut out = vec![-9.0f32; usize::from(device) * 64];
    for _ in 0..2 {
        session.render(&mut InterleavedMut::new(&mut out, layout).unwrap());
    }
    out[..usize::from(device)].to_vec()
}

fn offline(builder: SessionBuilder, device: u16) -> Result<Session, Error> {
    builder
        .sample_rate(RATE)
        .outputs(ChannelLayout::from(device))
        .max_block(MaxBlock::of(Samples::new(64)))
        .build()
}

fn buses(n: u16) -> ChannelTopology {
    ChannelTopology::unassigned(ChannelLayout::from(n)).unwrap()
}

/// Four declared buses on a six-wide device stay at 0..4. The main output is the
/// declaration's width, not the device's.
#[test]
fn a_declared_unassigned_main_output_keeps_its_buses_by_index() {
    let mut s = offline(Session::builder().main_output_topology(buses(4)), 6).unwrap();
    assert_eq!(s.graph_ref().outputs(), 4);
    assert_eq!(s.main_output_topology(), &buses(4));
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
}

/// A declared 5.1 main output folds into stereo by the 5.1 rules, from the speakers it
/// declared: the same six channels with the front pair declared the other way
/// round fold with left and right exchanged. (The plain 5.1 is the figure a bare
/// six gives, so only the swapped declaration shows the engine read it.)
#[test]
fn a_declared_surround_main_output_folds_by_the_surround_rules() {
    let five_one = tutti_buffers::Bed::Surround51.topology();
    let mut s = offline(Session::builder().main_output_topology(five_one), 2).unwrap();
    assert_eq!(s.graph_ref().outputs(), 6);
    feed(&mut s, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(
        heard(&mut s, 2),
        [
            1.0 + f32::MINUS_3_DB * 3.0 + f32::MINUS_3_DB * 5.0,
            2.0 + f32::MINUS_3_DB * 3.0 + f32::MINUS_3_DB * 6.0
        ]
    );
    let swapped = ChannelTopology::new([
        Speaker::FrontRight,
        Speaker::FrontLeft,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::SideLeft,
        Speaker::SideRight,
    ])
    .unwrap();
    s.declare_main_output_topology(swapped).unwrap().unfaded();
    assert_eq!(
        heard(&mut s, 2),
        [
            2.0 + f32::MINUS_3_DB * 3.0 + f32::MINUS_3_DB * 5.0,
            1.0 + f32::MINUS_3_DB * 3.0 + f32::MINUS_3_DB * 6.0
        ]
    );
}

/// With nothing declared, the main output is declared in the engine's order for the
/// wider of the floor and the device, and a bare four is quad.
#[test]
fn a_main_output_nothing_declared_is_the_engines_order_for_the_floor_and_device() {
    let mut s = offline(Session::builder().outputs_floor(ChannelLayout::QUAD), 6).unwrap();
    assert_eq!(
        s.main_output_topology(),
        &tutti_buffers::Bed::Surround51.topology()
    );
    assert_eq!(
        s.graph_ref().outputs(),
        6,
        "the device floors the main output"
    );
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);

    // A project of four on a stereo device is quad, folded at -3 dB.
    let mut quad = offline(Session::builder().outputs_floor(ChannelLayout::QUAD), 2).unwrap();
    assert_eq!(quad.graph_ref().outputs(), 4);
    feed(&mut quad, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(
        heard(&mut quad, 2),
        [0.1 + f32::MINUS_3_DB * 0.3, 0.2 + f32::MINUS_3_DB * 0.4]
    );
}

/// A declaration the device cannot place is refused, at build and after, and
/// leaves the session as it was.
#[test]
fn an_unplaceable_declaration_is_refused() {
    let refused = |r: Result<Session, Error>| match r {
        Err(Error::Output(e)) => e,
        other => panic!("expected Output, got {:?}", other.map(|_| ())),
    };
    assert_eq!(
        refused(offline(
            Session::builder().main_output_topology(buses(4)),
            2
        )),
        FoldError::Unplaced {
            channel: 2,
            speaker: Speaker::Unassigned
        }
    );
    let unnamed = ChannelTopology::new([Speaker::FrontLeft, Speaker::Unknown(9)]).unwrap();
    assert!(matches!(
        refused(offline(Session::builder().main_output_topology(unnamed), 2)),
        FoldError::Unnamed { .. }
    ));
    // A topology is at most `MAX_CHANNELS` wide, so a main output wider than the
    // engine renders cannot be declared; a device that wide still builds.
    assert!(
        ChannelTopology::new(vec![Speaker::Unassigned; tutti_buffers::MAX_CHANNELS + 1]).is_err()
    );
    let wide = offline(Session::builder(), 32).unwrap();
    assert_eq!(wide.output_device().unwrap().channels.topology(), None);

    // After the build.
    let mut s = offline(Session::builder(), 2).unwrap();
    assert!(matches!(
        s.declare_main_output_topology(buses(4)),
        Err(Error::Output(FoldError::Unplaced { channel: 2, .. }))
    ));
    assert_eq!(s.graph_ref().outputs(), 2);
    assert_eq!(
        s.main_output_topology(),
        &tutti_buffers::Bed::Stereo.topology()
    );
    // The plan sent last is still the one heard.
    feed(&mut s, &[0.25, 0.75]);
    assert_eq!(heard(&mut s, 2), [0.25, 0.75]);
}

/// Declared after the build, on a device-wide main output: the declaration sets the
/// main output's width (narrowing six to four), and declaring the engine's order for
/// four puts the rears on the device's side pair.
#[test]
fn a_declaration_sets_the_main_outputs_width() {
    let mut s = offline(Session::builder(), 6).unwrap();
    assert_eq!(s.graph_ref().outputs(), 6);
    s.declare_main_output_topology(buses(4)).unwrap();
    assert_eq!(s.graph_ref().outputs(), 4);
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);

    s.declare_main_output_topology(tutti_buffers::Bed::Quad.topology())
        .unwrap()
        .unfaded();
    assert_eq!(s.graph_ref().outputs(), 4);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]);
    assert_eq!(
        s.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
}

/// A refusal is a value: the declaration's, kept with the request. The
/// request is the same again once it is in force.
#[test]
fn a_refusal_is_a_status_not_a_log_line() {
    let mut s = offline(Session::builder(), 2).unwrap();
    assert_eq!(
        s.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
    let e = s.declare_main_output_topology(buses(4)).unwrap_err();
    let Error::Output(refusal) = e else { panic!() };
    assert_eq!(
        s.main_output_topology_status(),
        MainOutputTopologyStatus::Refused(refusal.into())
    );
    assert_eq!(s.requested_main_output_topology(), &buses(4), "kept");
    assert_eq!(
        s.main_output_topology(),
        &tutti_buffers::Bed::Stereo.topology(),
        "the previous stands"
    );
    // Asking for what is in force is Applied again.
    s.declare_main_output_topology(tutti_buffers::Bed::Stereo.topology())
        .unwrap();
    assert_eq!(
        s.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
}

/// A builder's declaration is in force after updates, and declaring the same
/// topology again changes nothing (no edit, so no commit).
///
/// The equal-request early return in `declare_main_output_topology` is
/// not observable here (the graph and engine ignore an equal declaration
/// themselves).
#[test]
fn declaring_the_same_topology_again_changes_nothing() {
    let mut s = offline(Session::builder().main_output_topology(buses(4)), 6).unwrap();
    let _ = s.update();
    let sent = s.graph_ref().testing().commits_sent();
    s.declare_main_output_topology(buses(4)).unwrap();
    let _ = s.update();
    let _ = s.update();
    assert_eq!(s.main_output_topology(), &buses(4));
    assert_eq!(s.graph_ref().testing().commits_sent(), sent);
    assert_eq!(
        s.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
}

/// An export of the main output places the file from the declaration, as the device
/// is: four buses into a six-channel file stay at 0..4.
#[cfg(feature = "export")]
#[test]
fn an_export_is_placed_from_the_declaration() {
    use crate::bounced::bounced;
    use tutti_session::{Bounce, BounceLatency, BounceTail, ExportClock, Range};
    let planes = |s: &mut tutti_session::DetachedSession| {
        let request = Bounce::new()
            .main_output(
                ToMemory::new().channels(tutti_buffers::ChannelSet::default_for(
                    ChannelLayout::from(6u16),
                )),
            )
            .range(Range::Seconds(
                tutti_units::Seconds::new(0.0),
                tutti_units::Seconds::new(0.01),
            ))
            .sample_rate(RATE)
            .clock(ExportClock::frozen())
            .latency(BounceLatency::Keep)
            .tail(BounceTail::None);
        let rendered = bounced(s, request).unwrap().into_main_output();
        rendered.planes().iter().map(|p| p[0]).collect::<Vec<_>>()
    };
    let mut declared = Session::builder()
        .main_output_topology(buses(4))
        .build_detached(RATE)
        .unwrap();
    feed(&mut declared, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(planes(&mut declared), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    declared
        .declare_main_output_topology(tutti_buffers::Bed::Quad.topology())
        .unwrap();
    assert_eq!(planes(&mut declared), [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]);
}

/// A bounced file's placement is stated only when the bounce names its speakers: a file of a
/// channel count is the default layout for that count and reads `Default`, one given
/// channels with speakers reads `Stated`.
#[cfg(feature = "export")]
#[test]
fn a_bounced_file_is_stated_only_when_its_speakers_are_named() {
    use crate::bounced::bounced;
    use tutti_buffers::FloatWidth;
    use tutti_codec::{Format, Pcm};
    use tutti_session::{Bounce, BounceTail, Range};
    let dir = tempfile::tempdir().expect("dir");
    let placement = |name: &str, topology: Option<ChannelTopology>| {
        let mut session = Session::builder().build_detached(RATE).unwrap();
        feed(&mut session, &[0.1, 0.2]);
        let format = Format::Wav(Pcm::Float(FloatWidth::F32));
        let encoding = match topology {
            Some(topology) => {
                Encoding::new(format).channels(tutti_buffers::ChannelSet::from(topology))
            }
            None => Encoding::new(format).width(ChannelLayout::STEREO),
        };
        let bounce = Bounce::new()
            .range(Range::Seconds(
                tutti_units::Seconds::new(0.0),
                tutti_units::Seconds::new(0.01),
            ))
            .main_output(ToFile::new(
                encoding,
                tutti_codec::ToFile::new(dir.path().join(name)),
            ))
            .tail(BounceTail::None);
        bounced(&mut session, bounce)
            .expect("renders")
            .into_main_output()
            .placement
    };
    assert_eq!(placement("count.wav", None), Placement::Default);
    assert_eq!(
        placement("named.wav", Some(tutti_buffers::Bed::Stereo.topology())),
        Placement::Stated
    );
}

/// The device's placement is stated or the default layout, and the session says which: a
/// device with nothing stated is the engine's order for its width and reads
/// `Default`; one the build was told about reads `Stated`, with the topology it
/// was told.
#[test]
fn the_output_device_reads_stated_or_default() {
    let default_device = offline(Session::builder(), 6).unwrap();
    let placed = default_device
        .output_device()
        .expect("an engine renders it");
    assert_eq!(placed.placement, Placement::Default);
    assert_eq!(
        placed.channels,
        tutti_buffers::ChannelSet::from(tutti_buffers::Bed::Surround51.topology())
    );

    let wired = ChannelTopology::new([
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::BackLeft,
        Speaker::BackRight,
    ])
    .unwrap();
    let stated = offline(Session::builder().device_topology(wired), 6).unwrap();
    let placed = stated.output_device().unwrap();
    assert_eq!(placed.placement, Placement::Stated);
    assert_eq!(placed.channels, tutti_buffers::ChannelSet::from(wired));

    let detached = Session::builder().build_detached(RATE).unwrap();
    assert_eq!(detached.output_device(), None, "no engine, no device");
}

/// A stated device is folded into by speaker: a stereo project into a device
/// stated right then left lands its left on the device's second channel, where
/// the default stereo device is the identity.
#[test]
fn a_stated_device_receives_the_main_output_by_speaker() {
    let swapped = ChannelTopology::new([Speaker::FrontRight, Speaker::FrontLeft]).unwrap();
    let mut s = offline(Session::builder().device_topology(swapped), 2).unwrap();
    feed(&mut s, &[0.25, 0.75]);
    assert_eq!(heard(&mut s, 2), [0.75, 0.25]);
    let mut plain = offline(Session::builder(), 2).unwrap();
    feed(&mut plain, &[0.25, 0.75]);
    assert_eq!(heard(&mut plain, 2), [0.25, 0.75]);
}

/// A statement that is not the device's width is refused where the device is
/// built, naming the widths, and builds nothing.
#[test]
fn a_statement_of_another_width_than_the_device_is_refused() {
    let five_one = tutti_buffers::Bed::Surround51.topology();
    match offline(Session::builder().device_topology(five_one), 2) {
        Err(Error::Output(FoldError::Buffer(tutti_buffers::BufferError::WidthMismatch {
            src,
            dst,
        }))) => {
            assert_eq!((src.count(), dst.count()), (6, 2));
        }
        other => panic!("expected a width mismatch, got {:?}", other.map(|_| ())),
    }
}

/// A main output changed after the build folds with the plan that carries it: the
/// session publishes the main output's folds, then sends the commit, and the block
/// that renders the new plan has the new main output's fold, not the old one's.
#[test]
fn a_main_output_declared_after_the_build_folds_from_the_plan_that_carries_it() {
    let mut s = offline(Session::builder(), 6).unwrap();
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    s.declare_main_output_topology(buses(4)).unwrap().unfaded();
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    // Back to quad: rears on 4, 5.
    s.declare_main_output_topology(tutti_buffers::Bed::Quad.topology())
        .unwrap()
        .unfaded();
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]);
}

/// A session with no engine places nothing: with no device to fold into, a
/// commit of any width is sent, since no block is ever folded.
#[test]
fn a_session_with_no_engine_commits_every_width() {
    let mut s = Session::builder().build_detached(RATE).unwrap();
    s.declare_main_output_topology(buses(5)).unwrap();
    assert_eq!(s.commit(), Committed::Sent);
    assert_eq!(s.graph_ref().outputs(), 5);
}

/// A main output declared under a stream that is not rendering is bounded: the
/// engine holds a few main outputs ahead of the plan in force, and past them a
/// declaration is refused by name (`Error::MainOutput`) with the main output unchanged;
/// one block frees the room.
#[test]
fn a_declaration_past_the_pending_main_outputs_is_refused_by_name() {
    let mut s = offline(Session::builder(), 6).unwrap();
    let mut refused = None;
    for i in 0..(tutti_engine::MAX_PENDING_MAIN_OUTPUTS + 2) {
        // Alternate, so each declaration is a new main output.
        let main_output = if i % 2 == 0 {
            buses(4)
        } else {
            tutti_buffers::Bed::Quad.topology()
        };
        match s.declare_main_output_topology(main_output) {
            Ok(_) => {}
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(
            refused,
            Some(Error::MainOutput(MainOutputError::TooManyPending { limit }))
                if limit == tutti_engine::MAX_PENDING_MAIN_OUTPUTS
        ),
        "{refused:?}"
    );
    // A block renders the newest committed plan and the room is back.
    let _ = heard(&mut s, 6);
    let _ = s.update();
    s.declare_main_output_topology(buses(4)).unwrap();
}
