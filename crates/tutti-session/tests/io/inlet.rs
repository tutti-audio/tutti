//! `Graph::add_inlet`: audio a thread of your own pushes into the graph, the
//! mirror of the tap.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Interleaved;
use tutti_buffers::InterleavedMut;
use tutti_graph::InletRing;
use tutti_session::{InletError, OutPort, Root, Session};
use tutti_units::{SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

fn offline() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .build()
        .expect("builds")
}

/// Frames pushed to an inlet come out of the graph's outputs in order, then
/// silence once the ring runs dry.
#[test]
fn pushed_frames_reach_the_outputs_in_order() {
    let mut s = offline();
    let (ring, mut producer) = InletRing::pair(ChannelLayout::STEREO, Samples::new(256), RATE);
    let key = s
        .graph()
        .add_inlet(ring)
        .expect("the producer runs at the graph's rate");
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(key, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(Root::output(1), [OutPort::new(key, 1)])
        .unwrap()
        .unfaded();
    let mut tap = s
        .graph()
        .taps()
        .open_output(Samples::new(1_024))
        .expect("the sources exist");
    let _ = s.update();

    producer.push_interleaved(
        Interleaved::new(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0], producer.layout()).unwrap(),
    );
    let mut block = vec![0.0f32; 8 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());

    let mut got = vec![f32::NAN; 8 * 2];
    assert_eq!(
        tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap())
            .unwrap(),
        Samples::new(8)
    );
    assert_eq!(
        got,
        [
            1.0, -1.0, 2.0, -2.0, 3.0, -3.0, //
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0
        ],
        "the three pushed frames, then silence for the frames the ring lacked"
    );
}

/// A producer at another rate is refused with a typed error carrying both
/// rates, and nothing is opened.
#[test]
fn a_producer_at_another_rate_is_refused() {
    let mut s = offline();
    let _ = s.update();
    let (ring, _producer) = InletRing::pair(
        ChannelLayout::STEREO,
        Samples::new(256),
        SampleRate::SR_44K1,
    );
    let err = s
        .graph()
        .add_inlet(ring)
        .expect_err("an inlet does not resample");
    assert_eq!(
        err,
        InletError::RateMismatch {
            producer: SampleRate::SR_44K1,
            graph: RATE
        }
    );
    assert!(
        !s.graph().is_edited(),
        "a refused inlet leaves the graph as it was"
    );
}

/// **A ring carries the rate its producer fills it at**, and `add_inlet`
/// checks it against the graph's, with no rate to give apart from the ring:
/// one made at another rate is refused with both rates and adds no node, and
/// one at the graph's is added.
#[test]
fn add_inlet_checks_the_rate_the_ring_carries() {
    let mut s = offline();
    let _ = s.update();
    let (ring, _writer) =
        InletRing::pair(ChannelLayout::STEREO, Samples::new(64), SampleRate::SR_44K1);
    assert_eq!(
        s.graph().add_inlet(ring).expect_err("another rate"),
        InletError::RateMismatch {
            producer: SampleRate::SR_44K1,
            graph: RATE
        }
    );
    assert!(!s.graph().is_edited(), "nothing was added");

    let (ring, _writer) = InletRing::pair(ChannelLayout::STEREO, Samples::new(64), RATE);
    s.graph().add_inlet(ring).expect("the graph's rate");
    assert!(s.graph().is_edited(), "the node was added");
}

/// **An inlet of an `f64` session carries `FINE` exactly**: the producer
/// pushes `f64` frames, the graph reads them as `f64`, and the render holds
/// the figure `f32` rounds away.
#[test]
fn an_f64_inlet_carries_a_figure_f32_rounds_away() {
    const FINE: f64 = 1.0 + 1.0 / 1_099_511_627_776.0;
    assert_ne!(f64::from(FINE as f32), FINE, "f32 would round FINE away");
    let mut s = Session::builder()
        .float::<f64>()
        .sample_rate(RATE)
        .build()
        .expect("an f64 session builds");
    let (ring, mut producer) = InletRing::pair(ChannelLayout::MONO, Samples::new(256), RATE);
    let key = s
        .graph()
        .add_inlet(ring)
        .expect("the producer runs at the graph's rate");
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(key, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();

    producer.push_interleaved(Interleaved::new(&[FINE; 8], producer.layout()).unwrap());
    let mut block = vec![0.0f64; 8 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    assert!(
        block.chunks(2).all(|frame| frame[0] == FINE),
        "{:?}",
        &block[..4]
    );
}
