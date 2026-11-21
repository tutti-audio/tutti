//! `ShimmerNode` as the graph runs it: its ports (the freeze's replaced by the shift and the
//! send), no reported latency though the shifter delays the loop, and a reported tail that is
//! the network's own with that delay in every line's loop.

use tutti_buffers::Planes;
use tutti_dsp::{FdnBank, FdnControls, FdnLayout, MaxDecay, Network, NetworkStage, System};
use tutti_graph::contract::{assert_declared_tail, drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, TransportChanges, TransportPosition};
use tutti_nodes::{ShimmerConfig, ShimmerNode};
use tutti_units::{
    Cents, Db, Feedback, Frame, Latency, SampleRate, SampleSpan, Samples, Seconds, Tail, UnitParam,
};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn node(config: ShimmerConfig) -> ShimmerNode<f32> {
    prepared(ShimmerNode::<f32>::from(config), SR, BLOCK)
}

/// The node offers the FDN's ten ports, a shift (`Pitch`, a `Cents`) and a send (`Feedback`),
/// and no freeze or wet port; it starts at the config's shift and send, and reports no latency:
/// the shifter's delay is the reverb's onset.
#[test]
fn the_ports_are_the_fdns_without_the_freeze_and_with_the_shift_and_the_send() {
    let n = node(
        ShimmerConfig::new()
            .pitch(Cents::new(700.0))
            .send(Feedback::new_clamped(0.3)),
    );
    assert_eq!(n.shape().params.len(), 12);
    let set = n.param_set();
    assert_eq!(set.get(UnitParam::Freeze), None);
    assert_eq!(set.get(UnitParam::Wet), None);
    assert_eq!(set.get(UnitParam::Pitch), Some(700.0));
    assert_eq!(set.get(UnitParam::Feedback), Some(0.3));
    assert_eq!(set.get(UnitParam::RoomSize), Some(0.5));
    assert!(matches!(n.shape().tail, Tail::Finite(_) | Tail::Unbounded));
    assert_eq!(
        n.shape().latency,
        Latency::new(Samples::new(0)),
        "the shifter sits in the loop: onset, not latency"
    );
}

/// The reported tail is the network's own `tail(-60 dB)` with the shifter's delay (the bank's
/// `shimmer_delay`) in the loop: the model with `loop_delay` set, which
/// the node reports at the controls in force, and past it the output is exactly silent (the
/// harness's check, which also looks past the ceiling). A send of 0 reports the plain network's.
#[test]
fn the_reported_tail_is_the_networks_with_the_shifters_delay_and_honest() {
    let layout = FdnLayout::shimmering(8, MaxDecay::DEFAULT).unwrap();
    let delay = FdnBank::<f32>::new(&layout, SR).shimmer_delay();
    let expected = |c: &FdnControls, loop_delay: Samples| {
        let mut network = Box::new(Network::new(SR));
        layout.design_into(c, SR, &mut network);
        for stage in network.stages.as_mut_slice() {
            if let NetworkStage::Fdn(f) = stage {
                f.loop_delay = SampleSpan::from_samples(loop_delay);
            }
        }
        let tutti_dsp::DecayTime::Finite(s) = network.tail(Db::new(-60.0)) else {
            panic!("finite")
        };
        (s.get() * SR.get()).ceil() as usize
    };
    for (decay, send) in [(0.3, 0.5), (1.0, 0.99), (0.3, 0.0)] {
        let c = FdnControls {
            decay: [Seconds::new(decay); 3],
            shimmer_pitch: Cents::new(1200.0),
            shimmer_send: Feedback::new_clamped(send),
            ..FdnControls::DEFAULT
        };
        let config = ShimmerConfig::new().controls(c);
        let mut n = node(config);
        let want = expected(&c, if send > 0.0 { delay } else { Samples::ZERO });
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SR,
            block_len: Samples::new(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let impulse = tutti_fixtures::signal::impulse(Samples::new(BLOCK));
        let reported = drive_input(
            &mut n,
            &env,
            Planes::new(&[&impulse[..]]).unwrap(),
            &[],
            &[],
        )
        .reported_tail;
        assert_eq!(
            reported,
            Some(Tail::Finite(Samples::new(want))),
            "decay {decay}, send {send}"
        );
        assert_declared_tail(&mut node(config), SR);
    }
}
