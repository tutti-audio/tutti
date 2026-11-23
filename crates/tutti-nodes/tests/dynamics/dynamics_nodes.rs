//! The compressor, gate, limiter and brickwall nodes as the graph runs them: what a node is
//! across blocks (a moved cell lands on the grid, a driven control signal is read per frame), resets,
//! parks and clones, the key bus, the latency and the tail. Every figure is from the
//! definition (the grid, the lookahead, the ceiling), never from another render.

use tutti_buffers::Planes;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_dsp::{CompressorControls, GateControls, LimiterControls};
use tutti_graph::contract::{Driven, assert_declared_tail, drive, drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Status, TransportChanges, TransportPosition};
use tutti_nodes::{
    BrickwallLimiterConfig, BrickwallLimiterNode, CompressorConfig, CompressorNode,
    DynamicEqConfig, DynamicEqNode, GateConfig, GateNode, KeySource, LimiterConfig, LimiterNode,
    Lookahead,
};
use tutti_units::{
    CompressionRatio, Db, Frame, Hz, Latency, Q, SampleRate, Samples, Seconds, Tail, UnitParam,
};

const SR: SampleRate = SampleRate::SR_48K;
/// The frames between two designs while a control moves: `Kernel::GRID` of these nodes.
const GRID: usize = 16;

fn noise(seed: u32, frames: usize, level: f32) -> Vec<f32> {
    tutti_fixtures::noise(seed, frames)
        .into_iter()
        .map(|x| level * x)
        .collect()
}

/// A node that renders `inputs` (one per input channel, the key's after the audio's) in blocks
/// of `block` frames, `signals[k]` driving param `k`, and `write` called with the node's
/// `ParamSet` before the block that begins at frame `at`.
fn render<N: Node + ParamNode>(
    node: N,
    inputs: &[&[f32]],
    signals: &[Option<&[f32]>],
    block: usize,
    at: usize,
    write: impl Fn(&tutti_graph::ParamSet),
) -> Vec<Vec<f32>> {
    let set = node.param_set();
    let mut node = prepared(node, SR, 1_024);
    let n = inputs[0].len();
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut start = 0;
    while start < n {
        if start == at {
            write(&set);
        }
        let end = (start + block).min(n);
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[start..end]).collect();
        let ps: Vec<Option<&[f32]>> = signals.iter().map(|l| l.map(|l| &l[start..end])).collect();
        let o = drive(&mut node, SR, Planes::new(&ins).unwrap(), &ps);
        out.resize(o.len(), Vec::new());
        for (all, more) in out.iter_mut().zip(o) {
            all.extend(more);
        }
        start = end;
    }
    out
}

/// The first frame at which two renders differ, on any channel.
fn first_difference(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<usize> {
    let n = a[0].len();
    (0..n).find(|&i| {
        a.iter()
            .zip(b)
            .any(|(x, y)| x[i].to_bits() != y[i].to_bits())
    })
}

fn compressor() -> CompressorNode<f32> {
    CompressorNode::<f32>::from(
        CompressorConfig::new(ChannelTopology::STEREO)
            .key(KeySource::Sidechain)
            .controls(CompressorControls {
                threshold: Db::new(-20.0),
                knee: Db::new(6.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.0),
                release: Seconds::new(0.0),
                makeup: Db::new(0.0),
                ..CompressorControls::DEFAULT
            }),
    )
}

fn gate() -> GateNode<f32> {
    GateNode::<f32>::from(
        GateConfig::new(ChannelTopology::STEREO)
            .key(KeySource::Sidechain)
            .controls(GateControls {
                threshold: Db::new(-30.0),
                attack: Seconds::new(0.0),
                hold: Seconds::new(0.0),
                release: Seconds::new(1.0),
                range: Db::new(-40.0),
                ..GateControls::DEFAULT
            }),
    )
}

fn limiter() -> LimiterNode<f32> {
    LimiterNode::<f32>::from(LimiterConfig::new(ChannelTopology::STEREO).controls(
        LimiterControls {
            threshold: Db::new(-12.0),
            ceiling: Db::new(-3.0),
            release: Seconds::new(0.05),
        },
    ))
}

fn brickwall() -> BrickwallLimiterNode<f32> {
    BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
        ChannelTopology::STEREO,
        Db::new(0.0),
    ))
}

/// A write between blocks to a control that lands whole starts at the next grid point: with
/// the blocks cut so one begins at frame 1000 (8 past the grid point at 992), the render is
/// the unmoved one bit for bit up to frame 1007 and differs from frame 1008, the next
/// multiple of the grid, whatever the block after it. The signal makes the move heard on the
/// frame it lands: a constant one, and times that are instant.
#[test]
fn a_move_between_grid_points_starts_at_the_next_one() {
    const N: usize = 1_400;
    let loud = vec![0.9f32; N];
    let ones = vec![1.0f32; N];
    let at = 1_000;
    assert_eq!(at % GRID, 8);
    let want = (at / GRID + 1) * GRID;

    // Compressor: the ratio of a loud key, from 4 to 20.
    let run = |moved: bool| {
        render(
            compressor(),
            &[&ones, &ones, &loud, &loud],
            &[],
            8,
            at,
            |set| {
                if moved {
                    assert!(set.set(UnitParam::Ratio, 20.0));
                }
            },
        )
    };
    assert_eq!(
        first_difference(&run(false), &run(true)),
        Some(want),
        "compressor"
    );

    // Gate: the release from 1 s to instant, on a key that has just gone quiet.
    let mut key = vec![0.5f32; 500];
    key.resize(N, 0.0);
    let run = |moved: bool| {
        render(gate(), &[&ones, &ones, &key, &key], &[], 8, at, |set| {
            if moved {
                assert!(set.set(UnitParam::Release, 0.0));
            }
        })
    };
    assert_eq!(
        first_difference(&run(false), &run(true)),
        Some(want),
        "gate"
    );

    // Limiter: the ceiling from -3 to -12 dB over a constant input. The reduction reaches
    // the output on the frame its input frame is designed at (the window looks back over
    // the lookahead, so the frames before were already reduced by it only from there).
    let run = |moved: bool| {
        render(limiter(), &[&ones, &ones], &[], 8, at, |set| {
            if moved {
                assert!(set.set(UnitParam::Ceiling, -12.0));
            }
        })
    };
    assert_eq!(
        first_difference(&run(false), &run(true)),
        Some(want),
        "limiter"
    );

    // Brickwall: the ceiling from 0 to -6 dB.
    let run = |moved: bool| {
        render(brickwall(), &[&ones, &ones], &[], 8, at, |set| {
            if moved {
                assert!(set.set(UnitParam::Ceiling, -6.0));
            }
        })
    };
    assert_eq!(
        first_difference(&run(false), &run(true)),
        Some(want),
        "brickwall"
    );
}

/// What a node is under cuts: a write between blocks at frame 1024 (a multiple of every
/// block below) and a driven control signal give the same render in blocks of 1, 16, 64, 128 and 1024,
/// with the key bus fed, and the move and the control signal are heard.
#[test]
fn a_moved_cell_and_a_driven_signal_render_the_same_bits_in_any_blocks() {
    const N: usize = 4_096;
    let audio = [noise(1, N, 0.9), noise(2, N, 0.9)];
    let key = [noise(3, N, 0.9), noise(4, N, 0.9)];
    let threshold: Vec<f32> = (0..N)
        .map(|i| -40.0 + 30.0 * (i % 700) as f32 / 700.0)
        .collect();
    let ceiling: Vec<f32> = (0..N)
        .map(|i| -3.0 - 9.0 * (i % 500) as f32 / 500.0)
        .collect();

    type Case<'a> = (
        &'static str,
        Box<dyn Fn(usize, bool, bool) -> Vec<Vec<f32>> + 'a>,
    );
    let cases: Vec<Case<'_>> = vec![
        (
            "compressor",
            Box::new(|block, moved, driven| {
                render(
                    CompressorNode::<f32>::from(
                        CompressorConfig::new(ChannelTopology::STEREO)
                            .key(KeySource::Sidechain)
                            .controls(CompressorControls {
                                threshold: Db::new(-20.0),
                                knee: Db::new(6.0),
                                ratio: CompressionRatio::new(4.0),
                                attack: Seconds::new(0.002),
                                release: Seconds::new(0.03),
                                makeup: Db::new(0.0),
                                ..CompressorControls::DEFAULT
                            }),
                    ),
                    &[&audio[0], &audio[1], &key[0], &key[1]],
                    &[driven.then_some(&threshold[..])],
                    block,
                    1_024,
                    |set| {
                        if moved {
                            assert!(set.set(UnitParam::Ratio, 12.0));
                            assert!(set.set(UnitParam::GainDb, 6.0));
                        }
                    },
                )
            }),
        ),
        (
            "gate",
            Box::new(|block, moved, driven| {
                render(
                    GateNode::<f32>::from(
                        GateConfig::new(ChannelTopology::STEREO)
                            .key(KeySource::Sidechain)
                            .controls(GateControls {
                                threshold: Db::new(-30.0),
                                attack: Seconds::new(0.002),
                                hold: Seconds::new(0.01),
                                release: Seconds::new(0.03),
                                range: Db::new(-30.0),
                                ..GateControls::DEFAULT
                            }),
                    ),
                    &[&audio[0], &audio[1], &key[0], &key[1]],
                    &[driven.then_some(&threshold[..])],
                    block,
                    1_024,
                    |set| {
                        if moved {
                            assert!(set.set(UnitParam::Range, -6.0));
                            assert!(set.set(UnitParam::Hold, 0.1));
                        }
                    },
                )
            }),
        ),
        (
            // Nothing drives it per frame: the control signal is not heard (see the loop).
            "dynamic eq",
            Box::new(|block, moved, _| {
                render(
                    DynamicEqNode::<f32>::from(
                        DynamicEqConfig::new(ChannelLayout::STEREO, Hz::new(1_000.0), Q::new(1.5))
                            .threshold(Db::new(-30.0))
                            .ratio(CompressionRatio::new(4.0))
                            .knee(Db::new(6.0))
                            .attack(Seconds::new(0.002))
                            .release(Seconds::new(0.03))
                            .key(KeySource::Sidechain),
                    ),
                    &[&audio[0], &audio[1], &key[0], &key[1]],
                    &[],
                    block,
                    1_024,
                    |set| {
                        if moved {
                            assert!(set.set(UnitParam::Ratio, 12.0));
                            assert!(set.set(UnitParam::Cutoff, 2_500.0));
                        }
                    },
                )
            }),
        ),
        (
            "limiter",
            Box::new(|block, moved, driven| {
                render(
                    limiter(),
                    &[&audio[0], &audio[1]],
                    &[
                        driven.then_some(&threshold[..]),
                        driven.then_some(&ceiling[..]),
                    ],
                    block,
                    1_024,
                    |set| {
                        if moved {
                            assert!(set.set(UnitParam::Release, 0.4));
                            assert!(set.set(UnitParam::Threshold, -24.0));
                        }
                    },
                )
            }),
        ),
        (
            "brickwall",
            Box::new(|block, moved, driven| {
                render(
                    BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                        ChannelTopology::STEREO,
                        Db::new(-1.0),
                    )),
                    &[&audio[0], &audio[1]],
                    &[driven.then_some(&ceiling[..])],
                    block,
                    1_024,
                    |set| {
                        if moved {
                            assert!(set.set(UnitParam::Ceiling, -9.0));
                        }
                    },
                )
            }),
        ),
    ];
    for (name, make) in &cases {
        let whole = make(1_024, true, true);
        assert_ne!(
            make(1_024, true, false),
            make(1_024, false, false),
            "{name}: the move is heard"
        );
        if *name != "dynamic eq" {
            assert_ne!(
                whole,
                make(1_024, true, false),
                "{name}: the signal is heard"
            );
        }
        for block in [1, 16, 64, 128] {
            assert_eq!(make(block, true, true), whole, "{name}: blocks of {block}");
        }
    }
}

/// A reset node renders as a fresh one, bit for bit: after a render and a `reset`, the same
/// stream gives the same bits as on a node that never ran.
#[test]
fn a_reset_node_renders_as_a_fresh_one() {
    const N: usize = 3_000;
    let audio = [noise(5, N, 0.9), noise(6, N, 0.9)];
    let key = [noise(7, N, 0.9), noise(8, N, 0.9)];
    let all: [&[f32]; 4] = [&audio[0], &audio[1], &key[0], &key[1]];
    let two: [&[f32]; 2] = [&audio[0], &audio[1]];
    let stream = |node: &mut dyn Node, ins: &[&[f32]]| -> Vec<Vec<f32>> {
        let mut out: Vec<Vec<f32>> = Vec::new();
        for start in (0..N).step_by(100) {
            let chunk: Vec<&[f32]> = ins.iter().map(|c| &c[start..start + 100]).collect();
            let o = drive(node, SR, Planes::new(&chunk).unwrap(), &[]);
            out.resize(o.len(), Vec::new());
            for (all, more) in out.iter_mut().zip(o) {
                all.extend(more);
            }
        }
        out
    };
    let nodes: Vec<(&str, Box<dyn Fn() -> Box<dyn Node>>, &[&[f32]])> = vec![
        (
            "compressor",
            Box::new(|| Box::new(prepared(compressor(), SR, 1_024))),
            &all,
        ),
        (
            "gate",
            Box::new(|| Box::new(prepared(gate(), SR, 1_024))),
            &all,
        ),
        (
            "limiter",
            Box::new(|| Box::new(prepared(limiter(), SR, 1_024))),
            &two,
        ),
        (
            "brickwall",
            Box::new(|| Box::new(prepared(brickwall(), SR, 1_024))),
            &two,
        ),
    ];
    for (name, make, ins) in nodes {
        let mut used = make();
        let _ = stream(&mut *used, ins);
        used.reset();
        let mut fresh = make();
        assert_eq!(stream(&mut *used, ins), stream(&mut *fresh, ins), "{name}");
    }
}

/// A clone shares the cells and starts silent: it renders as a fresh node does, whatever the
/// original had rendered, and a write through the original's `ParamSet` is the clone's.
#[test]
fn a_clone_starts_silent_and_shares_the_cells() {
    let x = noise(9, 2_000, 0.9);
    let run = |node: &mut dyn Node| -> Vec<Vec<f32>> {
        let mut out = vec![Vec::new(), Vec::new()];
        for chunk in x.chunks(100) {
            for (o, y) in
                out.iter_mut()
                    .zip(drive(node, SR, Planes::new(&[chunk, chunk]).unwrap(), &[]))
            {
                o.extend(y);
            }
        }
        out
    };
    let mut used = prepared(limiter(), SR, 1_024);
    let _ = run(&mut used);
    let mut clone = used.clone();
    let mut fresh = prepared(limiter(), SR, 1_024);
    assert_eq!(run(&mut clone), run(&mut fresh));
    assert!(used.param_set().set(UnitParam::Ceiling, -9.0));
    assert_eq!(clone.param_set().get(UnitParam::Ceiling), Some(-9.0));
}

/// The limiter's latency is the lookahead at the rate and it declares no tail: an impulse comes
/// out exactly the latency later and the output is exactly zero from then on, with the node
/// still rendering (it never parks, so its release runs on across a gap of silence).
#[test]
fn the_limiter_reports_its_lookahead_as_latency_and_never_parks() {
    let rate = SampleRate::SR_44K1;
    let lookahead = Lookahead::of(Seconds::new(0.005));
    let make = || {
        prepared(
            LimiterNode::<f32>::from(
                LimiterConfig::new(ChannelTopology::STEREO)
                    .lookahead(lookahead)
                    .controls(LimiterControls::DEFAULT),
            ),
            rate,
            64,
        )
    };
    let node = make();
    // 5 ms is 220.5 frames at 44.1 kHz, rounded up.
    let frames = 221;
    assert_eq!(node.shape().latency, Latency::new(Samples::new(frames)));
    assert_eq!(node.shape().tail, Tail::None);

    let mut node = make();
    let mut x = vec![0.0f32; 64];
    x[0] = 1.0;
    let silence = vec![0.0f32; 64];
    let mut out = Vec::new();
    for block in 0..64 {
        let env = Env {
            frame: Frame::new(block * 64),
            sample_rate: rate,
            block_len: Samples::new(64),
            transport: TransportPosition::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let input = if block == 0 { &x } else { &silence };
        let d: Driven = drive_input(
            &mut node,
            &env,
            Planes::new(&[input, input]).unwrap(),
            &[],
            &[],
        );
        assert_eq!(d.reported_tail, None, "block {block}: nothing narrowed");
        assert_eq!(
            d.status,
            Some(Status::Modified),
            "block {block}: never parked"
        );
        out.extend(d.audio[0].iter().copied());
    }
    let sounding: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, s)| **s != 0.0)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        sounding,
        [frames],
        "the impulse leaves the delay once, on frame {frames}"
    );

    assert_declared_tail(&mut make(), rate);
}

/// The key is the input bus with the sidechain role, after the audio's, as wide: a compressor and a gate keyed
/// from it listen to the key and not the signal, and one on its own signal has no key bus.
#[test]
fn the_key_is_the_sidechain_role_bus_and_the_detector_listens_to_it() {
    for (name, keyed, own) in [
        (
            "compressor",
            Box::new(compressor()) as Box<dyn Node>,
            Box::new(CompressorNode::<f32>::from(
                CompressorConfig::new(ChannelTopology::STEREO)
                    .controls(CompressorControls::DEFAULT),
            )) as Box<dyn Node>,
        ),
        (
            "gate",
            Box::new(gate()) as Box<dyn Node>,
            Box::new(GateNode::<f32>::from(
                GateConfig::new(ChannelTopology::STEREO).controls(GateControls::DEFAULT),
            )) as Box<dyn Node>,
        ),
    ] {
        let shape = keyed.shape();
        assert_eq!(
            shape.audio_in().count(),
            4,
            "{name}: the signal and its key"
        );
        let key = shape.input_bus(1).expect("a key bus");
        assert_eq!(key.width(), 2, "{name}");
        assert_eq!(
            own.shape().audio_in().count(),
            2,
            "{name}: no key bus on its own signal"
        );
        assert!(own.shape().input_bus(1).is_none(), "{name}");
    }

    // The detector reads the key: a quiet signal under a loud key is ducked, and a loud one
    // under a silent key is not; and the loudest key channel sets one gain for both channels.
    let n = 6_000;
    let (quiet, loud, silent) = (vec![0.02f32; n], vec![0.9f32; n], vec![0.0f32; n]);
    let ducked = render(
        compressor(),
        &[&quiet, &quiet, &loud, &loud],
        &[],
        100,
        usize::MAX,
        |_| {},
    );
    assert!(
        ducked[0][n - 1] < 0.5 * 0.02,
        "a loud key ducks a quiet signal"
    );
    let untouched = render(
        compressor(),
        &[&loud, &loud, &silent, &silent],
        &[],
        100,
        usize::MAX,
        |_| {},
    );
    assert!(
        untouched[0].iter().all(|&s| s == 0.9),
        "a silent key reduces nothing"
    );
    let right = render(
        compressor(),
        &[&quiet, &quiet, &silent, &loud],
        &[],
        100,
        usize::MAX,
        |_| {},
    );
    assert_eq!(
        right, ducked,
        "the key's loudest channel sets the gain of every channel"
    );
    let closed = render(
        gate(),
        &[&loud, &loud, &silent, &silent],
        &[],
        100,
        usize::MAX,
        |_| {},
    );
    assert!(
        closed[0][n - 1] < 0.9 * 0.02,
        "a silent key holds the gate closed"
    );
    let open = render(
        gate(),
        &[&quiet, &quiet, &loud, &loud],
        &[],
        100,
        usize::MAX,
        |_| {},
    );
    assert!((open[0][n - 1] - 0.02).abs() < 1e-6, "a loud key opens it");
}

/// The compressor, the gate, the limiter and the brickwall declare no tail, and a node whose
/// follower tracks a key (or whose envelope runs on) never parks, however long the signal is
/// silent.
#[test]
fn no_node_here_parks() {
    let nodes: Vec<Box<dyn Node>> = vec![
        Box::new(prepared(compressor(), SR, 64)),
        Box::new(prepared(gate(), SR, 64)),
        Box::new(prepared(limiter(), SR, 64)),
        Box::new(prepared(brickwall(), SR, 64)),
    ];
    for mut node in nodes {
        let shape = node.shape();
        assert_eq!(shape.tail, Tail::None);
        let silence = vec![0.0f32; 64];
        let width = shape.audio_in().count();
        let ins: Vec<&[f32]> = vec![&silence; usize::from(width)];
        for block in 0..400 {
            let env = Env {
                frame: Frame::new(block * 64),
                sample_rate: SR,
                block_len: Samples::new(64),
                transport: TransportPosition::default(),
                changes: TransportChanges::NONE,
                play: PlayClock::ZERO,
            };
            let d = drive_input(&mut *node, &env, Planes::new(&ins).unwrap(), &[], &[]);
            assert_eq!(d.status, Some(Status::Modified), "block {block}");
        }
    }
}
