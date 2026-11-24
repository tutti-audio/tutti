//! `PlateReverbNode` as the graph runs it: its ports, its tail (the declared ceiling and the
//! per-block narrowing to what rings), the park after the tail and the fresh wake, wet-only
//! output, a reset and a moved cell across block sizes. The tail figures are the network's own
//! queries and the one-pole's closed form, not another reverb's render.

use tutti_buffers::Planes;
use tutti_dsp::{DecayTime, Network, PlateControls, System};
use tutti_graph::contract::{Driven, assert_declared_tail, drive, drive_input, prepared};
use tutti_graph::{
    Env, MAX_NODE_TAIL, Node, ParamNode, PlayClock, Status, TransportChanges, TransportPosition,
};
use tutti_nodes::{PlateReverbConfig, PlateReverbNode};
use tutti_units::{
    Bipolar, Db, Frame, Latency, Mix, SampleRate, Samples, Seconds, Tail, UnitParam,
};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn node(c: PlateReverbConfig) -> PlateReverbNode<f32> {
    prepared(PlateReverbNode::<f32>::from(c), SR, BLOCK)
}

fn impulse() -> Vec<f32> {
    tutti_fixtures::signal::impulse::<f32>(Samples::new(BLOCK))
}

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    tutti_fixtures::noise::<f32>(seed, frames)
        .into_iter()
        .map(|x| 0.5 * x)
        .collect()
}

/// A node and the stream frame its next block starts at: the graph's view of it.
struct Rig {
    node: PlateReverbNode<f32>,
    frame: u64,
}

impl Rig {
    fn new(c: PlateReverbConfig) -> Self {
        Self {
            node: node(c),
            frame: 0,
        }
    }

    fn block(&mut self, input: &[f32]) -> Driven {
        let env = Env {
            frame: Frame::new(self.frame),
            sample_rate: SR,
            block_len: Samples::new(input.len()),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        self.frame += input.len() as u64;
        drive_input(
            &mut self.node,
            &env,
            Planes::new(&[input, input]).unwrap(),
            &[],
            &[],
        )
    }

    fn silent(&mut self) -> Driven {
        self.block(&[0.0; BLOCK])
    }
}

/// The node has the plate's six controls and no mix (the graph blends), declares no latency
/// (the pre-delay is part of the sound) and a finite ceiling over the longest decay that stays
/// inside what the graph takes at 192 kHz.
#[test]
fn the_ports_the_latency_and_the_declared_ceiling() {
    let n = node(PlateReverbConfig::new());
    assert_eq!(n.shape().params.len(), 6);
    let set = n.param_set();
    assert_eq!(set.get(UnitParam::Decay), Some(1.8));
    assert_eq!(set.get(UnitParam::Diffusion), Some(1.0));
    assert_eq!(
        set.get(UnitParam::Wet),
        None,
        "no mix port: the graph blends"
    );
    assert_eq!(n.shape().latency, Latency::new(Samples::new(0)));
    let Tail::Finite(ceiling) = n.shape().tail else {
        panic!("a finite ceiling")
    };
    assert!(
        ceiling.get() as f64 / SR.get() > 8.0,
        "the longest decay rings that long"
    );
    let Tail::Finite(at_192k) = prepared(
        PlateReverbNode::<f32>::from(PlateReverbConfig::new()),
        SampleRate::SR_192K,
        BLOCK,
    )
    .shape()
    .tail
    else {
        panic!("a finite ceiling at 192 kHz")
    };
    assert!(at_192k <= MAX_NODE_TAIL.to_samples_ceil(SampleRate::SR_192K));
}

/// The reported tail is the network's own `tail(-60 dB)` at the controls in force plus the
/// input one-pole's ring (`ln(0.001) / ln(pole)` frames, pole `exp(-2 pi fc / fs)`), within a
/// frame; it narrows with the decay; and past it the output is exactly silent (the harness's
/// check, which also looks past the ceiling).
#[test]
fn the_reported_tail_is_the_networks_and_honest() {
    let mut last = 0;
    for decay in [0.3, 1.0, 2.5] {
        let c = PlateReverbConfig::new().decay(Seconds::new(decay));
        let Some(Tail::Finite(reported)) = Rig::new(c).block(&impulse()).reported_tail else {
            panic!("a finite reported tail")
        };
        let mut network = Box::new(Network::new(SR));
        PlateControls {
            decay: Seconds::new(decay),
            ..PlateControls::DEFAULT
        }
        .design_into(SR, &mut network);
        let DecayTime::Finite(t) = network.tail(Db::new(-60.0)) else {
            panic!("finite")
        };
        let ring = f64::ln(1.0e-3) / (-std::f64::consts::TAU * 20_000.0 / SR.get());
        let want = t.get() * SR.get() + ring;
        assert!(
            (reported.get() as f64 - want).abs() <= 1.0,
            "decay {decay}: reported {reported:?}, network {want}"
        );
        assert!(reported.get() > last, "a longer decay is a longer tail");
        last = reported.get();
        assert_declared_tail(&mut node(c), SR);
    }
}

/// Past its tail the node parks, and a node that parked wakes as a fresh one: the same impulse
/// renders the same bits the second time as the first, whatever stream position it wakes at.
#[test]
fn a_parked_node_wakes_as_a_fresh_one() {
    let mut rig = Rig::new(PlateReverbConfig::new().decay(Seconds::new(0.3)));
    let play = |rig: &mut Rig| {
        let mut out = vec![rig.block(&impulse()).audio];
        out.extend((0..59).map(|_| rig.silent().audio));
        out
    };
    let first = play(&mut rig);
    let parked = (0..3_000).any(|_| rig.silent().status == Some(Status::Idle));
    assert!(parked, "it parks past its tail");
    for _ in 0..7 {
        assert_eq!(rig.silent().status, Some(Status::Idle));
    }
    let second = play(&mut rig);
    assert_eq!(first, second);
    assert!(first.iter().flatten().flatten().any(|&s| s != 0.0));
}

/// The output is wet only and silent for silence: nothing sounds until the earliest output tap
/// (266 frames at 29761 Hz, 429 at 48 kHz) has gone by, so blocks 0 to 5 of an impulse are
/// silent and block 6 (frames 384 to 447) is the first to sound; and a silent input makes
/// exactly silent output.
#[test]
fn the_output_is_wet_only_and_silent_for_silence() {
    let mut rig = Rig::new(PlateReverbConfig::new());
    let mut blocks = vec![rig.block(&impulse())];
    blocks.extend((1..12).map(|_| rig.silent()));
    let sounds = |d: &Driven| d.audio.iter().flatten().any(|&s| s != 0.0);
    assert!(!blocks[..6].iter().any(sounds), "no sound before frame 429");
    assert!(sounds(&blocks[6]), "the first sound is in block 6");
    let mut quiet = Rig::new(PlateReverbConfig::new());
    for _ in 0..40 {
        assert!(quiet.silent().audio.iter().flatten().all(|&s| s == 0.0));
    }
}

/// A reset node renders as a fresh one, bit for bit.
#[test]
fn a_reset_node_renders_as_a_fresh_one() {
    let c = PlateReverbConfig::new().modulation(Bipolar::of(0.8));
    let mut used = node(c);
    for chunk in noise(5, 5_000).chunks(BLOCK) {
        drive(&mut used, SR, Planes::new(&[chunk, chunk]).unwrap(), &[]);
    }
    used.reset();
    let mut fresh = node(c);
    let mut x = vec![0.0f32; 40 * BLOCK];
    x[0] = 1.0;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for chunk in x.chunks(BLOCK) {
        a.push(drive(
            &mut used,
            SR,
            Planes::new(&[chunk, chunk]).unwrap(),
            &[],
        ));
        b.push(drive(
            &mut fresh,
            SR,
            Planes::new(&[chunk, chunk]).unwrap(),
            &[],
        ));
    }
    assert_eq!(a, b);
    assert!(a.iter().flatten().flatten().any(|&s| s != 0.0));
}

/// A moved cell lands on the 32-frame grid and the bank's ramps are on the stream's chunks,
/// not a block's: a move mid-stream gives the same render in blocks of 1, 16, 64 and 128 as in
/// one block of 1024, and the move is heard.
#[test]
fn a_moved_cell_lands_on_the_grid_whatever_the_blocks() {
    let x = noise(11, 4_096);
    let render = |block: usize, moved: bool| -> Vec<Vec<f32>> {
        let n = PlateReverbNode::<f32>::from(PlateReverbConfig::new());
        let set = n.param_set();
        let mut n = prepared(n, SR, 1_024);
        let mut out = vec![Vec::new(), Vec::new()];
        for (i, chunk) in x.chunks(block).enumerate() {
            if moved && i * block == 1_024 {
                assert!(set.set(UnitParam::Decay, 5.0));
                assert!(set.set(UnitParam::Damping, 0.6));
                assert!(set.set(UnitParam::Diffusion, Mix::new(0.2).get()));
            }
            for (o, y) in out.iter_mut().zip(drive(
                &mut n,
                SR,
                Planes::new(&[chunk, chunk]).unwrap(),
                &[],
            )) {
                o.extend(y);
            }
        }
        out
    };
    let whole = render(1_024, true);
    assert_ne!(whole, render(1_024, false), "the move is heard");
    for block in [1, 16, 64, 128] {
        assert_eq!(render(block, true), whole, "blocks of {block}");
    }
}
