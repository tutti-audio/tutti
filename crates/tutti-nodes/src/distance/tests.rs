use super::*;
use tutti_graph::Node;
use tutti_graph::contract::{BlockRig, assert_param_fork, prepared};
use tutti_units::{Latency, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;
const C: f64 = Speed::SOUND_IN_AIR_20C.get();
const BLOCK: usize = 480;

/// The distance whose delay is exactly `frames` frames at the rate.
fn distance_of(frames: usize) -> Metres {
    Metres::new(frames as f64 / RATE.get() * C).unwrap()
}

/// `blocks` blocks of `BLOCK` frames of mono `f64` through the node: an impulse at frame 0
/// of block 0 (which wakes the node) and of block `impulse_at`, the distance set to `set(i)` before block `i`.
fn run(
    config: DistanceConfig,
    blocks: usize,
    impulse_at: usize,
    set: impl Fn(usize) -> Option<Metres>,
) -> Vec<f64> {
    let (mut rig, controls) =
        BlockRig::<f64>::with_format(DistanceNode::<f64>::from(config), RATE, BLOCK);
    let mut out = Vec::new();
    for i in 0..blocks {
        if let Some(distance) = set(i) {
            controls.set(UnitParam::Distance, distance.get());
        }
        {
            let mut input = rig.inputs_mut();
            let mut plane = input.plane_mut(0);
            let slot = plane.samples_mut();
            slot.fill(0.0);
            if i == 0 || i == impulse_at {
                slot[0] = 1.0;
            }
        }
        rig.block();
        let heard = rig.output();
        out.extend_from_slice(heard.plane(0).samples());
    }
    out
}

/// A fork starts from the distance last set and shares no cell.
#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(DistanceNode::from(
        DistanceConfig::new(2u16, Metres::of(50.0)).distance(Metres::of(8.0)),
    ));
}

/// The shape is the width in and out, declares the distance as `Distance` starting as
/// configured, reports no latency (the delay is musical) and a finite tail past the longest
/// delay; an empty layout is no channels.
#[test]
fn the_shape_declares_the_distance_no_latency_and_a_tail_past_the_longest_delay() {
    let max = Metres::of(100.0);
    let n = prepared(
        DistanceNode::<f32>::from(DistanceConfig::new(3u16, max).distance(Metres::of(8.0))),
        RATE,
        64,
    );
    let shape = n.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (3, 3)
    );
    assert_eq!(shape.latency, Latency::new(Samples::ZERO));
    let Tail::Finite(tail) = shape.tail else {
        panic!("a finite tail, got {:?}", shape.tail);
    };
    assert!(tail.get() as f64 >= (max / Speed::SOUND_IN_AIR_20C).get() * RATE.get());
    assert_eq!(shape.params.as_slice(), &[UnitParam::Distance][..]);
    assert_eq!(n.param_base(0), Some(8.0));
    let empty = DistanceNode::<f32>::from(DistanceConfig::new(ChannelLayout::EMPTY, max));
    assert_eq!(empty.shape().audio_in().count(), 0);
}

/// **The node delays by exactly `r / c` and scales by `r_ref / r`**: an impulse at a distance
/// of a whole 480 frames comes out at frame 480 with the gain `r_ref / r`.
#[test]
fn the_node_is_the_delay_r_over_c_scaled_by_reference_over_r() {
    let r = distance_of(480);
    let y = run(
        DistanceConfig::new(1u16, Metres::of(100.0)).distance(r),
        4,
        0,
        |_| None,
    );
    let gain = 1.0 / r.get();
    for (n, &v) in y.iter().enumerate() {
        let want = if n == 480 { gain } else { 0.0 };
        assert!((v - want).abs() <= gain * 2f64.powi(-31), "frame {n}: {v}");
    }
}

/// **The live distance moves the delay**: from 1 m, a `Distance` of `r` (a whole 480 frames)
/// is glided to and an impulse after it lands at `r / c` with the level `1 / r`.
#[test]
fn a_live_distance_lands_on_its_delay_and_level() {
    let r = distance_of(480);
    // 340 frames of delay at 0.125 a frame: 2720 frames, about six blocks.
    let y = run(DistanceConfig::new(1u16, Metres::of(100.0)), 20, 12, |i| {
        (i == 0).then_some(r)
    });
    let at = 12 * BLOCK + 480;
    let gain = 1.0 / r.get();
    assert!((y[at] - gain).abs() <= gain * 2f64.powi(-31), "{}", y[at]);
    assert!(
        y[12 * BLOCK..at]
            .iter()
            .all(|&v| v.abs() <= gain * 2f64.powi(-31))
    );
}

/// **The first distance after a prepare lands at once**: with nothing in the line to bend, a
/// `Distance` set before the first block is the delay of that block.
#[test]
fn the_first_distance_lands_at_once() {
    let r = distance_of(480);
    let y = run(DistanceConfig::new(1u16, Metres::of(100.0)), 2, 0, |i| {
        (i == 0).then_some(r)
    });
    let gain = 1.0 / r.get();
    assert!((y[480] - gain).abs() <= gain * 2f64.powi(-31), "{}", y[480]);
}
