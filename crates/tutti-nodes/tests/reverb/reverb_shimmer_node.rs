//! `ShimmerNode` as the graph runs it: its ports (the freeze's replaced by the shift and the
//! send), no reported latency though the shifter delays the loop, and a reported tail that is
//! the network's own with that delay in every line's loop.

use tutti_buffers::Planes;
use tutti_dsp::{
    FdnBank, FdnControls, FdnLayout, MaxDecay, Network, NetworkStage, ShimmerControls,
    ShimmerLayout, System,
};
use tutti_graph::contract::{assert_declared_tail, drive_input, prepared};
use tutti_graph::{
    Env, MAX_NODE_TAIL, Node, ParamNode, PlayClock, TransportChanges, TransportPosition,
};
use tutti_nodes::{FdnNodeControls, ShimmerConfig, ShimmerNode};
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

/// The tail reads the send and no shift or wet gain: moving either alone leaves the reported
/// tail, which is what their `no_ring` flags promise.
#[test]
fn the_shift_and_the_wet_gain_do_not_change_the_reported_tail() {
    let figure = |pitch: f64, wet: f64| {
        let c = FdnNodeControls::<ShimmerLayout> {
            network: FdnControls {
                wet: Db::new(wet),
                ..FdnControls::DEFAULT
            },
            kind: ShimmerControls {
                pitch: Cents::new(pitch),
                send: Feedback::new_clamped(0.5),
            },
        };
        let mut n = node(ShimmerConfig::new().controls(c));
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SR,
            block_len: Samples::new(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let impulse = tutti_fixtures::signal::impulse::<f32>(Samples::new(BLOCK));
        drive_input(
            &mut n,
            &env,
            Planes::new(&[&impulse[..]]).unwrap(),
            &[],
            &[],
        )
        .reported_tail
    };
    let base = figure(1200.0, 0.0);
    assert!(matches!(base, Some(Tail::Finite(_))), "{base:?}");
    for (pitch, wet) in [(-1200.0, 0.0), (700.0, 0.0), (1200.0, -12.0), (0.0, 6.0)] {
        assert_eq!(figure(pitch, wet), base, "{pitch} cents, {wet} dB");
    }
}

/// The reported tail is the network's own `tail(-60 dB)` with the shifter's delay (the bank's
/// `shimmer_delay`) in the loop: the model with `loop_delay` set, which
/// the node reports at the controls in force, and past it the output is exactly silent (the
/// harness's check, which also looks past the ceiling). A send of 0 reports the plain network's.
#[test]
fn the_reported_tail_is_the_networks_with_the_shifters_delay_and_honest() {
    let layout = ShimmerLayout::default();
    let delay = FdnBank::<f32, ShimmerLayout>::new(&layout, SR).shimmer_delay();
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
        let c = FdnNodeControls::<ShimmerLayout> {
            network: FdnControls {
                decay: [Seconds::new(decay); 3],
                ..FdnControls::DEFAULT
            },
            kind: ShimmerControls {
                pitch: Cents::new(1200.0),
                send: Feedback::new_clamped(send),
            },
        };
        let config = ShimmerConfig::new().controls(c);
        let mut n = node(config);
        let want = expected(&c.network, if send > 0.0 { delay } else { Samples::ZERO });
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SR,
            block_len: Samples::new(BLOCK),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let impulse = tutti_fixtures::signal::impulse::<f32>(Samples::new(BLOCK));
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

/// The declared tail is a figure the graph takes or it is unbounded: the default 8 s room at
/// 192 kHz and at 48 kHz declares a finite ceiling within `MAX_NODE_TAIL` (sixty seconds,
/// converted at the rate), a 12 s room at 96 kHz too, past the 2.88 million frames that bounded
/// a tail at 48 kHz; the ceiling is the decay stretched by the shifter's delay in every trip, so
/// a 19 s room at 48 kHz and the longest room the type allows (60 s at 192 kHz) declare
/// `Tail::Unbounded` and not a finite figure the graph would refuse or count as never decaying.
#[test]
fn the_declared_tail_is_within_the_graphs_bound_or_unbounded() {
    let at = |max: MaxDecay, rate: SampleRate| {
        prepared(
            ShimmerNode::<f32>::from(
                ShimmerConfig::new().layout(ShimmerLayout::new(FdnLayout::new(8, max).unwrap())),
            ),
            rate,
            BLOCK,
        )
        .shape()
        .tail
    };
    for (max, rate) in [
        (MaxDecay::DEFAULT, SampleRate::SR_192K),
        (MaxDecay::DEFAULT, SampleRate::SR_48K),
    ] {
        let Tail::Finite(t) = at(max, rate) else {
            panic!("a finite ceiling")
        };
        assert!(
            t <= MAX_NODE_TAIL.to_samples_ceil(rate) && t.get() as f64 > 8.0 * rate.get(),
            "{t:?} at {rate:?}"
        );
    }
    let Tail::Finite(room) = at(MaxDecay::of(Seconds::new(12.0)), SampleRate::SR_96K) else {
        panic!("a finite ceiling")
    };
    assert!(
        room.get() > 2_880_000 && room <= MAX_NODE_TAIL.to_samples_ceil(SampleRate::SR_96K),
        "{room:?}"
    );
    for (max, rate) in [
        (MaxDecay::of(Seconds::new(19.0)), SampleRate::SR_48K),
        (MaxDecay::new(MaxDecay::CAP).unwrap(), SampleRate::SR_192K),
    ] {
        assert_eq!(at(max, rate), Tail::Unbounded, "{max:?} at {rate:?}");
    }
}
