//! Bypass and mute are graph state: `NodeMode`, set with `Editor::set_mode`, applied by the
//! executor to any node.
//!
//! Every figure is analytic: the steady states are the input (delayed by the
//! declared latency) or silence, bit for bit, and a fade is the formula the
//! doc gives written out here with `FadeLaw::EqualAmplitude`'s gains.
//! A render is compared with another only as one-participant against parallel.

#![allow(
    clippy::needless_range_loop,
    reason = "a frame index reads several outputs and the signal at that frame"
)]

use crate::rendezvous::Rendezvous;
use common::spec_of;
use tutti_buffers::Float;
use tutti_fixtures::{Record, Shared};
use tutti_graph::testing as common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Kind, Pair, RingingNode, SpecBehaviour, TestNode, bits, prepare};
use tutti_buffers::FoldError;
use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology, Speaker};
use tutti_graph::{
    Bus, BusRole, Buses, Cx, Edge, Editor, Event, EventEdge, EventInputPort, EventKind,
    EventOutputPort, Executor, FadeLaw, ForkByClone, ForkTarget, GraphSpec, InputPort, Io,
    ModeError, Node, NodeKey, NodeMode, Offset, OutputPort, PassError, PortSource, Prepare,
    RenderMode, Shape, Status, Topology, TransportPosition, Unforkable,
};
use tutti_ump::Ump;
use tutti_units::{Latency, Samples, Tail};

const BLOCK: usize = 64;
/// The most a block here is: what every `Pair` is prepared for.
const MAX_BLOCK: usize = 128;
/// `DECLICK` at the rig's 48 kHz.
const FADE: usize = 240;

fn key(n: u64) -> NodeKey {
    NodeKey(n)
}

fn at(node: u64, port: u16) -> InputPort {
    InputPort::new(key(node), port)
}

fn out(node: u64, port: u16) -> PortSource {
    PortSource::Node(OutputPort::new(key(node), port))
}

fn direct(node: u64) -> Edge {
    Edge::Direct(out(node, 0))
}

/// `(g_in, g_out)` of frame `k` of a mode fade: the one function the doc names.
fn gains(k: usize) -> (f32, f32) {
    // The law's two gains sum to one: the mode's pair is the incoming `f32` weight and its complement.
    let inn = FadeLaw::EqualAmplitude
        .gains(Samples::new(k), Samples::new(FADE))
        .1
        .get() as f32;
    (inn, 1.0 - inn)
}

/// A `Pair` over `kinds` (keys `1..`), wired by `wire`, with `modes` set
/// before the first block (a unit starts steady in its mode).
struct Rig {
    pair: Pair,
    frame: u64,
}

impl Rig {
    fn new(
        workers: usize,
        kinds: &[Kind],
        inputs: u16,
        wire: impl FnOnce(&mut Topology),
        modes: &[(u64, NodeMode)],
    ) -> Self {
        let mut t = Topology::default();
        let mut map = BTreeMap::new();
        for (i, k) in kinds.iter().enumerate() {
            t.nodes.insert(key(i as u64 + 1), spec_of(k));
            map.insert(key(i as u64 + 1), k.clone());
        }
        t.inputs = ChannelLayout::from_count(inputs);
        wire(&mut t);
        let spec = GraphSpec::new(t);
        let mut pair = Pair::with_workers(MAX_BLOCK, workers);
        for &(n, m) in modes {
            pair.start_mode(key(n), m);
        }
        pair.switch(&spec.validate().expect("valid"), &map);
        Self { pair, frame: 0 }
    }

    fn mode(&mut self, n: u64, mode: NodeMode) {
        self.pair.set_mode(key(n), mode);
    }

    /// Render `blocks` (each a block length) of the signal `x`; the
    /// executor's outputs.
    fn run(&mut self, blocks: &[usize], x: &dyn Fn(u64) -> f32) -> Vec<Vec<f32>> {
        let mut all: Vec<Vec<f32>> = vec![Vec::new(); self.pair.outputs];
        for &n in blocks {
            let input: Vec<f32> = (0..n as u64).map(|i| x(self.frame + i)).collect();
            let (a, _) = self.pair.block(n, &input);
            for (all, a) in all.iter_mut().zip(a) {
                all.extend(a);
            }
            self.frame += n as u64;
        }
        all
    }

    /// `run` over `frames` frames in blocks of `BLOCK`.
    fn frames(&mut self, frames: usize, x: &dyn Fn(u64) -> f32) -> Vec<Vec<f32>> {
        assert_eq!(frames % BLOCK, 0);
        self.run(&vec![BLOCK; frames / BLOCK], x)
    }
}

/// One-participant, and parallel on two and four participants.
const WORKERS: [usize; 3] = [1, 2, 4];

/// A signal that is never zero and never repeats within a test.
fn wave(f: u64) -> f32 {
    ((f % 23) as f32 - 11.0) * 0.0625 + 0.03125 + (f / 23 % 5) as f32 * 0.5
}

/// One chain of `kinds[0]` fed by main input 0, one output.
fn chain(t: &mut Topology) {
    t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
    t.set_outputs(vec![out(1, 0)]);
}

// ---------------------------------------------------------------------------
// Steady states
// ---------------------------------------------------------------------------

/// **Steady `Bypassed`, `L = 0`: the output is the input, bit for bit**,
/// including `-0.0`, a NaN with a payload, a denormal and the infinities: a
/// steady bypass copies, it never multiplies (a multiply by one flushes the
/// denormal). Two nodes side by side (one in place, one not), so the parallel
/// executor has two tasks.
#[test]
fn a_steady_bypass_is_the_input_bit_for_bit() {
    let special = [
        -0.0,
        f32::from_bits(0x7fc0_1234),
        1.0e-40,
        f32::INFINITY,
        -2.5,
        0.0,
        0.1,
        f32::NEG_INFINITY,
    ];
    let x = |f: u64| special[(f % special.len() as u64) as usize];
    for workers in WORKERS {
        for kinds in [
            [
                Kind::Gain {
                    gain: 3.0,
                    width: 1,
                },
                Kind::Gain {
                    gain: 5.0,
                    width: 1,
                },
            ],
            [Kind::Smooth, Kind::ThruInPlace { width: 1 }],
        ] {
            let mut rig = Rig::new(
                workers,
                &kinds,
                1,
                |t| {
                    t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
                    t.connect(at(2, 0), Edge::Direct(PortSource::MainInput(0)));
                    t.set_outputs(vec![out(1, 0), out(2, 0)]);
                },
                &[(1, NodeMode::Bypassed), (2, NodeMode::Bypassed)],
            );
            let got = rig.frames(512, &x);
            let want: Vec<u32> = (0..512).map(|f| x(f).to_bits()).collect();
            assert_eq!(got[0].iter().map(|v| v.to_bits()).collect::<Vec<_>>(), want);
            assert_eq!(got[1].iter().map(|v| v.to_bits()).collect::<Vec<_>>(), want);
        }
    }
}

/// **Steady `Bypassed`, `L > 0`: the input delayed by exactly the declared
/// latency**, the first `L` frames what the line held (`+0.0`, silence at
/// start). The node is fed by a `Lag` of 100, so its **arrival** is 100 and
/// its declared latency 480: the delay is `100 + 480`, neither the arrival
/// nor the compensated figure alone (the PDC the plan holds has the node's
/// output at `total latency`, which here is its own departure).
#[test]
fn a_latent_bypass_is_the_input_delayed_by_the_declared_latency() {
    for workers in WORKERS {
        for blocks in [vec![BLOCK; 24], vec![100; 16], vec![7; 220]] {
            let mut rig = Rig::new(
                workers,
                &[
                    Kind::Lag { latency: 100 },
                    Kind::Spec {
                        behaviour: SpecBehaviour::Gain(3.0),
                        ins: 1,
                        outs: 1,
                        latency: 480,
                        tail: Tail::None,
                    },
                ],
                1,
                |t| {
                    t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
                    t.connect(at(2, 0), direct(1));
                    t.set_outputs(vec![out(2, 0)]);
                },
                &[(2, NodeMode::Bypassed)],
            );
            let got = rig.run(&blocks, &wave);
            for (n, &y) in got[0].iter().enumerate() {
                let want = if n >= 580 { wave(n as u64 - 580) } else { 0.0 };
                assert_eq!(y.to_bits(), want.to_bits(), "frame {n}");
            }
            assert!(got[0].len() >= 1000);
        }
    }
}

/// A node with `L > 0` fed a click and bypassed passes the click `L` frames
/// late, and the frames before it are silent.
#[test]
fn a_click_passes_l_frames_late() {
    let click = |f: u64| if f == 100 { 1.0 } else { 0.0 };
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[Kind::Spec {
                behaviour: SpecBehaviour::Gain(2.0),
                ins: 1,
                outs: 1,
                latency: 48,
                tail: Tail::None,
            }],
            1,
            chain,
            &[(1, NodeMode::Bypassed)],
        );
        let got = rig.frames(384, &click);
        for (n, &y) in got[0].iter().enumerate() {
            let want = if n == 148 { 1.0f32 } else { 0.0 };
            assert_eq!(y.to_bits(), want.to_bits(), "frame {n}");
        }
    }
}

/// **A latent node's bypass keeps the PDC alignment**: a pure delay of 480
/// declared as its latency, bypassed, lines up with the same node active (the
/// two renders are bit-identical), and with a dry path next to it the plan
/// aligned by 480 (the outputs are one signal).
#[test]
fn a_bypassed_latent_node_stays_aligned_with_an_active_one() {
    let kinds = [
        Kind::Lag { latency: 480 },
        Kind::Gain {
            gain: 1.0,
            width: 1,
        },
    ];
    let wire = |t: &mut Topology| {
        t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
        t.connect(at(2, 0), Edge::Direct(PortSource::MainInput(0)));
        t.set_outputs(vec![out(1, 0), out(2, 0)]);
    };
    for workers in WORKERS {
        let active = Rig::new(workers, &kinds, 1, wire, &[]).frames_owned(1280, &wave);
        let bypassed = Rig::new(workers, &kinds, 1, wire, &[(1, NodeMode::Bypassed)])
            .frames_owned(1280, &wave);
        assert_eq!(bits(&active), bits(&bypassed));
        assert_eq!(bits(&bypassed[..1]), bits(&bypassed[1..]));
        assert!(bypassed[0][480..].iter().any(|&x| x != 0.0));
    }
}

impl Rig {
    fn frames_owned(mut self, frames: usize, x: &dyn Fn(u64) -> f32) -> Vec<Vec<f32>> {
        self.frames(frames, x)
    }
}

/// **The channel mapping: by index at one width, by speaker across widths.**
/// A node built from widths names no placement, so its bypass passes output
/// `c` = input `c` only where the widths agree (a pair of different widths is
/// refused, `a_bypass_with_no_pass_is_refused`). A mono centre under a stereo
/// output is heard on both sides at gain 1, not on the left alone: the fold
/// between the main buses (the general case is
/// `a_bypass_is_the_fold_between_the_main_buses`).
#[test]
fn a_bypass_is_by_index_at_one_width_and_by_speaker_across() {
    let spec = |ins, outs, latency| Kind::Spec {
        behaviour: SpecBehaviour::Fan,
        ins,
        outs,
        latency,
        tail: Tail::None,
    };
    for workers in WORKERS {
        for latency in [0usize, 40] {
            let delayed = |f: u64| {
                if f >= latency as u64 {
                    wave(f - latency as u64)
                } else {
                    0.0
                }
            };
            // A mono centre into a stereo pair: both sides, at gain 1.
            let mut rig = Rig::new(
                workers,
                &[bused(1, 2, latency, smpte(1), smpte(2))],
                1,
                |t| {
                    t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
                    t.set_outputs(vec![out(1, 0), out(1, 1)]);
                },
                &[(1, NodeMode::Bypassed)],
            );
            let got = rig.frames(256, &wave);
            for n in 0..256 {
                let want = delayed(n as u64).to_bits();
                assert_eq!(got[0][n].to_bits(), want, "left, {n}");
                assert_eq!(got[1][n].to_bits(), want, "right, {n}");
            }
            // stereo -> stereo, from widths: channel `c` is input `c` (main
            // input `c` is the signal scaled by 2^-c).
            let mut rig = Rig::new(
                workers,
                &[spec(2, 2, latency)],
                2,
                |t| {
                    t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
                    t.connect(at(1, 1), Edge::Direct(PortSource::MainInput(1)));
                    t.set_outputs(vec![out(1, 0), out(1, 1)]);
                },
                &[(1, NodeMode::Bypassed)],
            );
            let got = rig.frames(256, &wave);
            for n in 0..256 {
                assert_eq!(got[0][n].to_bits(), delayed(n as u64).to_bits());
                assert_eq!(got[1][n].to_bits(), (0.5 * delayed(n as u64)).to_bits());
            }
        }
    }
}

/// A node of `ins` channels in, `outs` out, declaring `input` and `output` as
/// its `Main` buses (the node itself is `Fan`, with a declared `latency`).
fn bused(ins: usize, outs: usize, latency: usize, input: Bus, output: Bus) -> Kind {
    Kind::Bused {
        inner: Box::new(Kind::Spec {
            behaviour: SpecBehaviour::Fan,
            ins,
            outs,
            latency,
            tail: Tail::None,
        }),
        buses_in: Box::new(Buses::new(&[input]).expect("one bus")),
        buses_out: Box::new(Buses::new(&[output]).expect("one bus")),
    }
}

fn main_bus(topology: ChannelTopology) -> Bus {
    Bus::new(
        BusTopology::try_from(&topology).expect("fits a bus"),
        BusRole::Main,
    )
}

fn smpte(n: u16) -> Bus {
    main_bus(ChannelTopology::default_for(ChannelLayout::from_count(n)).unwrap())
}

fn unassigned(n: u16) -> Bus {
    main_bus(ChannelTopology::unassigned(ChannelLayout::from_count(n)).unwrap())
}

/// **A bypass is the fold between the main buses.** A node whose `Main` input
/// bus is named 5.1 and whose `Main` output bus is named stereo, bypassed,
/// writes the ITU downmix of its input (delayed by the declared latency), not
/// its first two channels: `L = FL + 3dB*FC + 3dB*SL`, `R = FR + 3dB*FC +
/// 3dB*SR`, the LFE dropped. Main input `c` is the signal scaled by `2^-c`,
/// so every channel is distinct. (Those are `Fold::contributions`' rows for
/// the pair, written out; a rounding of the sum differs, so the comparison is
/// to `1e-5`.)
///
/// The same holds for a node that runs at `Mix::DRY` (all of the aligned
/// input), which reads the fold through the blend and not the parked path.
#[test]
fn a_bypass_is_the_fold_between_the_main_buses() {
    for workers in WORKERS {
        for (latency, parked) in [(0usize, true), (40, true), (0, false), (40, false)] {
            // Parked (Bypassed) and running at `Mix::DRY`: the node's own
            // output is Fan's, and not what is heard.
            let modes = [(1, NodeMode::Bypassed)];
            let mut rig = Rig::new(
                workers,
                &[bused(6, 2, latency, smpte(6), smpte(2))],
                6,
                |t| {
                    for c in 0..6 {
                        t.connect(at(1, c), Edge::Direct(PortSource::MainInput(c)));
                    }
                    t.set_outputs(vec![out(1, 0), out(1, 1)]);
                },
                if parked { &modes } else { &[] },
            );
            if !parked {
                rig.pair.set_mix(key(1), tutti_units::Mix::DRY);
            }
            let got = rig.frames(256, &wave);
            let input = |c: u32, f: u64| {
                if f >= latency as u64 {
                    wave(f - latency as u64) * 0.5f32.powi(c as i32)
                } else {
                    0.0
                }
            };
            // A mix that moves ramps across the block it lands on: from the
            // second block, it holds still at DRY.
            let from = if parked { 0 } else { BLOCK as u64 };
            for n in from..256u64 {
                let left =
                    input(0, n) + f32::MINUS_3_DB * input(2, n) + f32::MINUS_3_DB * input(4, n);
                let right =
                    input(1, n) + f32::MINUS_3_DB * input(2, n) + f32::MINUS_3_DB * input(5, n);
                let i = n as usize;
                assert!((got[0][i] - left).abs() < 1e-5, "left, {n}: {}", got[0][i]);
                assert!(
                    (got[1][i] - right).abs() < 1e-5,
                    "right, {n}: {}",
                    got[1][i]
                );
            }
        }
    }
}

/// **A bypass of a node with no pass is refused**, with the reason, and nothing
/// changes: a stereo pair into a centre and an LFE has no place for the left
/// (`PassError::Fold`); six channels into two built from widths name no
/// placement, and six unplaced channels into a named stereo pair say nothing of
/// where each goes (`PassError::Unplaced`: by index, the first two inputs would
/// be a guess). Any other mode, and a bypass at one width, is not refused.
#[test]
fn a_bypass_with_no_pass_is_refused() {
    let (mut ed, _exec) = Editor::new(prepare(MAX_BLOCK));
    let widths = |ins, outs| {
        Unforkable(TestNode::new(Kind::Spec {
            behaviour: SpecBehaviour::Fan,
            ins,
            outs,
            latency: 0,
            tail: Tail::None,
        }))
    };
    let named = |s: &[Speaker]| {
        Bus::new(
            BusTopology::new(s.iter().copied()).expect("a bus"),
            BusRole::Main,
        )
    };
    let stereo = [Speaker::FrontLeft, Speaker::FrontRight];
    let centre_lfe = [Speaker::FrontCenter, Speaker::LowFrequency];
    ed.insert(
        key(1),
        "no fold",
        Unforkable(TestNode::new(bused(
            2,
            2,
            0,
            named(&stereo),
            named(&centre_lfe),
        ))),
    );
    ed.insert(key(2), "widths", widths(6, 2));
    ed.insert(
        key(3),
        "unplaced into named",
        Unforkable(TestNode::new(bused(6, 2, 0, unassigned(6), smpte(2)))),
    );
    ed.insert(key(4), "one width", widths(2, 2));
    let refused = ed.set_mode(key(1), NodeMode::Bypassed);
    assert!(
        matches!(
            refused,
            Err(ModeError::Unplaced {
                error: PassError::Fold(FoldError::Unplaced { .. }),
                ..
            })
        ),
        "{refused:?}"
    );
    for k in [2, 3] {
        assert_eq!(
            ed.set_mode(key(k), NodeMode::Bypassed),
            Err(ModeError::Unplaced {
                node: key(k),
                error: PassError::Unplaced {
                    inputs: 6,
                    outputs: 2
                },
            })
        );
        assert_eq!(ed.mode(key(k)), NodeMode::Active, "nothing changed");
    }
    assert_eq!(ed.mode(key(1)), NodeMode::Active, "nothing changed");
    assert_eq!(ed.set_mode(key(1), NodeMode::Muted), Ok(()));
    assert_eq!(ed.set_mode(key(4), NodeMode::Bypassed), Ok(()));
}

/// **`Muted`: every output is `+0.0`** (the bit pattern of positive zero),
/// whatever the node would have made.
#[test]
fn a_muted_node_is_positive_zero() {
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[
                Kind::Gain {
                    gain: -3.0,
                    width: 1,
                },
                Kind::Const {
                    value: 0.5,
                    width: 2,
                },
            ],
            1,
            |t| {
                t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
                t.set_outputs(vec![out(1, 0), out(2, 0), out(2, 1)]);
            },
            &[(1, NodeMode::Muted), (2, NodeMode::Muted)],
        );
        let got = rig.frames(256, &wave);
        for ch in &got {
            assert!(ch.iter().all(|x| x.to_bits() == 0), "{:?}", &ch[..4]);
        }
    }
}

// ---------------------------------------------------------------------------
// The declick
// ---------------------------------------------------------------------------

/// **The `DECLICK` fade follows `FadeLaw::EqualAmplitude`'s gains
/// exactly**, in both directions and into and out of mute. The node is an
/// in-place gain of 3 on a constant input of 1, so `y = 3`, `x = 1`:
///
/// | toggle | output at fade frame `k`, then after |
/// |---|---|
/// | Active → Bypassed | `y * g_out + x * g_in`, then `x` |
/// | Bypassed → Active | `y * g_in + x * g_out`, then `y` |
/// | Active → Muted | `y * g_out`, then `0` |
/// | Muted → Active | `y * g_in`, then `y` |
/// | Bypassed → Muted | `x * g_out`, then `0` |
///
/// A block is 64 frames, so the fade is four blocks; the toggle lands on the
/// first frame of a block.
#[test]
fn the_declick_follows_the_equal_amplitude_gains() {
    let one = |_: u64| 1.0f32;
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[Kind::Gain {
                gain: 3.0,
                width: 1,
            }],
            1,
            chain,
            &[],
        );
        let (y, x) = (3.0f32, 1.0f32);
        let steady = rig.frames(128, &one);
        assert!(steady[0].iter().all(|&v| v == y));
        let check = |rig: &mut Rig, to: NodeMode, fade: &dyn Fn(f32, f32) -> f32, then: f32| {
            rig.mode(1, to);
            let got = rig.frames(512, &one);
            for k in 0..FADE {
                let (g_in, g_out) = gains(k);
                assert_eq!(
                    got[0][k].to_bits(),
                    fade(g_in, g_out).to_bits(),
                    "{to:?} fade frame {k}"
                );
            }
            for (n, v) in got[0].iter().enumerate().skip(FADE) {
                assert_eq!(
                    v.to_bits(),
                    then.to_bits(),
                    "{to:?} after the fade, frame {n}"
                );
            }
        };
        check(&mut rig, NodeMode::Bypassed, &|i, o| y * o + x * i, x);
        check(&mut rig, NodeMode::Active, &|i, o| y * i + x * o, y);
        check(&mut rig, NodeMode::Muted, &|_, o| y * o, 0.0);
        check(&mut rig, NodeMode::Active, &|i, _| y * i, y);
        check(&mut rig, NodeMode::Bypassed, &|i, o| y * o + x * i, x);
        check(&mut rig, NodeMode::Muted, &|_, o| x * o, 0.0);
    }
}

/// **A toggle during a fade restarts from the current gain: no jump.** Fade
/// to Bypassed for 100 frames, then back to Active: the first frame of the
/// second fade is the previous frame plus one fade step, and the whole of it
/// is the doc's formula from the weights the first fade had reached:
/// `w = w0 + (w1 - w0) * g(k)`.
#[test]
fn a_toggle_mid_fade_does_not_jump() {
    let one = |_: u64| 1.0f32;
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[Kind::Gain {
                gain: 3.0,
                width: 1,
            }],
            1,
            chain,
            &[],
        );
        rig.mode(1, NodeMode::Bypassed);
        let first = rig.run(&[100], &one);
        let (g, _) = gains(99);
        // The weights the first fade reached, as the doc's formula gives them.
        let (wy0, wx0) = (1.0f32 + (0.0 - 1.0) * g, 0.0f32 + (1.0 - 0.0) * g);
        assert_eq!(first[0][99].to_bits(), (3.0 * wy0 + 1.0 * wx0).to_bits());
        rig.mode(1, NodeMode::Active);
        let second = rig.run(&[128, 128], &one);
        for k in 0..FADE {
            let (g, _) = gains(k);
            let (wy, wx) = (wy0 + (1.0 - wy0) * g, wx0 + (0.0 - wx0) * g);
            assert_eq!(
                second[0][k].to_bits(),
                (3.0 * wy + 1.0 * wx).to_bits(),
                "frame {k}"
            );
        }
        // One fade step: the first frame is within a step of the last.
        let step = (second[0][0] - first[0][99]).abs();
        assert!(step < 1e-5, "jumped by {step}");
        assert!(second[0][FADE..].iter().all(|&v| v == 3.0));
    }
}

// ---------------------------------------------------------------------------
// Un-bypass
// ---------------------------------------------------------------------------

/// **The line holds the history when the toggle comes**: a latent node active
/// for a while and then bypassed fades from its output to the input delayed by
/// `L`, where the input is the one the node had been sent *before* the toggle
/// (the line is fed every block, not only once bypassed). The node is a gain
/// of 2 declaring 100 frames: `y * g_out + x̂ * g_in`, `x̂` of 100 frames ago.
#[test]
fn the_line_is_fed_while_the_node_is_active() {
    const L: usize = 100;
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[Kind::Spec {
                behaviour: SpecBehaviour::Gain(2.0),
                ins: 1,
                outs: 1,
                latency: L,
                tail: Tail::None,
            }],
            1,
            chain,
            &[],
        );
        let active = rig.frames(256, &wave);
        assert!(
            active[0]
                .iter()
                .enumerate()
                .all(|(n, &v)| v == wave(n as u64) * 2.0)
        );
        rig.mode(1, NodeMode::Bypassed);
        let got = rig.frames(512, &wave);
        for (i, &v) in got[0].iter().enumerate() {
            let n = 256 + i as u64;
            let x_hat = wave(n - L as u64);
            let want = if i < FADE {
                let (g_in, g_out) = gains(i);
                (wave(n) * 2.0) * g_out + x_hat * g_in
            } else {
                x_hat
            };
            assert_eq!(v.to_bits(), want.to_bits(), "frame {n} (toggle + {i})");
        }
    }
}

/// A bypassed node's output carries its input's flags: a loud input is not
/// flagged silent (a downstream node must not skip it), a silent one is, and
/// through a bypass line the output is silent only once the line has run dry.
#[test]
fn a_bypassed_output_carries_its_inputs_flags() {
    for (latency, want) in [
        (0, [false, false, false, true, true, true, true, true]),
        (40, [false, false, false, false, true, true, true, true]),
    ] {
        let q = Probe::new(1.0);
        let mut p = Probe::new(3.0);
        p.latency = latency;
        let mut s = Solo::new(0);
        s.add(1, common::Burst::new(3 * BLOCK));
        s.add(2, p);
        s.add(3, q.clone());
        s.wire(|t| {
            t.connect(at(2, 0), direct(1));
            t.connect(at(3, 0), direct(2));
            t.set_outputs(vec![out(3, 0)]);
        });
        s.mode(2, NodeMode::Bypassed);
        s.commit();
        s.run(8, BLOCK, &|_| 0.0);
        let silent = q.silent.lock().unwrap().to_vec();
        assert_eq!(silent, want, "latency {latency}");
    }
}

/// **Un-bypass is reset, warm up for `L`, fade in**: leaving Bypassed, the
/// aligned input (the input delayed by `L`, the line having been fed all the
/// while) stays audible for `L` frames with the node's output discarded, and
/// then the `DECLICK` fade runs from the aligned input to the output:
/// `y * g_in + x̂ * g_out`. The node is a gain of 2 declaring 100 frames it
/// does not realise, so `y` is `2x` now and `x̂` is `x` of 100 frames ago.
#[test]
fn un_bypass_warms_up_for_the_latency_then_fades_in() {
    const L: usize = 100;
    for workers in WORKERS {
        let mut rig = Rig::new(
            workers,
            &[Kind::Spec {
                behaviour: SpecBehaviour::Gain(2.0),
                ins: 1,
                outs: 1,
                latency: L,
                tail: Tail::None,
            }],
            1,
            chain,
            &[(1, NodeMode::Bypassed)],
        );
        let parked = rig.frames(256, &wave);
        for (n, &y) in parked[0].iter().enumerate() {
            let want = if n >= L { wave((n - L) as u64) } else { 0.0 };
            assert_eq!(y.to_bits(), want.to_bits(), "bypassed, frame {n}");
        }
        rig.mode(1, NodeMode::Active);
        let got = rig.frames(640, &wave);
        for (i, &v) in got[0].iter().enumerate() {
            let n = 256 + i as u64;
            let x_hat = wave(n - L as u64);
            let y = wave(n) * 2.0;
            let want = match i {
                i if i < L => x_hat,
                i if i < L + FADE => {
                    let (g_in, g_out) = gains(i - L);
                    y * g_in + x_hat * g_out
                }
                _ => y,
            };
            assert_eq!(v.to_bits(), want.to_bits(), "frame {n} (toggle + {i})");
        }
    }
}

// ---------------------------------------------------------------------------
// A node that counts, remembers and records
// ---------------------------------------------------------------------------

/// A test node with a width, a declared latency and optional event inputs,
/// that counts its calls and resets, records what it saw, and (`integrate`)
/// holds state that only a reset clears: `y = gain * x + state`, `state = y`.
#[derive(Clone)]
struct Probe {
    ins: usize,
    outs: usize,
    latency: usize,
    gain: f32,
    integrate: bool,
    /// Answers `Status::Idle` ("park me until input") when it was sent no
    /// event: the node the silence skip trusts.
    idle: bool,
    /// Declares `Shape::own_bypass`.
    own_bypass: bool,
    events_in: u16,
    state: f32,
    calls: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
    /// Per call, whether input channel 0 was flagged silent.
    silent: Record<bool>,
    /// Every event the node was sent, as `(frame, words)`.
    events: Record<(u64, [u32; 4])>,
}

impl Probe {
    fn new(gain: f32) -> Self {
        Self {
            ins: 1,
            outs: 1,
            latency: 0,
            gain,
            integrate: false,
            idle: false,
            own_bypass: false,
            events_in: 0,
            state: 0.0,
            calls: Arc::new(AtomicUsize::new(0)),
            resets: Arc::new(AtomicUsize::new(0)),
            silent: Record::new(),
            events: Record::new(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn resets(&self) -> usize {
        self.resets.load(Ordering::Relaxed)
    }
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        let ch = |n| ChannelLayout::try_from(n).expect("a width");
        let shape = Shape::buses(
            tutti_graph::Buses::unassigned(ch(self.ins)),
            tutti_graph::Buses::unassigned(ch(self.outs)),
        )
        .latency(Latency::new(Samples::new(self.latency)))
        .events(self.events_in, 0)
        .tail(Tail::Unbounded);
        if self.own_bypass {
            shape.own_bypass()
        } else {
            shape
        }
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.ins > 0 {
            self.silent.lock().unwrap().push(io.silent().get(0));
        }
        let mut sent = false;
        for port in self.shape().event_inputs() {
            for e in io.events(port) {
                sent = true;
                if let EventKind::Midi(Ump(w)) = e.kind {
                    self.events
                        .lock()
                        .unwrap()
                        .push((cx.env.frame_at(e.offset).get(), w));
                }
            }
        }
        let n = io.frames();
        let (ins, mut outs) = io.split();
        for c in 0..self.outs {
            for i in 0..n {
                let x = if c < self.ins { ins.get(c)[i] } else { 0.0 };
                let mut y = self.gain * x;
                if self.integrate {
                    y += self.state;
                    self.state = y;
                }
                outs.get(c)[i] = y;
            }
        }
        if self.idle && !sent {
            Status::Idle
        } else {
            Status::Modified
        }
    }
    fn reset(&mut self) {
        self.resets.fetch_add(1, Ordering::Relaxed);
        self.state = 0.0;
    }
}

/// An editor and executor over a graph of probes.
struct Solo {
    ed: Editor,
    exec: Executor,
    frame: u64,
}

impl Solo {
    fn new(inputs: u16) -> Self {
        let (mut ed, exec) = Editor::new(prepare(MAX_BLOCK));
        ed.spec_mut().topology.inputs = ChannelLayout::from_count(inputs);
        Self { ed, exec, frame: 0 }
    }

    fn add(&mut self, n: u64, node: impl Node + 'static) {
        self.ed.insert(key(n), "probe", Unforkable(node));
    }

    fn wire(&mut self, f: impl FnOnce(&mut Topology)) {
        f(&mut self.ed.spec_mut().topology);
    }

    fn commit(&mut self) {
        self.ed.commit().expect("commits");
        self.exec.apply_pending();
        self.ed.collect();
    }

    fn mode(&mut self, n: u64, mode: NodeMode) {
        self.ed.set_mode(key(n), mode).expect("a node");
    }

    /// `blocks` blocks of `len` frames of the constant `v`, one output.
    fn run(&mut self, blocks: usize, len: usize, x: &dyn Fn(u64) -> f32) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..blocks {
            let input: Vec<f32> = (0..len as u64).map(|i| x(self.frame + i)).collect();
            let mut o = vec![0.0f32; len];
            self.exec.process(
                len,
                &TransportPosition::default(),
                &[&input],
                &mut [&mut o[..]],
            );
            all.extend(o);
            self.frame += len as u64;
        }
        all
    }
}

/// `MainInput(0) → probe 1 → output`, and optionally `probe 1 → probe 2`.
fn probe_chain(first: Probe, second: Option<Probe>) -> Solo {
    let mut s = Solo::new(1);
    s.add(1, first);
    s.wire(|t| t.connect(at(1, 0), Edge::Direct(PortSource::MainInput(0))));
    match second {
        Some(q) => {
            s.add(2, q);
            s.wire(|t| {
                t.connect(at(2, 0), direct(1));
                t.set_outputs(vec![out(2, 0)]);
            });
        }
        None => s.wire(|t| t.set_outputs(vec![out(1, 0)])),
    }
    s.commit();
    s
}

/// **`Muted`: the node keeps running** (called every block, never reset), its
/// output is `+0.0` flagged silent (a downstream node sees the flag), and
/// **un-mute needs no reset and no warm-up**: the very first frame back is
/// `y * g_in(0)`.
#[test]
fn a_muted_node_keeps_running_and_un_muting_needs_nothing() {
    let (p, q) = (Probe::new(3.0), Probe::new(1.0));
    let mut s = probe_chain(p.clone(), Some(q.clone()));
    let half = |_: u64| 0.5f32;
    s.run(10, BLOCK, &half);
    assert_eq!((p.calls(), p.resets()), (10, 0));
    s.mode(1, NodeMode::Muted);
    let muted = s.run(20, BLOCK, &half);
    assert_eq!(p.calls(), 30, "called every block while muted");
    assert_eq!(p.resets(), 0);
    for (k, &v) in muted.iter().enumerate().take(FADE) {
        let (_, g_out) = gains(k);
        assert_eq!(v.to_bits(), (1.5 * g_out).to_bits(), "fade frame {k}");
    }
    assert!(muted[FADE..].iter().all(|v| v.to_bits() == 0));
    // The downstream node was told the input is silent, from the first
    // steady block (after the 4 blocks of fade) on.
    let silent = q.silent.lock().unwrap().to_vec();
    assert!(
        silent[10..14].iter().all(|&b| !b),
        "not silent while fading"
    );
    assert!(silent[14..].iter().all(|&b| b), "silent once muted");
    s.mode(1, NodeMode::Active);
    let back = s.run(8, BLOCK, &half);
    assert_eq!(p.resets(), 0, "un-mute resets nothing");
    for (k, &v) in back.iter().enumerate().take(FADE) {
        let (g_in, _) = gains(k);
        assert_eq!(v.to_bits(), (1.5 * g_in).to_bits(), "first fade frame {k}");
    }
    assert!(back[FADE..].iter().all(|&v| v == 1.5));
}

/// Un-mute has no warm-up even for a node with latency: the fade is the
/// first thing heard, whereas un-bypass keeps the aligned input for `L`.
#[test]
fn un_mute_has_no_warm_up() {
    let mut p = Probe::new(2.0);
    p.latency = 480;
    let mut s = probe_chain(p.clone(), None);
    s.mode(1, NodeMode::Muted);
    s.commit();
    s.run(4, BLOCK, &|_| 0.25);
    s.mode(1, NodeMode::Active);
    let back = s.run(5, BLOCK, &|_| 0.25);
    for (k, &v) in back.iter().enumerate().take(FADE) {
        let (g_in, _) = gains(k);
        assert_eq!(v.to_bits(), (0.5 * g_in).to_bits(), "frame {k}");
    }
    assert_eq!(p.resets(), 0);
}

/// **Parking**: a bypassed node is not called after the fade — the fade of 256
/// frames is 4 blocks of 64 (or 3 of 100), counted — and a silent input leaves
/// its output flagged silent, so a downstream node that parks on silence
/// parks. A node with `L > 0` is parked too.
#[test]
fn a_bypassed_node_is_not_called_after_the_fade() {
    for (len, fade_blocks, latency) in [
        (64usize, 4usize, 0usize),
        (100, 3, 0),
        (64, 4, 40),
        (100, 3, 150),
    ] {
        let mut p = Probe::new(3.0);
        p.latency = latency;
        let ringer = RingingNode::new(Tail::Finite(Samples::new(1)), 1, 1);
        let ring_calls = Arc::clone(&ringer.calls);
        let mut s = Solo::new(0);
        // A source that is loud for 5 blocks and then says it is silent.
        s.add(1, common::Burst::new(5 * len));
        s.add(2, p.clone());
        s.add(3, ringer);
        s.wire(|t| {
            t.connect(at(2, 0), direct(1));
            t.connect(at(3, 0), direct(2));
            t.set_outputs(vec![out(3, 0)]);
        });
        s.commit();
        s.run(5, len, &|_| 0.0);
        assert_eq!(p.calls(), 5);
        s.mode(2, NodeMode::Bypassed);
        s.run(fade_blocks, len, &|_| 0.0);
        assert_eq!(p.calls(), 5 + fade_blocks, "called through the fade");
        // The reverb sees one parked block of silence, and parks after the
        // call that leaves its own output silent.
        s.run(2, len, &|_| 0.0);
        let before = ring_calls.load(Ordering::Relaxed);
        s.run(12, len, &|_| 0.0);
        assert_eq!(p.calls(), 5 + fade_blocks, "not called once parked");
        assert_eq!(
            ring_calls.load(Ordering::Relaxed),
            before,
            "a silent input left the output silent, and the reverb parked"
        );
    }
}

/// **Un-bypass resets the node once, before its first call, and its old
/// state is never heard**: an integrator (it holds what it has summed until
/// reset) is fed a signal, bypassed, parked, then un-bypassed on silence:
/// every frame after is `+0.0`, and `reset` ran exactly once.
#[test]
fn un_bypass_resets_once_and_old_state_is_never_heard() {
    for workers_len in [64usize, 100] {
        let mut p = Probe::new(1.0);
        p.integrate = true;
        let mut s = probe_chain(p.clone(), None);
        let loud = s.run(10, workers_len, &|_| 0.01);
        assert!(loud.last().expect("rendered") > &0.5, "it accumulated");
        s.mode(1, NodeMode::Bypassed);
        s.run(8, workers_len, &|_| 0.0);
        assert_eq!(p.resets(), 0, "bypass-on resets nothing");
        s.mode(1, NodeMode::Active);
        let back = s.run(10, workers_len, &|_| 0.0);
        assert_eq!(p.resets(), 1);
        assert!(back.iter().all(|&v| v == 0.0), "no stale state is heard");
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

const ON60: [u32; 4] = [0x2090_3C64, 0, 0, 0];
const ON62: [u32; 4] = [0x2090_3E64, 0, 0, 0];
const OFF60: [u32; 4] = [0x2080_3C00, 0, 0, 0];

/// A note source: sends what its mailbox holds, as one block's events.
struct Src {
    mail: Shared<Vec<[u32; 4]>>,
    calls: Arc<AtomicUsize>,
}

impl Src {
    const SHAPE: Shape = Shape::EMPTY.events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.calls.fetch_add(1, Ordering::Relaxed);
        for w in self.mail.lock().unwrap().drain(..) {
            let _ = io
                .event_out(Self::OUT)
                .push(Event::midi(Offset::ZERO, Ump(w)));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn ev_out(n: u64) -> EventOutputPort {
    EventOutputPort::new(key(n), 0)
}

fn ev_in(n: u64) -> EventInputPort {
    EventInputPort::new(key(n), 0)
}

/// **A held note on a bypassed node's event input is closed**: the executor
/// sends the node the off during the fade, and later events are discarded. A
/// note-on after the toggle never reaches it; the node is not called once the
/// fade is over and nothing is held.
#[test]
fn a_held_input_note_is_closed_on_bypass() {
    let mail = Shared::new(Vec::new());
    let mut s = Solo::new(1);
    let mut p = Probe::new(3.0);
    p.events_in = 1;
    s.add(
        1,
        Src {
            mail: mail.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        },
    );
    s.add(2, p.clone());
    s.wire(|t| {
        t.connect(at(2, 0), Edge::Direct(PortSource::MainInput(0)));
        t.set_outputs(vec![out(2, 0)]);
    });
    s.ed.spec_mut()
        .events
        .insert(ev_in(2), vec![EventEdge::Direct(ev_out(1))]);
    s.commit();
    mail.lock().unwrap().push(ON60);
    s.run(2, BLOCK, &|_| 0.5);
    s.mode(2, NodeMode::Bypassed);
    mail.lock().unwrap().push(ON62);
    s.run(8, BLOCK, &|_| 0.5);
    let seen: Vec<[u32; 4]> = p.events.lock().unwrap().iter().map(|e| e.1).collect();
    assert_eq!(
        seen,
        vec![ON60, OFF60],
        "the off, and not the later note-on"
    );
    // Called for the 2 blocks, and the 4 of the fade; the off was delivered
    // in the first of those.
    assert_eq!(p.calls(), 2 + 4);
}

/// The close reaches a node the silence skip would otherwise leave alone: one
/// that answers `Status::Idle` ("park me until input"), on a silent input, is
/// skipped by the executor — and a bypass sends it the off all the same.
#[test]
fn a_held_input_note_is_closed_on_an_idle_node() {
    let mail = Shared::new(Vec::new());
    let mut s = Solo::new(0);
    let mut p = Probe::new(3.0);
    p.events_in = 1;
    p.idle = true;
    s.add(
        1,
        Src {
            mail: mail.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        },
    );
    s.add(2, common::Burst::new(BLOCK));
    s.add(3, p.clone());
    s.wire(|t| {
        t.connect(at(3, 0), direct(2));
        t.set_outputs(vec![out(3, 0)]);
    });
    s.ed.spec_mut()
        .events
        .insert(ev_in(3), vec![EventEdge::Direct(ev_out(1))]);
    s.commit();
    mail.lock().unwrap().push(ON60);
    s.run(5, BLOCK, &|_| 0.0);
    let idle_calls = p.calls();
    assert!(
        idle_calls < 5,
        "the idle node is skipped: {idle_calls} calls"
    );
    s.mode(3, NodeMode::Bypassed);
    s.run(8, BLOCK, &|_| 0.0);
    let seen: Vec<[u32; 4]> = p.events.lock().unwrap().iter().map(|e| e.1).collect();
    assert_eq!(seen, vec![ON60, OFF60]);
}

/// **A bypassed source closes what it holds downstream**: a note source
/// bypassed and parked has the off of its held note written to its event
/// output in the first parked block, and nothing after.
#[test]
fn a_bypassed_source_closes_its_notes() {
    let mail = Shared::new(Vec::new());
    let src_calls = Arc::new(AtomicUsize::new(0));
    let mut s = Solo::new(1);
    let mut sink = Probe::new(1.0);
    sink.events_in = 1;
    s.add(
        1,
        Src {
            mail: mail.clone(),
            calls: Arc::clone(&src_calls),
        },
    );
    s.add(2, sink.clone());
    s.wire(|t| {
        t.connect(at(2, 0), Edge::Direct(PortSource::MainInput(0)));
        t.set_outputs(vec![out(2, 0)]);
    });
    s.ed.spec_mut()
        .events
        .insert(ev_in(2), vec![EventEdge::Direct(ev_out(1))]);
    s.commit();
    mail.lock().unwrap().push(ON60);
    s.run(2, BLOCK, &|_| 0.5);
    s.mode(1, NodeMode::Bypassed);
    // The fade: 4 blocks, in which the source is still called.
    s.run(4, BLOCK, &|_| 0.5);
    assert_eq!(src_calls.load(Ordering::Relaxed), 6);
    assert_eq!(
        sink.events.lock().unwrap().len(),
        1,
        "only the note-on so far"
    );
    s.run(1, BLOCK, &|_| 0.5);
    let seen: Vec<(u64, [u32; 4])> = sink.events.lock().unwrap().to_vec();
    assert_eq!(
        seen.iter().map(|e| e.1).collect::<Vec<_>>(),
        vec![ON60, OFF60]
    );
    assert_eq!(seen[1].0, 6 * BLOCK as u64, "in the first parked block");
    mail.lock().unwrap().push(ON62);
    s.run(4, BLOCK, &|_| 0.5);
    assert_eq!(src_calls.load(Ordering::Relaxed), 6, "parked: not called");
    assert_eq!(sink.events.lock().unwrap().len(), 2, "and says nothing");
}

// ---------------------------------------------------------------------------
// Determinism, the plan, forks
// ---------------------------------------------------------------------------

/// **The mode snapshot is per block**: a `set_mode` that lands while a block
/// runs — here from an upstream node's `process`, before the node's own op —
/// reaches no op of that block, and the fade starts on the next. One-participant and
/// parallel.
#[test]
fn a_mode_stored_mid_block_lands_on_the_next_block() {
    for workers in WORKERS {
        let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
        if workers > 1 {
            exec.set_pool(Some(common::test_pool(workers)));
        }
        let (hook, handle) = Rendezvous::new();
        ed.insert(key(1), "hook", Unforkable(hook));
        ed.insert(
            key(2),
            "gain",
            Unforkable(TestNode::new(Kind::Gain {
                gain: 3.0,
                width: 1,
            })),
        );
        ed.spec_mut().topology.connect(at(2, 0), direct(1));
        ed.spec_mut().topology.set_outputs(vec![out(2, 0)]);
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        let mut got = Vec::new();
        std::thread::scope(|scope| {
            // The control thread stores once the third block is inside
            // `process`, then lets the block go.
            scope.spawn(|| {
                handle.store_while_inside(|| {
                    ed.set_mode(key(2), NodeMode::Bypassed).expect("a node");
                });
            });
            for _ in 0..8 {
                let mut o = vec![0.0f32; BLOCK];
                exec.process(BLOCK, &TransportPosition::default(), &[], &mut [&mut o[..]]);
                got.extend(o);
            }
        });
        assert!(
            got[..3 * BLOCK].iter().all(|&v| v == 3.0),
            "the block the store landed in is still Active"
        );
        for k in 0..FADE {
            let (g_in, g_out) = gains(k);
            assert_eq!(
                got[3 * BLOCK + k].to_bits(),
                (3.0 * g_out + 1.0 * g_in).to_bits()
            );
        }
    }
}

/// **`set_mode` does not change the plan**: the `Arc` the editor and the
/// executor hold is the same one after it, and a commit that has nothing else
/// to send sends nothing.
#[test]
fn set_mode_does_not_change_the_plan() {
    let mut s = probe_chain(Probe::new(2.0), None);
    let (ed_plan, exec_plan) = (
        Arc::clone(s.ed.base().expect("planned")),
        Arc::clone(s.exec.plan().expect("running")),
    );
    s.mode(1, NodeMode::Bypassed);
    s.ed.commit().expect("commits");
    s.exec.apply_pending();
    s.ed.collect();
    assert!(Arc::ptr_eq(&ed_plan, s.ed.base().expect("planned")));
    assert!(Arc::ptr_eq(&exec_plan, s.exec.plan().expect("running")));
    assert_eq!(s.ed.mode(key(1)), NodeMode::Bypassed);
    assert_eq!(s.ed.mode(key(9)), NodeMode::Active, "absent is Active");
    // Back to Active, and on to Muted: a commit between them still has
    // nothing to send.
    s.mode(1, NodeMode::Active);
    assert_eq!(s.ed.mode(key(1)), NodeMode::Active);
    s.ed.set_mode(key(1), NodeMode::Muted).expect("a node");
    s.ed.commit().expect("commits");
    s.exec.apply_pending();
    s.ed.collect();
    assert!(Arc::ptr_eq(&ed_plan, s.ed.base().expect("planned")));
    let heard = s.run(8, BLOCK, &|_| 0.5);
    assert!(heard[BLOCK * 6..].iter().all(|v| v.to_bits() == 0), "muted");
}

/// `set_mode` refuses a key with no node, and removing the node takes its
/// mode with it.
#[test]
fn modes_name_nodes() {
    let mut s = probe_chain(Probe::new(2.0), None);
    assert_eq!(
        s.ed.set_mode(key(7), NodeMode::Muted),
        Err(ModeError::NoSuchNode { node: key(7) })
    );
    s.mode(1, NodeMode::Muted);
    s.ed.remove(key(1));
    assert_eq!(s.ed.mode(key(1)), NodeMode::Active);
}

/// **A fork copies the modes and starts every unit steady, with no fade**:
/// the live graph, mid-fade to Bypassed, forked, renders its fork the input
/// bit for bit from the fork's first frame (an export hears the state, not a
/// toggle), and keeps fading itself.
#[test]
fn a_fork_copies_the_mode_and_starts_steady() {
    let (mut ed, mut exec) = Editor::new(prepare(MAX_BLOCK));
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    ed.insert(key(1), "probe", ForkByClone(Probe::new(3.0)));
    ed.spec_mut()
        .topology
        .connect(at(1, 0), Edge::Direct(PortSource::MainInput(0)));
    ed.spec_mut().topology.set_outputs(vec![out(1, 0)]);
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let input: Vec<f32> = (0..BLOCK as u64).map(wave).collect();
    let mut o = vec![0.0f32; BLOCK];
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[&input],
        &mut [&mut o[..]],
    );
    ed.set_mode(key(1), NodeMode::Bypassed).expect("a node");
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[&input],
        &mut [&mut o[..]],
    );
    assert_ne!(o[BLOCK - 1], input[BLOCK - 1], "the live graph is mid-fade");

    let (fed, mut fexec) = ed
        .fork(
            ForkTarget::MainOutput,
            prepare(MAX_BLOCK).with_render_mode(RenderMode::Offline),
        )
        .expect("forks");
    assert_eq!(fed.mode(key(1)), NodeMode::Bypassed);
    let mut f = vec![0.0f32; BLOCK];
    fexec.process(
        BLOCK,
        &TransportPosition::default(),
        &[&input],
        &mut [&mut f[..]],
    );
    assert_eq!(
        f.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        input.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    exec.process(
        BLOCK,
        &TransportPosition::default(),
        &[&input],
        &mut [&mut o[..]],
    );
    assert_ne!(o[0], input[0], "and the live graph still fades");
}

/// **A node that declares its own bypass is still called when bypassed**, and
/// the executor applies no crossfade: its output is its own `process`'s, bit
/// for bit, from the first block of the mode on, through the mode and back
/// out of it, and it is never reset (the executor's un-bypass reset is for a
/// node it parked). Its latency stays declared, so the output is not aligned
/// to anything the executor built.
#[test]
fn an_own_bypass_node_runs_its_process_with_no_crossfade() {
    let mut p = Probe::new(3.0);
    p.own_bypass = true;
    p.latency = 40;
    let mut s = probe_chain(p.clone(), None);
    let half = |_: u64| 0.5f32;
    s.run(4, BLOCK, &half);
    s.mode(1, NodeMode::Bypassed);
    let bypassed = s.run(12, BLOCK, &half);
    assert_eq!(p.calls(), 16, "called every block, bypassed or not");
    assert!(
        bypassed.iter().all(|v| v.to_bits() == 1.5f32.to_bits()),
        "its own output at every frame: no blend with the input, no fade"
    );
    s.mode(1, NodeMode::Active);
    let back = s.run(8, BLOCK, &half);
    assert!(back.iter().all(|v| v.to_bits() == 1.5f32.to_bits()));
    assert_eq!(p.resets(), 0, "the executor parked and reset nothing");
}

/// **A node with no pass can be set `Bypassed` when it bypasses itself**
/// (the executor never needs to guess where its input goes), and is still
/// refused `Mix`, which is the executor's blend.
#[test]
fn an_own_bypass_node_needs_no_pass() {
    let (mut ed, _exec) = Editor::new(prepare(MAX_BLOCK));
    let mut wide = Probe::new(1.0);
    wide.ins = 6;
    wide.outs = 2;
    let mut own = wide.clone();
    own.own_bypass = true;
    ed.insert(key(1), "plain", Unforkable(wide));
    ed.insert(key(2), "own", Unforkable(own));
    assert!(matches!(
        ed.set_mode(key(1), NodeMode::Bypassed),
        Err(ModeError::Unplaced { .. })
    ));
    assert_eq!(ed.set_mode(key(2), NodeMode::Bypassed), Ok(()));
    assert_eq!(ed.mode(key(2)), NodeMode::Bypassed);
}

/// **`Muted` stays the executor's for a node with its own bypass**: no format
/// has a mute, so the node is called and its output fades to silence.
#[test]
fn an_own_bypass_node_is_muted_by_the_executor() {
    let mut p = Probe::new(3.0);
    p.own_bypass = true;
    let mut s = probe_chain(p.clone(), None);
    s.mode(1, NodeMode::Muted);
    let muted = s.run(8, BLOCK, &|_| 0.5);
    for (k, &v) in muted.iter().enumerate().take(FADE) {
        let (_, g_out) = gains(k);
        assert_eq!(v.to_bits(), (1.5 * g_out).to_bits(), "fade frame {k}");
    }
    assert!(muted[FADE..].iter().all(|v| v.to_bits() == 0));
}

/// **A plain node is bypassed by the executor as before**: the output is the
/// blend of its own and the input over the declick, from the first frame, and
/// the node is parked once the fade is done.
#[test]
fn a_plain_node_is_still_blended_and_parked_when_bypassed() {
    let p = Probe::new(3.0);
    let mut s = probe_chain(p.clone(), None);
    let half = |_: u64| 0.5f32;
    s.run(4, BLOCK, &half);
    s.mode(1, NodeMode::Bypassed);
    let out = s.run(8, BLOCK, &half);
    for (k, &v) in out.iter().enumerate().take(FADE) {
        let (g_in, g_out) = gains(k);
        assert_eq!(
            v.to_bits(),
            (1.5 * g_out + 0.5 * g_in).to_bits(),
            "fade frame {k}"
        );
    }
    assert!(out[FADE..].iter().all(|v| v.to_bits() == 0.5f32.to_bits()));
    assert_eq!(p.calls(), 8, "called through the fade, then parked");
}
