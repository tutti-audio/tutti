//! `FdnNode` as the graph runs it: its ports (the freeze only where the layout can freeze),
//! its tail (the network's own, unbounded while frozen, held to by the node), the freeze
//! itself, a driven decay, and what the node is across blocks, resets, parks and clones.
//! Every figure is from the network's own queries or the definition of a freeze, not from
//! another reverb's render.

use std::sync::Arc;
use tutti_buffers::MaxBlock;
use tutti_buffers::Planes;

use atomic_float::AtomicF64;
use tutti_buffers::ChannelLayout;
use tutti_dsp::{FdnBank, FdnControls, FdnLayout, FreezeMode, MaxDecay, Network, System};
use tutti_graph::contract::{
    Driven, assert_declared_tail, assert_param_fork, drive, drive_input, prepared,
};
use tutti_graph::{
    Env, GraphBuilder, MAX_NODE_TAIL, Node, ParamIn, ParamNode, PlayClock, Prepare, Status,
    TransportChanges, TransportPosition,
};
use tutti_modulation::ControlProgramBuilder;
use tutti_nodes::automation::{ControlLaneNode, UnitLaneConfig};
use tutti_nodes::{FdnConfig, FdnNode, FreeverbConfig, FreeverbNode};
use tutti_units::{
    Bipolar, Db, Frame, Latency, Mix, SampleRate, Samples, Seconds, Tail, UnitAddr, UnitParam,
};

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn plain() -> FdnLayout {
    FdnLayout::default()
}

fn freezable() -> FdnLayout {
    FdnLayout::new(8, MaxDecay::DEFAULT, FreezeMode::Freezable).unwrap()
}

fn live(layout: FdnLayout, c: FdnControls) -> FdnNode<f32> {
    FdnNode::<f32>::from(FdnConfig::new().layout(layout).controls(c))
}

fn node(layout: FdnLayout, c: FdnControls) -> FdnNode<f32> {
    prepared(live(layout, c), SR, BLOCK)
}

struct Rig {
    node: FdnNode<f32>,
    frame: u64,
}

impl Rig {
    fn new(node: FdnNode<f32>) -> Self {
        Self { node, frame: 0 }
    }

    fn block(&mut self, input: &[f32], params: &[Option<&[f32]>]) -> Driven {
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
            Planes::new(&[input]).unwrap(),
            params,
            &[],
        )
    }

    fn silent(&mut self) -> Driven {
        self.block(&[0.0; BLOCK], &[])
    }
}

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    tutti_fixtures::noise::<f32>(seed, frames)
        .into_iter()
        .map(|x| 0.5 * x)
        .collect()
}

fn impulse() -> Vec<f32> {
    tutti_fixtures::signal::impulse::<f32>(tutti_units::Samples::new(BLOCK))
}

fn finite(d: &Driven) -> usize {
    match d.reported_tail {
        Some(Tail::Finite(n)) => n.get(),
        other => panic!("a finite reported tail, not {other:?}"),
    }
}

/// The network's own tail at -60 dB in frames at 48 kHz.
fn tail_of(layout: &FdnLayout, c: &FdnControls) -> usize {
    let mut network = Box::new(Network::new(SR));
    layout.design_into(c, SR, &mut network);
    let tutti_dsp::DecayTime::Finite(s) = network.tail(Db::new(-60.0)) else {
        panic!("finite")
    };
    (s.get() * SR.get()).ceil() as usize
}

fn energy(blocks: &[Driven]) -> f64 {
    blocks
        .iter()
        .flat_map(|d| d.audio.iter().flatten())
        .map(|&s| f64::from(s).powi(2))
        .sum()
}

/// A layout that cannot freeze offers ten params and a finite tail ceiling; one that can
/// offers an eleventh, the freeze, and an unbounded ceiling (so an export spends its cap:
/// freeze is opt-in for that reason). The addresses are the doc's: three decays at indices 0
/// (mid), 1 (low), 2 (high).
#[test]
fn the_freeze_is_a_port_only_where_the_layout_can_freeze() {
    let (p, f) = (
        node(plain(), FdnControls::DEFAULT),
        node(freezable(), FdnControls::DEFAULT),
    );
    assert_eq!(p.shape().params.len(), 10);
    assert_eq!(f.shape().params.len(), 11);
    assert!(matches!(p.shape().tail, Tail::Finite(_)));
    assert_eq!(f.shape().tail, Tail::Unbounded);
    let (set_p, set_f) = (p.param_set(), f.param_set());
    assert_eq!(set_p.get(UnitParam::Freeze), None);
    assert_eq!(set_f.get(UnitParam::Freeze), Some(0.0));
    for (index, secs) in [(0u8, 2.0), (1, 2.4), (2, 1.2)] {
        assert_eq!(
            set_p.get(UnitAddr::new(UnitParam::Decay, index)),
            Some(secs)
        );
    }
    assert_eq!(
        set_p.get(UnitParam::Wet),
        None,
        "no mix port: the graph blends"
    );
    let Tail::Finite(ceiling) = p.shape().tail else {
        unreachable!()
    };
    assert!(
        ceiling.get() as f64 / SR.get() > 8.0,
        "the room at its longest decay"
    );
    assert_eq!(
        p.shape().latency,
        Latency::new(Samples::new(0)),
        "a pre-delay is not latency"
    );
}

/// The reported tail is the network's own `tail(-60 dB)` at the controls in force, and past
/// it the output is exactly silent, at a short and a long decay (the harness's check, which
/// also looks past the ceiling): the node cuts the room off where it says it rings out.
#[test]
fn the_reported_tail_is_the_networks_and_honest() {
    for decay in [0.3, 1.0, 2.5] {
        let c = FdnControls {
            decay: [Seconds::new(decay); 3],
            ..FdnControls::DEFAULT
        };
        let mut rig = Rig::new(node(plain(), c));
        assert_eq!(finite(&rig.block(&impulse(), &[])), tail_of(&plain(), &c));
        assert_declared_tail(&mut node(plain(), c), SR);
    }
}

/// A node of a freezable layout built with the freeze on reports `Tail::Unbounded` to the graph
/// on every block from its first, with and without input (a lossless loop with a shut input
/// never falls to the floor); the same controls on a layout that cannot freeze ignore the
/// freeze and report a finite tail, held under the declared ceiling.
#[test]
fn a_frozen_node_reports_an_unbounded_tail_to_the_graph() {
    let frozen = FdnControls {
        freeze: true,
        ..FdnControls::DEFAULT
    };
    let mut rig = Rig::new(node(freezable(), frozen));
    for block in [
        impulse(),
        vec![0.0; BLOCK],
        noise(3, BLOCK),
        vec![0.0; BLOCK],
    ] {
        assert_eq!(rig.block(&block, &[]).reported_tail, Some(Tail::Unbounded));
    }
    let mut decaying = Rig::new(node(plain(), frozen));
    let Tail::Finite(ceiling) = decaying.node.shape().tail else {
        panic!("a layout that cannot freeze declares a finite ceiling")
    };
    let reported = finite(&decaying.block(&impulse(), &[]));
    assert!(reported <= ceiling.get(), "{reported} under {ceiling:?}");
}

/// A decay raised while the short one rings out is reported by the node as the larger of what
/// is owed and the new settings' tail, from the last non-silent input frame and under the
/// declared ceiling: the silence heard since does not reduce it (the clock measures from the last
/// input), and the move to a longer tail is not measured from the move. The expected figures are
/// the bank's own `tail_of` for the two settings and its ceiling, in whole frames, with no
/// latency (the node declares none).
#[test]
fn the_node_reports_a_raised_decay_as_the_larger_of_what_is_owed_and_the_new_tail() {
    let layout = FdnLayout::new(
        8,
        MaxDecay::new(Seconds::new(2.0)).unwrap(),
        FreezeMode::Decaying,
    )
    .unwrap();
    let decays = |secs: f64| FdnControls {
        decay: [Seconds::new(secs); 3],
        ..FdnControls::DEFAULT
    };
    let (short, long) = (decays(0.3), decays(2.0));
    let mut bank = FdnBank::<f32>::new(&layout, SR);
    let ramps = bank.ramps();
    let frames = |t: Tail| match t {
        Tail::Finite(n) => n.get(),
        other => panic!("a finite tail, not {other:?}"),
    };
    let (owed, new, ceiling) = (
        frames(bank.tail_of(&ramps, &short)),
        frames(bank.tail_of(&ramps, &long)),
        frames(bank.tail()),
    );
    assert!(owed < new && new <= ceiling, "{owed} < {new} <= {ceiling}");
    let mut rig = Rig::new(node(layout, short));
    let cells = rig.node.param_set();
    for chunk in noise(2, 4 * BLOCK).chunks(BLOCK) {
        assert_eq!(finite(&rig.block(chunk, &[])), owed);
    }
    // Part of the short ring-out, a fifth of it, heard in silence: still owed in full.
    for _ in 0..owed / 5 / BLOCK {
        assert_eq!(finite(&rig.silent()), owed, "silence does not reduce it");
    }
    for index in 0..3u8 {
        assert!(cells.set(UnitAddr::new(UnitParam::Decay, index), 2.0));
    }
    let after: Vec<usize> = (0..8).map(|_| finite(&rig.silent())).collect();
    assert_eq!(
        after.last().copied(),
        Some(owed.max(new).min(ceiling)),
        "the report after the move: {after:?}"
    );
}

/// A freeze reports an unbounded tail and holds: with the input silent the level a second
/// after the freeze and four seconds later differ by under half a dB, and the node is never
/// parked. Thawed, it reports a finite tail again, decays and is eventually idle.
#[test]
fn a_freeze_holds_and_a_thaw_lets_go() {
    let mut rig = Rig::new(node(freezable(), FdnControls::DEFAULT));
    let cells = rig.node.param_set();
    for chunk in noise(1, 20_000).chunks(BLOCK) {
        rig.block(chunk, &[]);
    }
    assert!(cells.set(UnitParam::Freeze, 1.0));
    let run = |rig: &mut Rig, seconds: f64| -> Vec<Driven> {
        (0..(seconds * SR.get()) as usize / BLOCK)
            .map(|_| rig.silent())
            .collect()
    };
    let _ramp = run(&mut rig, 0.3);
    let early = run(&mut rig, 0.2);
    assert!(
        early
            .iter()
            .all(|d| d.reported_tail == Some(Tail::Unbounded))
    );
    let _between = run(&mut rig, 4.0);
    let late = run(&mut rig, 0.2);
    let db = |a: f64, b: f64| 10.0 * (b / a).log10();
    assert!(db(energy(&early), energy(&late)).abs() < 0.5, "held");
    assert!(
        late.iter().all(|d| d.status == Some(Status::Modified)),
        "never parked"
    );

    assert!(cells.set(UnitParam::Freeze, 0.0));
    let thawed = run(&mut rig, 0.1);
    assert!(thawed.last().unwrap().reported_tail != Some(Tail::Unbounded));
    let fall = run(&mut rig, 12.0);
    assert!(
        fall.last().unwrap().status == Some(Status::Idle),
        "parked after its tail"
    );
    assert!(
        energy(&fall[..BLOCK]) > 1e3 * energy(&fall[fall.len() - 10..]),
        "it decayed"
    );
}

/// The tail is reported from the longest decay a driven port takes in the block, so a decay
/// that falls through the block is not reported short; the size and the pre-delay lengthen
/// the tail the same way: a room that is large early in the block and small at its end, a
/// pre-delay likewise.
#[test]
fn the_peak_of_a_driven_control_sets_the_tail() {
    let rising: Vec<f32> = (0..BLOCK)
        .map(|i| 0.5 + 2.0 * i as f32 / (BLOCK - 1) as f32)
        .collect();
    let none: Option<&[f32]> = None;
    let falling: Vec<f32> = rising.iter().rev().copied().collect();
    let mut rig = Rig::new(node(plain(), FdnControls::DEFAULT));
    let d = rig.block(
        &impulse(),
        &[Some(&falling), Some(&falling), Some(&falling)],
    );
    let at_peak = FdnControls {
        decay: [Seconds::new(2.5); 3],
        ..FdnControls::DEFAULT
    };
    assert!(
        finite(&d) >= tail_of(&plain(), &at_peak),
        "{} against the peak's",
        finite(&d)
    );
    let big = FdnControls {
        size: Mix::new(1.0),
        pre_delay: Seconds::new(0.2),
        ..FdnControls::DEFAULT
    };
    let (high, low): (Vec<f32>, Vec<f32>) = (
        (0..BLOCK).map(|i| if i < 8 { 1.0 } else { 0.1 }).collect(),
        (0..BLOCK).map(|i| if i < 8 { 0.2 } else { 0.0 }).collect(),
    );
    let mut rig = Rig::new(node(plain(), FdnControls::DEFAULT));
    let d = rig.block(
        &impulse(),
        &[none, none, none, Some(&high), none, Some(&low)],
    );
    assert!(
        finite(&d) >= tail_of(&plain(), &big),
        "{} against the large room's {}",
        finite(&d),
        tail_of(&plain(), &big)
    );
}

/// A real control lane in a real graph reaches the decay: with the cell at its default, a
/// lane holding the mid decay at 6 s makes a noise burst ring far longer than one holding it
/// at 0.3 s.
#[test]
fn a_lane_driving_the_decay_changes_how_long_it_rings() {
    let ring = |decay: f64| -> f64 {
        let node = live(plain(), FdnControls::DEFAULT);
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let (n, _) = g.add_with_controls(node);
        g.connect_input(0, n, 0).connect_output(n, 0, 0);
        g.connect_output(n, 1, 1);
        let mut r = g
            .renderer(Prepare::new(SR, MaxBlock::of(Samples::new(100))))
            .expect("the graph builds");
        let at = ParamIn {
            node: n,
            param: UnitAddr::new(UnitParam::Decay, 0),
        };
        let program = ControlProgramBuilder::new().build().expect("a program");
        let lane = ControlLaneNode::from(
            UnitLaneConfig::new(UnitParam::Decay, program)
                .knob(Arc::new(AtomicF64::new(decay)).into()),
        );
        let key = tutti_graph::NodeKey(9_000);
        let editor = r.editor_mut();
        editor.insert(key, "lane", lane);
        editor
            .spec_mut()
            .connect_param(at, tutti_graph::EventOutputPort::new(key, 0));
        editor.commit().expect("the lane commits");
        let frames = 96_000;
        let mut x = noise(8, 4_800);
        x.resize(frames, 0.0);
        let out = r.render_input(Planes::new(&[&x]).unwrap());
        let energy = |from: usize, to: usize| -> f64 {
            out.iter()
                .flat_map(|c| &c[from..to])
                .map(|&s| f64::from(s).powi(2))
                .sum()
        };
        energy(72_000, 90_000) / energy(12_000, 30_000)
    };
    let (short, long) = (ring(0.3), ring(6.0));
    assert!(
        long > 100.0 * short,
        "6 s keeps {long} of its energy, 0.3 s {short}"
    );
}

/// A fork starts from the values set, shares no cell and rings nothing: the harness's fork
/// checks over every cell of both layouts.
#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(live(plain(), FdnControls::DEFAULT));
    assert_param_fork(live(freezable(), FdnControls::DEFAULT));
}

/// A non-finite write to any cell reads as unchanged: the render equals one that never saw
/// it.
#[test]
fn a_non_finite_write_is_ignored() {
    let c = FdnControls {
        decay: [Seconds::new(0.9); 3],
        ..FdnControls::DEFAULT
    };
    let (mut hostile, mut calm) = (Rig::new(node(plain(), c)), Rig::new(node(plain(), c)));
    let set = hostile.node.param_set();
    for (i, chunk) in noise(4, 8_192).chunks(BLOCK).enumerate() {
        if i == 3 {
            for addr in set.params().collect::<Vec<_>>() {
                set.set(addr, f64::NAN);
            }
        }
        assert_eq!(
            hostile.block(chunk, &[]).audio,
            calm.block(chunk, &[]).audio
        );
    }
}

/// A live Freeverb node has the room and the damping as params, a live FDN node its eleven
/// controls, and the FDN is stereo out.
#[test]
fn a_live_node_has_its_layouts_params() {
    let f = FreeverbNode::<f32>::from(FreeverbConfig::new());
    assert_eq!(f.param_set().params().count(), 2, "the room, the damping");
    let n = live(freezable(), FdnControls::DEFAULT);
    assert_eq!(n.param_set().params().count(), 11);
    assert_eq!(n.shape().audio_out(), ChannelLayout::STEREO);
}

/// A cell that moves lands on the stream's grid, whatever the blocks: the decay, the size and
/// the diffusion written between two blocks at frame 1024 (a multiple of every block below)
/// give the same render in blocks of 1, 16, 64, 128 and 1024 (the node against itself under
/// cuts), because the reader takes each whole at the grid and the bank's ramps are on the
/// stream's chunks, not a block's. And the move is heard.
#[test]
fn a_moved_cell_lands_on_the_grid_whatever_the_blocks() {
    let x = noise(11, 4_096);
    let render = |block: usize, moved: bool| -> Vec<Vec<f32>> {
        let n = live(plain(), FdnControls::DEFAULT);
        let set = n.param_set();
        let mut n = prepared(n, SR, 1_024);
        let mut out = vec![Vec::new(), Vec::new()];
        for (i, chunk) in x.chunks(block).enumerate() {
            if moved && i * block == 1_024 {
                assert!(set.set(UnitAddr::new(UnitParam::Decay, 0), 5.0));
                assert!(set.set(UnitParam::RoomSize, 0.9));
                assert!(set.set(UnitParam::Diffusion, 0.2));
            }
            for (o, y) in out
                .iter_mut()
                .zip(drive(&mut n, SR, Planes::new(&[chunk]).unwrap(), &[]))
            {
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

/// A control the graph drives is read on the stream's 32-frame grid and at the corners of its
/// control signal, not at the block's last frame, so what is heard does not depend on how the stream is
/// cut into blocks (the node against itself under cuts); and the control signal is heard, since the
/// cell it overrides is a different decay.
#[test]
fn a_driven_slow_control_is_sampled_on_the_grid_whatever_the_blocks() {
    let len = 4_096;
    let signal: Vec<f32> = (0..len)
        .map(|n| 0.4 + 5.0 * n as f32 / len as f32)
        .collect();
    let x = noise(12, len);
    let render = |block: usize, driven: bool| -> Vec<Vec<f32>> {
        let mut n = prepared(live(plain(), FdnControls::DEFAULT), SR, 1_024);
        let mut out = vec![Vec::new(), Vec::new()];
        for (i, chunk) in x.chunks(block).enumerate() {
            let at = i * block;
            let l = &signal[at..at + chunk.len()];
            let params = [driven.then_some(l)];
            for (o, y) in
                out.iter_mut()
                    .zip(drive(&mut n, SR, Planes::new(&[chunk]).unwrap(), &params))
            {
                o.extend(y);
            }
        }
        out
    };
    let whole = render(1_024, true);
    assert_ne!(whole, render(1_024, false), "the control signal is heard");
    for block in [1, 16, 64, 128] {
        assert_eq!(render(block, true), whole, "blocks of {block}");
    }
}

/// A reset node is a fresh one: after a noise burst and a `reset`, an impulse renders the
/// same bits as it does on a node that never ran.
#[test]
fn a_reset_node_renders_as_a_fresh_one() {
    let c = FdnControls {
        modulation: Bipolar::of(0.8),
        ..FdnControls::DEFAULT
    };
    let mut used = node(plain(), c);
    for chunk in noise(5, 5_000).chunks(BLOCK) {
        drive(&mut used, SR, Planes::new(&[chunk]).unwrap(), &[]);
    }
    used.reset();
    let mut fresh = node(plain(), c);
    // Past the shortest line, so the echo is heard.
    let mut x = vec![0.0f32; 40 * BLOCK];
    x[0] = 1.0;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for chunk in x.chunks(BLOCK) {
        a.push(drive(&mut used, SR, Planes::new(&[chunk]).unwrap(), &[]));
        b.push(drive(&mut fresh, SR, Planes::new(&[chunk]).unwrap(), &[]));
    }
    assert_eq!(a, b);
    assert!(a.iter().flatten().flatten().any(|&s| s != 0.0));
}

/// A node that parked on its tail wakes as a fresh one at the start of its stream: the same
/// impulse renders the same bits the second time as the first, with the room's lines, its
/// modulation phases and its grid all started over.
#[test]
fn a_parked_node_wakes_as_a_fresh_one() {
    let c = FdnControls {
        decay: [Seconds::new(0.3); 3],
        ..FdnControls::DEFAULT
    };
    let mut rig = Rig::new(node(plain(), c));
    let take = |rig: &mut Rig| -> Vec<Vec<Vec<f32>>> {
        let mut out = vec![rig.block(&impulse(), &[]).audio];
        out.extend((0..39).map(|_| rig.silent().audio));
        out
    };
    let first = take(&mut rig);
    let mut parked = false;
    for _ in 0..2_000 {
        if rig.silent().status == Some(Status::Idle) {
            parked = true;
            break;
        }
    }
    assert!(parked, "it parks past its tail");
    // A few blocks into the park: the wake lands at a stream position that is not 0.
    for _ in 0..7 {
        assert_eq!(rig.silent().status, Some(Status::Idle));
    }
    let second = take(&mut rig);
    assert_eq!(first, second);
    assert!(first.iter().flatten().flatten().any(|&s| s != 0.0));
}

/// A clone shares the cells and starts silent: it renders an impulse as a fresh node does,
/// whatever the original had rung, and a write through the original's `ParamSet` is the
/// clone's too.
#[test]
fn a_clone_starts_silent_and_shares_the_cells() {
    let mut used = node(plain(), FdnControls::DEFAULT);
    for chunk in noise(6, 6_000).chunks(BLOCK) {
        drive(&mut used, SR, Planes::new(&[chunk]).unwrap(), &[]);
    }
    let mut clone = used.clone();
    let mut fresh = node(plain(), FdnControls::DEFAULT);
    // Past the shortest line, so the echo is heard.
    let mut x = vec![0.0f32; 40 * BLOCK];
    x[0] = 1.0;
    let mut heard = false;
    for chunk in x.chunks(BLOCK) {
        let (a, b) = (
            drive(&mut clone, SR, Planes::new(&[chunk]).unwrap(), &[]),
            drive(&mut fresh, SR, Planes::new(&[chunk]).unwrap(), &[]),
        );
        heard |= a.iter().flatten().any(|&s| s != 0.0);
        assert_eq!(a, b);
    }
    assert!(heard);
    assert!(used.param_set().set(UnitParam::RoomSize, 0.9));
    assert_eq!(clone.param_set().get(UnitParam::RoomSize), Some(0.9));
}

/// A size on the move does not stretch the decay: the gains follow the length the lines have
/// while they shrink half a frame a chunk (about 7 s from size 1 to 0), so the size-0
/// network's tail is the tail. A node whose lines kept the gains designed for the target, 3
/// times too long a trip at first, would ring 3 times as long as it reports and be cut off
/// about 30 dB under the burst. Here a 1 s room at size 1 hears a burst, the size drops to 0
/// and the input is silent; the node parks (it is idle in the end), and every one of the last
/// 8 blocks it renders before parking is under 60 dB below the burst (it measures -72 dB; with
/// the lines' gains left as designed for the target it parks at -32 dB): what it cuts
/// off is under the floor.
#[test]
fn a_size_on_the_move_is_not_parked_while_it_still_rings() {
    let c = FdnControls {
        decay: [Seconds::new(1.0); 3],
        size: Mix::new(1.0),
        modulation: Bipolar::of(0.0),
        ..FdnControls::DEFAULT
    };
    let mut rig = Rig::new(node(plain(), c));
    let cells = rig.node.param_set();
    let rms = |d: &Driven| -> f64 {
        let n: usize = d.audio.iter().map(Vec::len).sum();
        (energy(std::slice::from_ref(d)) / n as f64).sqrt()
    };
    let mut burst = Vec::new();
    for chunk in noise(21, 24_000).chunks(BLOCK) {
        burst.push(rms(&rig.block(chunk, &[])));
    }
    let reference = burst[burst.len() - 8..].iter().sum::<f64>() / 8.0;
    assert!(reference > 0.0);
    assert!(cells.set(UnitParam::RoomSize, 0.0));
    let mut levels = Vec::new();
    let mut parked = false;
    for _ in 0..(20.0 * SR.get() / BLOCK as f64) as usize {
        let d = rig.silent();
        if d.status == Some(Status::Idle) {
            parked = true;
            break;
        }
        levels.push(rms(&d));
    }
    assert!(parked, "it parks in the end");
    assert!(levels.len() >= 8, "it rang for {} blocks", levels.len());
    let last = levels[levels.len() - 8..]
        .iter()
        .copied()
        .fold(0.0, f64::max);
    let db = 20.0 * (last / reference).log10();
    assert!(db < -60.0, "{db} dB under the burst when it parked");
}

/// A node frozen from the start with no input, then thawed, renders its ring-out in silence
/// and parks: and what wakes is a fresh node, bit for bit. The kernel has rendered all along
/// without hearing a sample, so a park that resets only a node that heard input leaves its
/// grid, its modulation phases and its ramps where the thaw left them.
#[test]
fn a_node_frozen_without_input_wakes_fresh_after_the_thaw_parks() {
    let frozen = FdnControls {
        freeze: true,
        ..FdnControls::DEFAULT
    };
    let mut rig = Rig::new(node(freezable(), frozen));
    let cells = rig.node.param_set();
    for _ in 0..500 {
        assert_eq!(
            rig.silent().status,
            Some(Status::Modified),
            "frozen: never parked"
        );
    }
    assert!(cells.set(UnitParam::Freeze, 0.0));
    let mut parked = false;
    for _ in 0..(20.0 * SR.get() / BLOCK as f64) as usize {
        if rig.silent().status == Some(Status::Idle) {
            parked = true;
            break;
        }
    }
    assert!(parked, "it parks past the thawed tail");
    let take = |rig: &mut Rig| -> Vec<Vec<Vec<f32>>> {
        let mut out = vec![rig.block(&impulse(), &[]).audio];
        out.extend((0..39).map(|_| rig.silent().audio));
        out
    };
    let mut fresh = Rig::new(node(freezable(), FdnControls::DEFAULT));
    let woken = take(&mut rig);
    assert_eq!(woken, take(&mut fresh));
    assert!(woken.iter().flatten().flatten().any(|&s| s != 0.0));
}

/// The declared tail is a figure the graph takes or it is unbounded: the default 8 s room at
/// 192 kHz declares a finite ceiling within `MAX_NODE_TAIL` (sixty seconds, converted at the
/// rate) and under 2.5 million frames (the 8 s decay to the floor deepened by the eight lines the mixing
/// matrix lets sum, 18 dB: 10.4 s, with the longest pre-delay and line: nothing stretches it), a 19 s room at 48 kHz too, and the longest layout
/// the type allows (60 s at 192 kHz, 11.5 million frames and more) declares `Tail::Unbounded`
/// and not a finite figure the graph would refuse or count as never decaying. The node still
/// narrows an unbounded ceiling to what rings: a thawed room reports and parks on its own.
#[test]
fn the_declared_tail_is_within_the_graphs_bound_or_unbounded() {
    let at = |layout: FdnLayout, rate: SampleRate| {
        prepared(live(layout, FdnControls::DEFAULT), rate, BLOCK)
            .shape()
            .tail
    };
    let Tail::Finite(default) = at(plain(), SampleRate::SR_192K) else {
        panic!("a finite ceiling")
    };
    assert!(
        default <= MAX_NODE_TAIL.to_samples_ceil(SampleRate::SR_192K),
        "{default:?}"
    );
    assert!(default.get() as f64 > 8.0 * 192_000.0);
    assert!(
        default.get() < 2_500_000,
        "nothing stretches the ceiling: {default:?}"
    );
    let nineteen =
        FdnLayout::new(8, MaxDecay::of(Seconds::new(19.0)), FreezeMode::Decaying).unwrap();
    let Tail::Finite(long) = at(nineteen, SampleRate::SR_48K) else {
        panic!("a finite ceiling")
    };
    assert!(
        long <= MAX_NODE_TAIL.to_samples_ceil(SampleRate::SR_48K)
            && long.get() as f64 > 19.0 * 48_000.0,
        "{long:?}"
    );
    // A 30 s room at 96 kHz (its floor 18 dB deeper, 39 s) is past 2.88 million frames (what
    // bounded a tail at 48 kHz) and within the sixty seconds the graph takes at that rate.
    let room = FdnLayout::new(8, MaxDecay::of(Seconds::new(30.0)), FreezeMode::Decaying).unwrap();
    let Tail::Finite(room) = at(room, SampleRate::SR_96K) else {
        panic!("a finite ceiling")
    };
    assert!(
        room.get() as f64 > 30.0 * 96_000.0
            && room <= MAX_NODE_TAIL.to_samples_ceil(SampleRate::SR_96K),
        "{room:?}"
    );
    assert!(room.get() > 2_880_000, "{room:?}");
    let cap = FdnLayout::new(
        16,
        MaxDecay::new(MaxDecay::CAP).unwrap(),
        FreezeMode::Decaying,
    )
    .unwrap();
    assert_eq!(at(cap, SampleRate::SR_192K), Tail::Unbounded);
}
