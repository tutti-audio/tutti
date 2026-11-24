//! `FreeverbNode` as the graph runs it: its
//! ports, its tail (a ceiling it narrows and honours), its controls (a cell, or
//! a port the graph modulates), and its fork. Every figure is from the
//! definition (the Schroeder RT60 of the comb and the all-pass chain's echoes, an
//! impulse's first echo, a decay measured on a real lane), not from another reverb's
//! render.

use std::sync::Arc;
use tutti_buffers::Planes;

use atomic_float::AtomicF64;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_dsp::{FreeverbBank, FreeverbControls};
use tutti_fixtures::Noise;
use tutti_graph::contract::{assert_declared_tail, assert_param_fork, drive_input, prepared};
use tutti_graph::{
    Env, GraphBuilder, IntoNode, Node, ParamFork, ParamIn, ParamNode, Prepare, Status,
    TransportChanges, TransportPosition,
};
use tutti_modulation::ControlProgramBuilder;
use tutti_nodes::automation::{ControlLaneNode, UnitLaneConfig};
use tutti_nodes::{FdnConfig, FdnNode, FreeverbConfig, FreeverbNode};
use tutti_units::{Frame, Mix, SampleRate, Samples, Tail, UnitAddr, UnitParam};

// These tests pin the `f32` instantiation, the session's audio format, so their tolerances
// are `f32`'s.

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;

fn gm() -> FreeverbControls {
    FreeverbControls::GENERAL_MIDI
}

fn controls(room: f64, damp: f64) -> FreeverbControls {
    FreeverbControls {
        room: Mix::new(room),
        damp: Mix::new(damp),
    }
}

/// A live (modulatable) Freeverb node at `c`, prepared at 48 kHz.
fn live(c: FreeverbControls) -> FreeverbNode<f32> {
    prepared(
        FreeverbNode::<f32>::from(FreeverbConfig::new().controls(c)),
        SR,
        BLOCK,
    )
}

/// A node driven block by block, at its own running frame.
struct Rig<N> {
    node: N,
    frame: u64,
}

impl<N: Node> Rig<N> {
    fn new(node: N) -> Self {
        Self { node, frame: 0 }
    }

    fn block(&mut self, input: &[f32], params: &[Option<&[f32]>]) -> tutti_graph::contract::Driven {
        let env = Env {
            frame: Frame::new(self.frame),
            sample_rate: SR,
            block_len: Samples::new(input.len()),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
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

    fn silent(&mut self) -> tutti_graph::contract::Driven {
        self.block(&[0.0; BLOCK], &[])
    }
}

fn noise(seed: u32, frames: usize) -> Vec<f32> {
    Noise::seeded(seed).take(frames).map(|x| 0.5 * x).collect()
}

fn impulse() -> Vec<f32> {
    tutti_fixtures::signal::impulse(tutti_units::Samples::new(BLOCK))
}

fn reported(d: &tutti_graph::contract::Driven) -> usize {
    match d.reported_tail {
        Some(Tail::Finite(n)) => n.get(),
        other => panic!("a finite reported tail, not {other:?}"),
    }
}

/// The tail node reports for `c`, in frames at 48 kHz: what the first block of an impulse
/// reports. [`the_tail_is_the_schroeder_figure`] holds it to the definition.
fn tail_of(c: FreeverbControls) -> usize {
    reported(&Rig::new(live(c)).block(&impulse(), &[]))
}

/// The tail the definition gives for a room, in frames at 48 kHz: the longest comb (1640
/// frames at 44.1 kHz, scaled to the rate) rings for the Schroeder trips `ln(1e-3) / ln g` of
/// its loop gain `g = room * 0.28 + 0.7`, each a period and the interpolation's two frames,
/// and the four all-passes add their own echoes, `a^(k-1)` at `kD` for `a = 0.5`: ten trips
/// to -60 dB, eleven periods. The figure errs long.
fn schroeder_tail(room: f64) -> i64 {
    let scale = |t: usize| (SR.get() / 44_100.0 * t as f64).round();
    let trips = |q: f64| (libm::log(1e-3) / libm::log(q)).ceil();
    let chain: f64 = [556usize, 441, 341, 225]
        .iter()
        .map(|&d| (1.0 + trips(0.5)) * (scale(d + 23) + 2.0))
        .sum();
    let g = room * 0.28 + 0.7;
    let comb = (1.0 + trips(g)) * (scale(1617 + 23) + 2.0);
    (comb + chain).ceil() as i64
}

/// The node's tail is the definition's, to the frame the arithmetic rounds to, whatever the
/// damping (it only shortens the ring).
#[test]
fn the_tail_is_the_schroeder_figure() {
    for room in [0.0f64, 0.1, 0.5, 0.9, 1.0] {
        for damp in [0.0f64, 0.5, 1.0] {
            let got = tail_of(controls(room, damp)) as i64;
            let want = schroeder_tail(room);
            assert!((got - want).abs() <= 1, "room {room}: {got} against {want}");
        }
    }
}

/// A Freeverb, the General MIDI send included, takes a mono input and gives a stereo one, with
/// no latency; it offers the room and the damping and no mix (the blend is the graph's mix).
/// Its declared tail is the ceiling (the room at its largest), and the first block reports the
/// General MIDI room's own figure, the Schroeder tail of room 0.5.
#[test]
fn the_ports_and_the_ceiling() {
    let send = prepared(FreeverbNode::<f32>::from(FreeverbConfig::new()), SR, BLOCK);
    let shape = send.shape();
    assert_eq!(shape.audio_in(), ChannelLayout::MONO);
    assert_eq!(shape.audio_out(), ChannelLayout::STEREO);
    assert_eq!(shape.latency.samples(), Samples::ZERO);
    assert_eq!(shape.params.len(), 2, "the room and the damping");
    assert_eq!(send.param_set().params().count(), 2);
    assert_eq!(
        send.param_set().get(UnitParam::Wet),
        None,
        "wet only: the blend is the graph's mix, not a port"
    );
    let Tail::Finite(ceiling) = shape.tail else {
        panic!("finite")
    };
    let reported = tail_of(gm()) as i64;
    assert!(
        (reported - schroeder_tail(0.5)).abs() <= 1,
        "the send reports {reported}"
    );
    assert!(
        ceiling.get() > 2 * tail_of(gm()),
        "the ceiling is the worst case (room 1), far above the General MIDI room's"
    );
}

/// The ports are in a fixed order, which the graph's wires and a saved project
/// name by index: the design's cells in the order its params list them. Freeverb
/// offers `[RoomSize, Damping]`; the FDN offers the three decays, the size, the diffusion, the pre-delay, the
/// modulation depth and rate, the width, the wet gain and the freeze.
#[test]
fn the_port_order_is_the_cells_in_their_order() {
    let freeverb = live(gm());
    assert_eq!(
        freeverb.shape().params.as_slice(),
        [
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Damping),
        ]
    );
    let fdn: FdnNode<f32> = prepared(FdnNode::<f32>::from(FdnConfig::new()), SR, BLOCK);
    assert_eq!(
        fdn.shape().params.as_slice(),
        [
            UnitAddr::new(UnitParam::Decay, 0),
            UnitAddr::new(UnitParam::Decay, 1),
            UnitAddr::new(UnitParam::Decay, 2),
            UnitAddr::first(UnitParam::RoomSize),
            UnitAddr::first(UnitParam::Diffusion),
            UnitAddr::first(UnitParam::DelayTime),
            UnitAddr::first(UnitParam::Depth),
            UnitAddr::first(UnitParam::Rate),
            UnitAddr::first(UnitParam::StereoSpread),
            UnitAddr::new(UnitParam::GainDb, 0),
            UnitAddr::first(UnitParam::Freeze),
        ]
    );
}

/// A live node narrows its ceiling each block to the tail of the network it is
/// running, from the network's own `tail` query at -60 dB: a smaller room reports
/// less than a bigger one, and both less than the ceiling.
#[test]
fn a_live_node_reports_the_tail_of_its_current_network() {
    let mut last = 0;
    let ceiling = match live(gm()).shape().tail {
        Tail::Finite(n) => n.get(),
        t => panic!("{t:?}"),
    };
    for room in [0.1, 0.5, 0.9] {
        let mut rig = Rig::new(live(controls(room, 0.5)));
        let first = rig.block(&impulse(), &[]);
        let got = reported(&first);
        assert!(
            (got as i64 - schroeder_tail(room)).abs() <= 1,
            "room {room}: {got}"
        );
        assert!(got > last && got < ceiling, "room {room}: {got}");
        last = got;
    }
}

/// The reported tail holds: past it the output is exactly silent, for the
/// General MIDI room and a short and a long one (the harness's check, which
/// also looks past the ceiling).
#[test]
fn the_reported_tail_is_honest() {
    assert_declared_tail(&mut live(gm()), SR);
    assert_declared_tail(&mut live(controls(0.1, 0.5)), SR);
    assert_declared_tail(&mut live(controls(0.9, 0.2)), SR);
}

/// What the node cuts off at the tail is under the floor: the bank alone, left to ring, is
/// below -60 dB of its loudest frame (an impulse's answer) at every frame past the tail the
/// node reports, so parking the room there takes nothing audible.
#[test]
fn the_cut_discards_only_what_is_below_the_floor() {
    for c in [gm(), controls(0.9, 0.2), controls(0.1, 0.9)] {
        let tail = tail_of(c);
        let mut bank = FreeverbBank::<f32>::new(SR);
        let mut ramps = bank.ramps();
        bank.retarget(&mut ramps, &c);
        ramps.jump_to_target();
        let frames = tail + 20_000;
        let mut x = vec![0.0f32; frames];
        x[0] = 1.0;
        let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        let ins = [x.as_slice()];
        let mut outs: [&mut [f32]; 2] = [&mut l, &mut r];
        bank.process(
            &mut ramps,
            &mut tutti_buffers::InputOutput::paired(
                tutti_buffers::Planes::new(&ins).unwrap(),
                tutti_buffers::PlanesMut::new(&mut outs).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let peak = l.iter().chain(&r).fold(0.0f32, |m, s| m.max(s.abs()));
        let after = l[tail + 1..]
            .iter()
            .chain(&r[tail + 1..])
            .fold(0.0f32, |m, s| m.max(s.abs()));
        let floor = peak * 10f32.powf(-60.0 / 20.0);
        assert!(after <= floor, "{c:?}: {after} above {floor} past {tail}");
    }
}

/// The room is cut at its tail, to the frame: after input that ends at frame
/// `n - 1`, the last frame to sound is `n - 1 + tail` (the tail counted from the
/// last frame of input) and every frame after it is exactly zero. The input is
/// loud noise, so the answer is dense at the cut and a frame early or late shows.
#[test]
fn the_room_is_cut_at_its_tail_to_the_frame() {
    let n = 4_000;
    let tail = tail_of(gm());
    let mut x: Vec<f32> = noise(9, n).iter().map(|s| 1_000.0 * s).collect();
    x.resize(n + tail + 4 * BLOCK, 0.0);
    // Block-aligned input ends at a block edge: 4000 = 62 * 64 + 32, so the last
    // input frame is mid-block, as it may be.
    let mut rig = Rig::new(prepared(
        FreeverbNode::<f32>::from(FreeverbConfig::new()),
        SR,
        BLOCK,
    ));
    let mut out = [Vec::new(), Vec::new()];
    for chunk in x.chunks(BLOCK) {
        for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
            all.extend(more);
        }
    }
    for (c, ch) in out.iter().enumerate() {
        let last = ch.iter().rposition(|&s| s != 0.0).expect("it sounds");
        assert_eq!(last, n - 1 + tail, "channel {c}");
    }
}

/// A shorter decay does not shorten the tail owed: the figure is held at the
/// longer one until the room has been heard out, measured from the last frame of
/// input. Room 0.9 rung by an impulse, then the cell set to 0.1: every later
/// block still reports the long room's tail, and a block of new input takes
/// off the frames since the old input and no more.
#[test]
fn a_shorter_decay_keeps_the_longer_tail_until_heard_out() {
    let mut rig = Rig::new(live(controls(0.9, 0.5)));
    let long = tail_of(controls(0.9, 0.5));
    let short = tail_of(controls(0.1, 0.5));
    assert!(short < long);
    assert_eq!(reported(&rig.block(&impulse(), &[])), long);
    let cell = rig.node.param_set();
    assert!(cell.set(UnitParam::RoomSize, 0.1));
    for _ in 0..8 {
        assert_eq!(
            reported(&rig.silent()),
            long,
            "held while it may still ring"
        );
    }
    // New input at frame 0 of a block: the old ring ends `long` after the
    // impulse, so `long - (8 blocks + 1 block's frames)` are left, and the new
    // input's own short tail is less than that.
    let owed = long - 9 * BLOCK;
    assert!(short < owed);
    assert_eq!(reported(&rig.block(&impulse(), &[])), owed);
}

/// Past its tail the node cuts the room off, resets and parks (`Status::Idle`),
/// and an idle node answers new input as a fresh one does, bit for bit: the
/// state it holds is exactly zero. Before the tail has elapsed it is not idle.
#[test]
fn an_idle_reverb_wakes_as_a_fresh_one() {
    let mut rig = Rig::new(live(gm()));
    // Loud, so what the cut leaves behind would be well above the flush floor.
    let burst: Vec<f32> = noise(1, BLOCK).iter().map(|s| 1_000.0 * s).collect();
    rig.block(&burst, &[]);
    let tail = tail_of(gm());
    let mut idle_at = None;
    for b in 0..(tail / BLOCK + 4) {
        let d = rig.silent();
        if d.status == Some(Status::Idle) && idle_at.is_none() {
            idle_at = Some(b);
        }
        assert!(
            idle_at.is_some() || d.status == Some(Status::Modified),
            "block {b}: {:?}",
            d.status
        );
        if idle_at.is_some() {
            assert_eq!(d.status, Some(Status::Idle), "stays idle");
        }
    }
    let idle_at = idle_at.expect("it parks after its tail");
    assert!(
        (idle_at + 1) * BLOCK >= tail - BLOCK,
        "not before the tail: block {idle_at} of a {tail}-frame tail"
    );
    // Long enough for the first echoes to come round, where any old state shows.
    let probe = noise(2, 4_096);
    let mut fresh = Rig::new(live(gm()));
    for chunk in probe.chunks(BLOCK) {
        assert_eq!(rig.block(chunk, &[]).audio, fresh.block(chunk, &[]).audio);
    }
}

/// A port the graph drives is read on the stream's 32-frame grid, as the FDN's is: a control signal
/// held at 0.9 renders as a cell at 0.9, whatever the blocks, and a control signal that rises through
/// the stream renders the same cut into blocks of 32, 64 and 100 frames.
#[test]
fn a_modulated_port_is_read_on_the_grid() {
    // Long enough for the combs to come round, where the room is heard.
    let x = noise(3, 4_096);
    let held = vec![0.9f32; BLOCK];
    let mut by_port = Rig::new(live(controls(0.5, 0.5)));
    let mut by_cell = Rig::new(live(controls(0.9, 0.5)));
    let none: Option<&[f32]> = None;
    for chunk in x.chunks(BLOCK) {
        assert_eq!(
            by_port.block(chunk, &[Some(&held), none]).audio,
            by_cell.block(chunk, &[]).audio
        );
    }
    let rising: Vec<f32> = (0..x.len())
        .map(|i| 0.1 + 0.8 * i as f32 / (x.len() - 1) as f32)
        .collect();
    let render = |block: usize, driven: bool| -> Vec<Vec<f32>> {
        let mut rig = Rig::new(live(controls(0.5, 0.5)));
        let mut out = vec![Vec::new(), Vec::new()];
        for (k, chunk) in x.chunks(block).enumerate() {
            let signal = &rising[k * block..k * block + chunk.len()];
            let params = [driven.then_some(signal), none];
            for (all, more) in out.iter_mut().zip(rig.block(chunk, &params).audio) {
                all.extend(more);
            }
        }
        out
    };
    let whole = render(32, true);
    assert_ne!(whole, render(32, false), "the control signal is heard");
    for block in [64, 100] {
        assert_eq!(render(block, true), whole, "blocks of {block}");
    }
}

/// The ceiling the shape declares, in frames.
fn ceiling(node: &FreeverbNode<f32>) -> usize {
    match node.shape().tail {
        Tail::Finite(n) => n.get(),
        other => panic!("a finite ceiling, not {other:?}"),
    }
}

/// A port has frames for every block it has a source, so a signal that is flat for the block
/// (here, low) is a driven room: it may take any value after the block, and the block reports
/// the ceiling the shape declares, not the figure of the low room. An automation source at rest
/// also hands frames (`tutti-graph`'s `ParamState::port` frames a port whenever it has a source);
/// a node-level drive cannot deliver a param event, so a flat signal is the nearest this pins.
#[test]
fn a_flat_signal_on_the_room_reports_the_ceiling_not_the_blocks_figure() {
    let none: Option<&[f32]> = None;
    let low = vec![0.1f32; BLOCK];
    let mut rig = Rig::new(live(controls(0.5, 0.5)));
    let ceiling = ceiling(&rig.node);
    let d = rig.block(&impulse(), &[Some(&low), none]);
    assert_eq!(reported(&d), ceiling);
    assert!(
        tail_of(controls(0.1, 0.5)) < ceiling,
        "the low room's figure is below the ceiling"
    );
}

/// The tail reads the room and not the damping: moving the damping alone leaves the reported
/// tail, which is what its `no_ring` flag promises, and a driven damping reports the cells'
/// figure, below the ceiling.
#[test]
fn the_damping_does_not_change_the_reported_tail() {
    let base = tail_of(controls(0.5, 0.5));
    for damp in [0.0, 0.25, 0.75, 1.0] {
        assert_eq!(tail_of(controls(0.5, damp)), base, "damping {damp}");
    }
    let none: Option<&[f32]> = None;
    let heavy = vec![1.0f32; BLOCK];
    let mut rig = Rig::new(live(controls(0.5, 0.5)));
    let ceiling = ceiling(&rig.node);
    let d = rig.block(&impulse(), &[none, Some(&heavy)]);
    assert_eq!(reported(&d), base);
    assert!(base < ceiling);
}

/// With the port gone the cells' figure applies again, but the ring the ceiling owed is held
/// until heard out, measured from the last input frame: eight silent blocks and a block of
/// new input at its frame 0 take off nine blocks.
#[test]
fn a_released_port_keeps_the_ceilings_ring_until_heard_out() {
    let none: Option<&[f32]> = None;
    let low = vec![0.1f32; BLOCK];
    let mut rig = Rig::new(live(controls(0.1, 0.5)));
    let ceiling = ceiling(&rig.node);
    assert_eq!(
        reported(&rig.block(&impulse(), &[Some(&low), none])),
        ceiling
    );
    for _ in 0..8 {
        assert_eq!(reported(&rig.silent()), ceiling, "held");
    }
    let owed = ceiling - 9 * BLOCK;
    assert!(tail_of(controls(0.1, 0.5)) < owed);
    assert_eq!(reported(&rig.block(&impulse(), &[])), owed);
}

/// A non-finite write to a cell reads as unchanged: the node keeps the last
/// finite room, so the render equals one that never saw the NaN.
#[test]
fn a_non_finite_cell_write_is_ignored() {
    let x = noise(4, 4_096);
    let mut hostile = Rig::new(live(controls(0.9, 0.5)));
    let mut calm = Rig::new(live(controls(0.9, 0.5)));
    let set = hostile.node.param_set();
    for (i, chunk) in x.chunks(BLOCK).enumerate() {
        if i == 1 {
            set.set(UnitParam::RoomSize, f64::NAN);
            set.set(UnitParam::Damping, f64::INFINITY);
        }
        assert_eq!(
            hostile.block(chunk, &[]).audio,
            calm.block(chunk, &[]).audio
        );
    }
}

/// The node is the room alone: an impulse leaves the first frame silent on both
/// channels (no dry input in the output), and the first left echo is the input at
/// the fixed gain, 1215 frames in at 48 kHz. Its dry/wet is the graph's mix
/// (`node_mix.rs`), which blends this signal with the aligned input.
#[test]
fn the_node_is_the_room_alone() {
    let mut x = vec![0.0f32; 1_400];
    x[0] = 1.0;
    let mut rig = Rig::new(live(gm()));
    let mut out = [Vec::new(), Vec::new()];
    for chunk in x.chunks(BLOCK) {
        for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
            all.extend(more);
        }
    }
    assert!(out.iter().all(|c| c[0] == 0.0), "nothing at once");
    assert!((out[0][1_215] - FreeverbBank::<f32>::FIXED_GAIN.get() as f32).abs() < 1e-6);
}

/// A fork of the node starts from silence with the values set, and shares nothing
/// with it: the harness's fork checks, and a fork taken while the room rings
/// does not carry the ring (a fork is `fresh()` plus the graph's preroll).
#[test]
fn a_fork_starts_silent_with_the_values_set() {
    assert_param_fork(FreeverbNode::<f32>::from(
        FreeverbConfig::new().controls(gm()),
    ));

    let mut rig = Rig::new(live(controls(0.9, 0.5)));
    rig.block(&noise(5, BLOCK), &[]);
    rig.node.param_set().set(UnitParam::RoomSize, 0.3);
    let template = ParamFork::new(&rig.node);
    let mut fork = Rig::new(prepared(template.fork_node(), SR, BLOCK));
    // Silent input: the fork has nothing to ring, though the live room does.
    let forked = fork.silent().audio;
    assert!(
        forked.iter().flatten().all(|&s| s == 0.0),
        "a fork is silent"
    );
    // It runs the room set (0.3), not the room it was built with (0.9).
    let mut fresh = Rig::new(live(controls(0.3, 0.5)));
    let x = noise(6, 4 * BLOCK);
    for chunk in x.chunks(BLOCK) {
        assert_eq!(fork.block(chunk, &[]).audio, fresh.block(chunk, &[]).audio);
    }
}

/// A real control lane in a real graph reaches the reverb: with the cell at the
/// General MIDI room, a lane holding the room port at 0.9 makes the answer to a
/// noise burst ring far longer than one holding it at 0.1. The decay is measured
/// as the energy between two windows well after the burst: a bigger room keeps
/// more of it.
///
/// This is the modulation path as an engine runs it: the lane sends a segment to
/// the node's param port, the executor fuses it with the node's base, the node
/// reads the port at the block's last frame, designs, and retargets its realization.
#[test]
fn a_lane_driving_the_room_changes_how_long_it_rings() {
    let ring = |room: f64| -> f64 {
        let node = FreeverbNode::<f32>::from(FreeverbConfig::new().controls(gm()));
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let (n, _) = g.add_with_controls(node);
        g.connect_input(0, n, 0).connect_output(n, 0, 0);
        g.connect_output(n, 1, 1);
        let mut r = g
            .renderer(Prepare::new(SR, MaxBlock::of(Samples::new(100))))
            .expect("the graph builds");
        let at = ParamIn {
            node: n,
            param: UnitAddr::first(UnitParam::RoomSize),
        };
        let program = ControlProgramBuilder::new().build().expect("a program");
        let lane = ControlLaneNode::from(
            UnitLaneConfig::new(UnitParam::RoomSize, program)
                .knob(Arc::new(AtomicF64::new(room)).into()),
        );
        let key = tutti_graph::NodeKey(9_000);
        let editor = r.editor_mut();
        editor.insert(key, "lane", lane);
        editor
            .spec_mut()
            .connect_param(at, tutti_graph::EventOutputPort::new(key, 0));
        editor.commit().expect("the lane commits");
        let frames = 60_000;
        let mut x = noise(8, 4_800);
        x.resize(frames, 0.0);
        let out = r.render_input(Planes::new(&[&x]).unwrap());
        let energy = |from: usize, to: usize| -> f64 {
            out.iter()
                .flat_map(|c| &c[from..to])
                .map(|&s| f64::from(s).powi(2))
                .sum()
        };
        energy(40_000, 50_000) / energy(10_000, 20_000)
    };
    let (small, big) = (ring(0.1), ring(0.9));
    assert!(
        big > 20.0 * small,
        "a room of 0.9 keeps {big} of its energy, a room of 0.1 keeps {small}"
    );
}

/// A `FreeverbNode` is an `IntoNode` whose controls are its
/// `ParamSet`, so the session can address the room by `UnitParam`.
#[test]
fn it_is_inserted_with_a_param_set() {
    let (_, set) = FreeverbNode::<f32>::from(FreeverbConfig::new().controls(gm())).into_node();
    assert!(set.set(UnitParam::RoomSize, 0.7));
    assert_eq!(set.get(UnitParam::RoomSize), Some(0.7));
    assert!(
        !set.set(UnitParam::Wet, 0.25),
        "no mix port: the graph blends"
    );
}

/// A node has no rate until it is prepared: it declares no latency and an unknown tail
/// (nothing is derived from a rate it was not given), and once prepared its tail is the
/// finite figure at the rate it was given, so the same node prepared at two rates
/// declares two tails.
#[test]
fn an_unprepared_node_declares_nothing_derived_from_a_rate() {
    let node = FreeverbNode::<f32>::from(FreeverbConfig::new());
    let shape = node.shape();
    assert_eq!(shape.latency.samples().get(), 0);
    assert_eq!(shape.tail, Tail::Unknown);

    let tail_at = |rate: SampleRate| match prepared(node.clone(), rate, BLOCK).shape().tail {
        Tail::Finite(frames) => frames.get(),
        other => panic!("a fixed network's tail is finite once prepared, got {other:?}"),
    };
    let (low, high) = (tail_at(SampleRate::SR_44K1), tail_at(SampleRate::SR_96K));
    assert!(high > low, "{high} frames at 96 kHz, {low} at 44.1 kHz");
}

/// A cell that moves lands on the stream's 32-frame grid, whatever the blocks: the room and
/// the damping written between two blocks at frame 1024 (a multiple of every block below)
/// give the same render in blocks of 1, 16, 64, 128 and 1024 (the node against itself under
/// cuts), because the reader takes each whole at the grid and the bank's ramps are on the
/// stream's chunks, not a block's. And the move is heard.
#[test]
fn a_moved_cell_lands_on_the_grid_whatever_the_blocks() {
    let x = noise(11, 4_096);
    let render = |block: usize, moved: bool| -> Vec<Vec<f32>> {
        let node = live(gm());
        let set = node.param_set();
        let mut rig = Rig::new(node);
        let mut out = vec![Vec::new(), Vec::new()];
        for (i, chunk) in x.chunks(block).enumerate() {
            if moved && i * block == 1_024 {
                assert!(set.set(UnitParam::RoomSize, 0.9));
                assert!(set.set(UnitParam::Damping, 0.2));
            }
            for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
                all.extend(more);
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

/// A cell that moves **between** grid points starts its ramp at the next one: a room and a
/// damping written 13 frames past frame 1024 and heard from the block that begins there
/// render the same as the same write at frame 1056, the next grid point of 32, and
/// differently from the write at 1088.
#[test]
fn a_move_between_grid_points_starts_at_the_next_one() {
    let x = noise(13, 3_072);
    let render = |at: usize| -> Vec<Vec<f32>> {
        let node = live(gm());
        let set = node.param_set();
        let mut rig = Rig::new(node);
        let mut out = vec![Vec::new(), Vec::new()];
        for (chunk, from) in [(&x[..at], 0), (&x[at..], at)] {
            if from == at {
                assert!(set.set(UnitParam::RoomSize, 0.95));
                assert!(set.set(UnitParam::Damping, 0.05));
            }
            for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
                all.extend(more);
            }
        }
        out
    };
    let at_the_point = render(1_056);
    assert_eq!(render(1_037), at_the_point);
    assert_ne!(render(1_088), at_the_point);
}

/// A reset node equals a fresh one that has the cell: render, move the room and the damping,
/// reset, and the render is the fresh node's with those cells (the move lands at once, it does
/// not ramp from the old room).
#[test]
fn a_cell_moved_before_a_reset_lands_at_once_as_on_a_fresh_node() {
    let x = noise(21, 4_096);
    let moved = |node: &FreeverbNode<f32>| {
        let set = node.param_set();
        assert!(set.set(UnitParam::RoomSize, 0.95));
        assert!(set.set(UnitParam::Damping, 0.05));
    };
    let blocks = |rig: &mut Rig<FreeverbNode<f32>>| -> Vec<Vec<f32>> {
        let mut out = vec![Vec::new(), Vec::new()];
        for chunk in x.chunks(BLOCK) {
            for (all, more) in out.iter_mut().zip(rig.block(chunk, &[]).audio) {
                all.extend(more);
            }
        }
        out
    };
    let mut used = Rig::new(live(gm()));
    blocks(&mut used);
    moved(&used.node);
    used.node.reset();
    let fresh = FreeverbNode::<f32>::from(FreeverbConfig::new().controls(gm()));
    moved(&fresh);
    let mut fresh = Rig::new(prepared(fresh, SR, BLOCK));
    let (a, b) = (blocks(&mut used), blocks(&mut fresh));
    assert!(a.iter().flatten().any(|&s| s != 0.0));
    assert_eq!(a, b);
}
