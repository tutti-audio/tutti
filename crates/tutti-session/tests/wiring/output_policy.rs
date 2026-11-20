//! The output policy: where the mix lands, set through
//! `session.policy::<OutputPolicy>()` and applied by `update`, live.
//!
//! Every figure is analytic: a `Const` writes a known value per channel and the
//! expected frame is written out from the ITU rules and the ramp's formula.

use tutti_buffers::Float;
use tutti_buffers::InterleavedMut;
use tutti_buffers::{Bed, ChannelLayout, MaxBlock};
use tutti_engine::{OutputMap, OutputStrip, RoutingError};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{
    OutPort, OutputPolicy, OutputPolicyError, Root, Session, SessionBuilder, SetRefusal,
    ValueError, ValueErrorKind,
};
use tutti_units::{Amplitude, SampleRate, Samples};

/// The output policy error a write was refused with.
fn refused(result: Result<(), ValueError<&'static str>>) -> OutputPolicyError {
    match result.expect_err("the write is refused").kind {
        ValueErrorKind::Refused(SetRefusal::OutputPolicy(error)) => error,
        other => panic!("expected an output policy refusal, got {other:?}"),
    }
}

const RATE: SampleRate = SampleRate::SR_48K;

fn feed(session: &mut Session, values: &[f32]) {
    let (node, _) = session.graph().add(ForkByClone(Const::frame(values)));
    for c in 0..values.len() {
        session
            .graph()
            .set_sources_unfaded(Root::output(c), [OutPort::new(node, c)])
            .unwrap();
    }
}

fn offline(builder: SessionBuilder, device: u16) -> (Session, tutti_session::Background) {
    builder
        .sample_rate(RATE)
        .outputs(ChannelLayout::from(device))
        .max_block(MaxBlock::of(Samples::new(64)))
        .build()
        .unwrap()
}

/// Renders `frames` frames after an update, returning the last frame heard.
fn last_frame(session: &mut Session, device: u16, frames: usize) -> Vec<f32> {
    let _ = session.update();
    let width = usize::from(device);
    let mut out = vec![-9.0f32; width * frames];
    session.render(&mut InterleavedMut::new(&mut out, ChannelLayout::from(device)).unwrap());
    out[width * (frames - 1)..].to_vec()
}

/// A monitor format and a map are heard on the device and change nothing the
/// root is: a tap on the root reads the six channels as fed.
#[test]
fn monitoring_changes_what_is_heard_and_never_the_root() {
    let (mut s, _bg) = offline(Session::builder(), 6);
    feed(&mut s, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let mut tap = s
        .graph()
        .taps()
        .open_output(Samples::new(4_096))
        .expect("the sources exist");
    assert_eq!(last_frame(&mut s, 6, 128), [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    let policy = OutputPolicy {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([2, 3]).unwrap(),
        ..OutputPolicy::default()
    };
    s.set(s.policy::<OutputPolicy>().to(policy)).unwrap();
    let m = f32::MINUS_3_DB;
    let (l, r) = (1.0 + m * 3.0 + m * 5.0, 2.0 + m * 3.0 + m * 6.0);
    // Heard once the routing's crossfade (the declick length) has run out: the
    // stereo fold on outputs 3 and 4, silence elsewhere.
    assert_eq!(last_frame(&mut s, 6, 512), [0.0, 0.0, l, r, 0.0, 0.0]);

    // The root is as fed, every frame the tap holds.
    let mut frame = [0.0f32; 6];
    let mut seen = 0;
    while tap
        .pop_into(InterleavedMut::new(&mut frame, tap.layout()).unwrap())
        .unwrap()
        .get()
        == 1
    {
        assert_eq!(frame, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0], "frame {seen}");
        seen += 1;
    }
    assert!(seen > 0, "the tap read the root");
}

/// The policy is checked against the device when it is set: a map that names an
/// output the device lacks, a width that is not what is heard, a strip past the
/// device. Nothing is kept.
#[test]
fn a_policy_that_does_not_fit_the_device_is_refused_when_set() {
    let (s, _bg) = offline(Session::builder(), 2);
    let map = OutputPolicy {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([2, 3]).unwrap(),
        ..OutputPolicy::default()
    };
    assert_eq!(
        refused(s.set(s.policy::<OutputPolicy>().to(map))),
        OutputPolicyError::Routing(RoutingError::MapPast {
            output: 2,
            device: ChannelLayout::STEREO
        })
    );
    let wide = OutputPolicy {
        map: OutputMap::channels([0, 1, 2]).unwrap(),
        ..OutputPolicy::default()
    };
    assert_eq!(
        refused(s.set(s.policy::<OutputPolicy>().to(wide))),
        OutputPolicyError::Routing(RoutingError::MapWidth {
            map: 3,
            heard: ChannelLayout::STEREO
        })
    );
    let mut strip = OutputPolicy::default();
    strip.outputs.set_strip(2, OutputStrip::default()).unwrap();
    assert_eq!(
        refused(s.set(s.policy::<OutputPolicy>().to(strip))),
        OutputPolicyError::StripPast {
            output: 2,
            device: ChannelLayout::STEREO
        }
    );
    assert_eq!(
        s.get(s.policy::<OutputPolicy>()).unwrap(),
        OutputPolicy::default()
    );
}

/// A session with no engine has no device to route to.
#[test]
fn a_detached_session_has_nothing_to_route() {
    let (s, _bg) = Session::builder().build_detached(RATE).unwrap();
    assert_eq!(
        refused(s.set(s.policy::<OutputPolicy>().to(OutputPolicy::default()))),
        OutputPolicyError::Detached
    );
}

/// A mute ramps to silence over the declick length and the dim scales the
/// rest, live, applied by `update`: the frames in between are the ramp.
#[test]
fn a_mute_and_a_dim_ramp_and_settle() {
    let (mut s, _bg) = offline(Session::builder(), 2);
    feed(&mut s, &[1.0, 1.0]);
    assert_eq!(last_frame(&mut s, 2, 64), [1.0, 1.0]);

    let mut policy = OutputPolicy::default();
    policy
        .outputs
        .set_strip(
            0,
            OutputStrip {
                mute: true,
                ..OutputStrip::default()
            },
        )
        .unwrap();
    policy.outputs = std::mem::take(&mut policy.outputs)
        .with_dim(Some(Amplitude::new(0.5)))
        .unwrap();
    s.set(s.policy::<OutputPolicy>().to(policy)).unwrap();
    let _ = s.update();
    let n = tutti_graph::declick_frames(RATE).get();
    let mut out = vec![0.0f32; 2 * (n + 8)];
    s.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
    // Left ramps from unity to silence; right from unity to the dim.
    for k in 0..n {
        // The ramp renders in `f32` as `from + (to - from) * (k + 1) / n`: one
        // rounding each for the ratio, the product and the sum, plus the ends'
        // narrowing, at most 4 ulp of the ramp's scale `max(|from|, |to|)` = 1
        // (the left ramp cancels toward 0, so its scale is the end it left, not
        // its value). The last frame is the target exactly.
        let t = (k + 1) as f64 / n as f64;
        let (left, right) = (1.0 + (0.0 - 1.0) * t, 1.0 + (0.5 - 1.0) * t);
        if k + 1 >= n {
            assert_eq!(out[2 * k], 0.0, "left, frame {k}");
            assert_eq!(out[2 * k + 1], 0.5, "right, frame {k}");
        } else {
            let tolerance = 4.0 * f64::from(f32::EPSILON) * 1.0;
            assert!(
                (f64::from(out[2 * k]) - left).abs() <= tolerance,
                "left, frame {k}"
            );
            assert!(
                (f64::from(out[2 * k + 1]) - right).abs() <= tolerance,
                "right, frame {k}"
            );
        }
    }
    assert_eq!(&out[2 * n..2 * n + 2], [0.0, 0.5]);
}

/// A level that is not a gain is refused when the policy is set, even with no
/// monitor to use it, and the policy in force is kept.
#[test]
fn a_level_that_is_not_a_gain_is_refused_when_set() {
    use tutti_buffers::{FoldError, FoldLevel, FoldLevels};
    let (s, _bg) = offline(Session::builder(), 2);
    let policy = OutputPolicy {
        levels: FoldLevels {
            surround: Amplitude::new(f64::NAN),
            ..FoldLevels::default()
        },
        ..OutputPolicy::default()
    };
    assert_eq!(
        refused(s.set(s.policy::<OutputPolicy>().to(policy))),
        OutputPolicyError::Routing(RoutingError::Fold(FoldError::NotAGain {
            level: FoldLevel::Surround
        }))
    );
    assert_eq!(
        s.get(s.policy::<OutputPolicy>()).unwrap(),
        OutputPolicy::default()
    );
}
