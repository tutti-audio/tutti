use super::*;
use crate::test_support::{RATE, prepared, render_chunked};
use tutti_dsp::{Inline, Tap};
use tutti_graph::Node;
use tutti_graph::contract::assert_declared_tail;
use tutti_units::{Gain, SampleSpan, Samples};

fn tap(delay: f64, gain: f64, channel: u8) -> Tap {
    Tap {
        delay: SampleSpan::new(delay).unwrap(),
        gain: Gain::new(gain),
        channel,
    }
}

fn config(width: u16, taps: &[Tap]) -> EarlyReflectionsConfig {
    EarlyReflectionsConfig::new(
        width,
        TapSet {
            taps: Inline::from_slice(taps).unwrap(),
        },
    )
}

fn node(width: u16, taps: &[Tap]) -> EarlyReflectionsNode<f32> {
    prepared(EarlyReflectionsNode::try_from(config(width, taps)).unwrap())
}

/// The shape is the input's width in and a channel for each one a tap feeds out, with no
/// latency; an empty layout is mono; the tail is the last tap, and the output holds to it.
#[test]
fn the_shape_is_the_input_width_to_the_taps_channels_with_no_latency() {
    let taps = [tap(3.0, 0.5, 0), tap(40.0, -0.25, 1)];
    let mut n = node(3, &taps);
    let shape = n.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (3, 2)
    );
    assert_eq!(shape.latency, tutti_units::Latency::new(Samples::ZERO));
    assert_eq!(shape.tail, Tail::Finite(Samples::new(40)));
    assert_declared_tail(&mut n, RATE);
    assert_eq!(node(0, &taps).shape().audio_in().count(), 1);
}

/// An impulse through the node is the taps' gains at their delays on their channels, whatever
/// the block size.
#[test]
fn an_impulse_through_the_node_is_the_taps() {
    let taps = [tap(3.0, 0.5, 0), tap(7.0, -0.25, 1), tap(7.0, 0.125, 0)];
    let mut x = vec![0.0f32; 24];
    x[0] = 1.0;
    let mut want = vec![vec![0.0f32; 24]; 2];
    want[0][3] = 0.5;
    want[1][7] = -0.25;
    want[0][7] = 0.125;
    for chunk in [1, 5, 24] {
        let mut n = node(1, &taps);
        assert_eq!(
            render_chunked(&mut n, &[x.clone()], chunk),
            want,
            "chunk {chunk}"
        );
    }
}

/// A tap that cannot be realized is refused at construction.
#[test]
fn a_tap_that_cannot_run_is_refused_at_construction() {
    let bad = EarlyReflectionsNode::<f32>::try_from(config(1, &[tap(1.0, f64::NAN, 0)]));
    assert_eq!(bad.err(), Some(EarlyReflectionsError::Gain { index: 0 }));
}

/// A room's taps make a one-channel stage that rings for the last image: the tail is that
/// image's delay rounded up.
#[test]
fn a_rooms_taps_are_a_stage_that_rings_for_its_last_image() {
    use tutti_dsp::ReflectionOrder;
    use tutti_units::{Metres, Normalized};
    let m = Metres::of;
    let room = tutti_dsp::ShoeboxRoom::try_from(
        tutti_dsp::ShoeboxConfig::new(
            [m(10.0), m(8.0), m(6.0)],
            [m(3.0), m(4.0), m(2.0)],
            [m(6.0), m(2.0), m(1.5)],
            Normalized::of(0.3),
        )
        .order(ReflectionOrder::Second),
    )
    .unwrap();
    let taps = room.taps(RATE);
    let last = taps.taps.iter().map(|t| t.delay).next_back().unwrap();
    let n = prepared(
        EarlyReflectionsNode::<f32>::try_from(EarlyReflectionsConfig::new(1u16, taps)).unwrap(),
    );
    assert_eq!(n.shape().audio_out().count(), 1);
    assert_eq!(n.shape().tail, Tail::Finite(last.to_samples_ceil()));
}
