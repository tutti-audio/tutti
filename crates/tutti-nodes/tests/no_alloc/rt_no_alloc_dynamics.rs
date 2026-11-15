//! Regression gate: dynamics processors must not allocate per-buffer.
//!
//! Covers the four `tutti-nodes` dynamics nodes: `CompressorNode` (mono +
//! stereo), `GateNode` (mono + stereo), `LimiterNode` (lookahead), and
//! `BrickwallLimiterNode` (zero-latency clipper). Each runs alone
//! in a graph through `tutti_graph::contract::BlockRig`, so the gate walks
//! the executor's block path as well as the node's.
//!
//! Lookahead limiter is the most failure-prone of the group — it carries
//! a monotonic deque + a ring buffer, both of which would historically
//! be tempting to resize on `set_lookahead`. Its steady gates warm up
//! outside the measurement; a burst and its release are measured from the
//! first block, which is where the deque fills to its whole window.
//! Lookahead changes are off-RT (`prepare`).

use tutti_graph::IntoNode;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    BrickwallLimiterNode, CompressorControls, CompressorNode, GateControls, GateNode, KeySource,
    LimiterControls, LimiterNode, Lookahead,
};
use tutti_types::{BusTopology, ChannelLayout, CompressionRatio, Db, SampleRate, Seconds};

/// `node` alone in a graph through `BlockRig`: every input an alternating
/// ±`amplitude` (to keep the envelope follower active), `warm` warm-up
/// blocks, then `blocks` 64-frame blocks under `assert_no_alloc`.
fn gate<N: IntoNode>(node: N, amplitude: f32, warm: usize, blocks: usize) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { amplitude } else { -amplitude };
        }
    }
    for _ in 0..warm {
        rig.block();
    }
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..blocks {
            rig.block();
        }
    });
}

#[test]
fn compressor_mono_process_is_allocation_free() {
    // 2 inputs (audio + sidechain), 1 output.
    gate(
        CompressorNode::new(
            BusTopology::MONO,
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db::new(-20.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.005),
                release: Seconds::new(0.050),
                ..CompressorControls::DEFAULT
            },
        ),
        0.7,
        16,
        2_000,
    );
}

#[test]
fn compressor_stereo_process_is_allocation_free() {
    // 4 inputs (L, R, SC-L, SC-R), 2 outputs, linked gain.
    gate(
        CompressorNode::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db::new(-18.0),
                knee: Db::new(6.0),
                ratio: CompressionRatio::new(3.0),
                attack: Seconds::new(0.003),
                release: Seconds::new(0.080),
                ..CompressorControls::DEFAULT
            },
        ),
        0.8,
        16,
        2_000,
    );
}

/// The compressor's lane path with every cell moving too: the threshold port fed a per-frame
/// ramp with a NaN frame (the `Inputs` lane the recurrence reads), and each block writes
/// the ratio, the times, the knee and the makeup (a new design on the stream grid, the makeup
/// gliding) through the `ParamSet`, 6 channels, 2 000 blocks under `assert_no_alloc`.
#[test]
fn compressor_fed_threshold_and_moving_cells_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_types::UnitParam;

    let mut node = Direct::new(
        CompressorNode::new(
            tutti_types::BusTopology::assumed(tutti_types::ChannelLayout::from(6u16))
                .expect("at most one bus wide"),
            KeySource::Sidechain,
            CompressorControls {
                threshold: Db::new(-20.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.003),
                release: Seconds::new(0.08),
                ..CompressorControls::DEFAULT
            },
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 0.8 } else { -0.8 };
        }
    }
    let mut lane: Vec<f32> = (0..64).map(|i| -30.0 + 0.3 * i as f32).collect();
    lane[9] = f32::NAN;
    node.feed(0, Some(&lane));
    let set = node.node.param_set();
    for _ in 0..16 {
        node.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for b in 0..2_000 {
            let k = (b % 40) as f32;
            assert!(set.set(UnitParam::Ratio, 2.0 + 0.2 * k));
            assert!(set.set(UnitParam::Attack, 0.0005 + 0.0002 * k));
            assert!(set.set(UnitParam::Release, 0.01 + 0.005 * k));
            assert!(set.set(UnitParam::Knee, 0.3 * k));
            assert!(set.set(UnitParam::GainDb, k - 20.0));
            node.block();
        }
    });
}

#[test]
fn limiter_node_process_is_allocation_free() {
    // Warm-up fills the lookahead ring and primes the deque; over ceiling.
    gate(
        LimiterNode::stereo(LimiterControls {
            threshold: Db::new(-3.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        }),
        1.5,
        32,
        2_000,
    );
}

#[test]
fn limiter_node_wide_6ch_process_is_allocation_free() {
    // The per-channel lookahead rings + frame scratch must be built before
    // the first block; the linked-gain wide path must not allocate per
    // buffer.
    gate(
        LimiterNode::new(
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("at most one bus wide"),
            Lookahead::DEFAULT,
            LimiterControls {
                threshold: Db::new(-3.0),
                ceiling: Db::new(-0.3),
                ..LimiterControls::DEFAULT
            },
        ),
        1.5,
        32,
        2_000,
    );
}

#[test]
fn brickwall_limiter_wide_6ch_process_is_allocation_free() {
    gate(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        1.5,
        1,
        5_000,
    );
}

#[test]
fn brickwall_limiter_process_is_allocation_free() {
    gate(BrickwallLimiterNode::new(Db::new(-0.3)), 1.5, 1, 5_000);
}

#[test]
fn gate_mono_process_is_allocation_free() {
    // 2 inputs (audio + sidechain), 1 output.
    gate(
        GateNode::new(
            BusTopology::MONO,
            KeySource::Sidechain,
            GateControls {
                threshold: Db::new(-40.0),
                attack: Seconds::new(0.001),
                hold: Seconds::new(0.010),
                release: Seconds::new(0.100),
                range: Db::new(-60.0),
            },
        ),
        0.5,
        16,
        2_000,
    );
}

#[test]
fn gate_stereo_process_is_allocation_free() {
    gate(
        GateNode::new(
            BusTopology::STEREO,
            KeySource::Sidechain,
            GateControls {
                threshold: Db::new(-40.0),
                attack: Seconds::new(0.001),
                hold: Seconds::new(0.010),
                release: Seconds::new(0.100),
                range: Db::new(-80.0),
            },
        ),
        0.6,
        16,
        2_000,
    );
}

/// A lookahead limiter through a burst and its release, every block under
/// `assert_no_alloc` from the first. Whenever the envelope rises (from the
/// node's initial zero onto the burst, and through the release after it)
/// every gain is larger than the last, so the window-minimum deque keeps
/// every push and fills to the whole lookahead window. The constant-drive
/// gates above warm up outside the gate, so the one fill that would grow
/// the deque happens before they measure.
#[test]
fn limiter_node_burst_and_release_are_allocation_free() {
    let node = LimiterNode::stereo(LimiterControls {
        threshold: Db::new(-3.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    });
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    let fill = |rig: &mut BlockRig, amplitude: f32| {
        for c in rig.inputs_mut() {
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    // 0.1 s of release at 48 kHz is 75 blocks of 64: each phase runs well
    // past the 240-frame window, so the deque fills and turns over.
    assert_no_alloc::assert_no_alloc(|| {
        fill(&mut rig, 4.0);
        for _ in 0..200 {
            rig.block();
        }
        fill(&mut rig, 0.05);
        for _ in 0..200 {
            rig.block();
        }
    });
    assert!(
        rig.output(0).iter().any(|s| s.abs() > 1e-6),
        "the limiter rendered silence after the release"
    );
}

/// The brickwall with its ceiling port fed a per-frame ramp (the `Inputs` lane the
/// recurrence reads each frame), 6 channels, 1 000 blocks under `assert_no_alloc`.
/// `Direct` feeds the port, which `BlockRig` cannot.
#[test]
fn brickwall_limiter_fed_ceiling_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let ceiling: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    node.feed(0, Some(&ceiling));
    node.block();
    let first = tutti_types::Db(-12.0).to_amplitude().get();
    assert!(
        (node.output(5)[0] - first).abs() < 1e-5,
        "the fed ceiling did not reach the clip: {}",
        node.output(5)[0]
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            node.block();
        }
    });
}

/// The brickwall's lane path with the cell moving too, and a lane carrying every
/// value the clip must survive (NaN, `+-inf`, a mute): each block writes the cell to a
/// new ceiling through the `ParamSet` (a new design, retargeted on the audio thread),
/// 2 000 blocks under `assert_no_alloc`.
#[test]
fn brickwall_limiter_moving_cell_and_hostile_lane_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_types::UnitParam;

    let mut node = Direct::new(
        BrickwallLimiterNode::with_bus(
            BusTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let mut lane: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    (lane[3], lane[4], lane[5], lane[6]) = (f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN);
    node.feed(0, Some(&lane));
    let set = node.node.param_set();
    node.block();
    assert_no_alloc::assert_no_alloc(|| {
        for b in 0..2_000 {
            let db = -20.0 + (b % 40) as f32 * 0.5;
            assert!(set.set(UnitParam::Ceiling, if b % 7 == 0 { f32::NAN } else { db }));
            node.block();
        }
    });
}

/// The lookahead limiter with its ceiling and threshold ports fed per frame
/// (the path that reads both per sample instead of through the block's
/// curve), 6 channels, 1 000 blocks under `assert_no_alloc`, the first
/// measured after a warm-up that has the limiter reducing.
#[test]
fn limiter_node_fed_ceiling_and_threshold_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        LimiterNode::new(
            BusTopology::assumed(ChannelLayout::from(6u16)).expect("at most one bus wide"),
            Lookahead::DEFAULT,
            LimiterControls::DEFAULT,
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut() {
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let ceiling: Vec<f32> = (0..64).map(|i| -6.0 + 0.0625 * i as f32).collect();
    let threshold: Vec<f32> = (0..64).map(|i| -12.0 + 0.1 * i as f32).collect();
    // The ports are in the set's order: threshold, ceiling.
    node.feed(0, Some(&threshold));
    node.feed(1, Some(&ceiling));
    for _ in 0..32 {
        node.block();
    }
    assert!(
        node.output(0).iter().any(|y| y.abs() < 1.0),
        "the limiter was not reducing; the gate would walk no gain curve"
    );
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            node.block();
        }
    });
}

/// A lookahead limiter cloned mid-release, and the clone processed on
/// through the rest of the release under `assert_no_alloc`, with no
/// `prepare` between (which would rebuild the deque at full capacity and
/// hide a short one). At the clone the window-minimum deque holds about
/// one block of a rising run, fewer entries than its window; the clone must
/// still hold the whole window's allocation, or the release grows it on
/// the audio thread.
#[test]
fn a_limiter_cloned_mid_release_processes_allocation_free() {
    use tutti_graph::contract::Direct;

    let node = LimiterNode::stereo(LimiterControls {
        threshold: Db::new(-3.0),
        ceiling: Db::new(-0.3),
        ..LimiterControls::DEFAULT
    });
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let fill = |d: &mut Direct<LimiterNode>, amplitude: f32| {
        for c in d.inputs_mut() {
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    // A held burst, long enough (25 release time constants) for the
    // envelope to settle: the gain is then constant, and the deque holds
    // one entry.
    fill(&mut d, 4.0);
    for _ in 0..1_875 {
        d.block();
    }
    // One block into the release: 64 rising gains of a 240-frame window.
    fill(&mut d, 0.05);
    d.block();
    d.node = d.node.clone();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..200 {
            d.block();
        }
    });
    assert!(
        d.output(0).iter().any(|s| s.abs() > 1e-6),
        "the clone rendered silence after the release"
    );
}

/// What a corner sweep drives: the controls a node has at their two ends (a write each block
/// from one end to the other, so a design is in flight on all of them), the ports a lane
/// can drive, and the bound its output keeps.
struct Corners {
    /// Each control and its two ends.
    cells: &'static [(tutti_types::UnitParam, f32, f32)],
    /// The param ports that read a lane per frame.
    lanes: &'static [usize],
    /// The lane values: a lane cycles through them, so every frame is a corner or hostile.
    lane_values: &'static [&'static [f32]],
    /// The most a sample may reach: the largest ceiling or gain the corners ask for, times
    /// the input.
    bound: f32,
    /// The frames a window renders, at least: enough to fill and wrap the longest ring.
    frames: usize,
}

/// `node` at every corner of `corners`, in blocks of 1, 7, 33 and 64 frames, with the lanes
/// fed none and then each of their value sets: each cell written to an end every block
/// (alternating, then all low, all high), the input a square at `amplitude` with silent
/// stretches (a parking node parks and wakes), a key sample (from channel `key_from`) that is
/// infinite every 50 blocks,
/// and a `reset` inside the window at 90 and 99 percent of it (after a ring of
/// `corners.frames` has wrapped); between windows the node is prepared again and forked. Every output sample is
/// finite and within the bound.
fn sweep<N: tutti_graph::Node + tutti_graph::ParamNode>(
    make: impl Fn() -> N,
    corners: &Corners,
    amplitude: f32,
    key_from: Option<usize>,
) {
    use tutti_graph::Prepare;
    use tutti_graph::contract::Direct;
    use tutti_types::{MaxBlock, Samples};

    let fill = |d: &mut Direct<N>, amplitude: f32| {
        for c in d.inputs_mut() {
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    for block in [1usize, 7, 33, 64] {
        let mut d = Direct::new(make(), SampleRate::SR_48K, 64);
        d.set_block_len(block);
        let mut set = d.node.param_set();
        let blocks = corners.frames.div_ceil(block);
        let outs = usize::from(d.node.shape().audio_out().count());
        let variants: Vec<Option<&[f32]>> = std::iter::once(None)
            .chain(corners.lane_values.iter().copied().map(Some))
            .collect();
        for (v, values) in variants.iter().enumerate() {
            for &port in corners.lanes {
                let lane: Option<Vec<f32>> =
                    values.map(|vals| (0..64).map(|i| vals[(i + port) % vals.len()]).collect());
                d.feed(port, lane.as_deref());
            }
            for phase in 0..3usize {
                let (mut finite, mut within) = (true, true);
                fill(&mut d, amplitude);
                assert_no_alloc::assert_no_alloc(|| {
                    for b in 0..blocks {
                        // Silent from 65 to 80 percent of the window: a node that parks
                        // parks and wakes.
                        if b == blocks * 65 / 100 {
                            for c in d.inputs_mut() {
                                c.fill(0.0);
                            }
                        } else if b == blocks * 80 / 100 {
                            for c in d.inputs_mut() {
                                for (i, x) in c.iter_mut().enumerate() {
                                    *x = if i % 2 == 0 { amplitude } else { -amplitude };
                                }
                            }
                        }
                        for &(addr, lo, hi) in corners.cells {
                            let high = match phase {
                                0 => false,
                                1 => true,
                                _ => b % 2 == 1,
                            };
                            set.set(addr, if high { hi } else { lo });
                        }
                        if b == blocks * 90 / 100 || b == blocks * 99 / 100 {
                            d.node.reset();
                        }
                        if let Some(k) = key_from {
                            let hot = b % 50 == 5 && b < blocks * 65 / 100;
                            let silent = (blocks * 65 / 100..blocks * 80 / 100).contains(&b);
                            for c in &mut d.inputs_mut()[k..] {
                                c[0] = if hot {
                                    f32::INFINITY
                                } else if silent {
                                    0.0
                                } else {
                                    amplitude
                                };
                            }
                        }
                        d.block();
                        for c in 0..outs {
                            for &s in d.output(c) {
                                finite &= s.is_finite();
                                within &= s.abs() <= corners.bound;
                            }
                        }
                    }
                });
                assert!(
                    finite,
                    "block {block}, lanes {v}, phase {phase}: not finite"
                );
                assert!(
                    within,
                    "block {block}, lanes {v}, phase {phase}: past the bound"
                );
            }
            // Between windows: prepared again, and forked, the control side's own work.
            d.node
                .prepare(&Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
            let fork = d.node.fork_fresh();
            *d.node = fork;
            d.node
                .prepare(&Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
            set = d.node.param_set();
        }
    }
}

const HOSTILE_DB: &[f32] = &[
    -200.0,
    24.0,
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    -6.0,
    0.0,
];

/// The compressor at every corner, stereo and 6 wide, keyed and on its own signal: threshold at
/// both ends and as a hostile lane, knee, ratio (below 1 and 1000), times (instant and 10 s)
/// and makeup (to a clamped 1e30 dB), with an infinite key sample now and then.
#[test]
fn the_compressor_corners_are_allocation_free_and_finite() {
    use tutti_types::UnitParam::{Attack, GainDb, Knee, Ratio, Release, Threshold};
    static CORNERS: Corners = Corners {
        cells: &[
            (Threshold, -200.0, 60.0),
            (Knee, 0.0, 200.0),
            (Ratio, 0.25, 1000.0),
            (Attack, 0.0, 10.0),
            (Release, 0.0, 10.0),
            (GainDb, -120.0, 24.0),
        ],
        lanes: &[0],
        lane_values: &[HOSTILE_DB],
        // 24 dB of makeup on a 1.5 square: 23.8.
        bound: 24.0,
        frames: 19_200,
    };
    static CLAMPED: Corners = Corners {
        cells: &[(GainDb, -1.0e30, 1.0e30), (Ratio, 1.0, 1.0)],
        lanes: &[0],
        lane_values: &[HOSTILE_DB],
        // The makeup is held to 400 dB: 1e20 on a 1.5 square, a finite gain.
        bound: 1.0e21,
        frames: 19_200,
    };
    for width in [2u16, 6] {
        let bus = BusTopology::assumed(ChannelLayout::from(width)).expect("one bus");
        let key = Some(usize::from(width));
        for corners in [&CORNERS, &CLAMPED] {
            sweep(
                || CompressorNode::new(bus, KeySource::Sidechain, CompressorControls::DEFAULT),
                corners,
                1.5,
                key,
            );
            sweep(
                || CompressorNode::new(bus, KeySource::Own, CompressorControls::DEFAULT),
                corners,
                1.5,
                None,
            );
        }
    }
}

/// The gate at every corner: the threshold at both ends and as a hostile lane, times from
/// instant to 5 s (the hold is 240 000 frames), and the range from 120 dB down to above 0 dB,
/// with an infinite key sample now and then.
#[test]
fn the_gate_corners_are_allocation_free_and_finite() {
    use tutti_types::UnitParam::{Attack, Hold, Range, Release, Threshold};
    static CORNERS: Corners = Corners {
        cells: &[
            (Threshold, -200.0, 20.0),
            (Attack, 0.0, 5.0),
            (Hold, 0.0, 5.0),
            (Release, 0.0, 5.0),
            (Range, -120.0, 12.0),
        ],
        lanes: &[0],
        lane_values: &[HOSTILE_DB],
        bound: 1.5,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        let bus = BusTopology::assumed(ChannelLayout::from(width)).expect("one bus");
        sweep(
            || GateNode::new(bus, KeySource::Sidechain, GateControls::DEFAULT),
            &CORNERS,
            1.5,
            Some(usize::from(width)),
        );
        sweep(
            || GateNode::new(bus, KeySource::Own, GateControls::DEFAULT),
            &CORNERS,
            1.5,
            None,
        );
    }
}

/// The limiter at every corner, at the shortest lookahead (one frame) and the longest (one
/// second: 48 000 frames, so a window of 60 000 frames wraps the ring and a reset lands after
/// it), stereo and 6 wide: the threshold at both ends and as a hostile lane, the ceiling from
/// -60 to 0 dB and as a lane of the same with non-finite frames, the release from 1 ms to 10 s.
/// The output stays within the ceiling's top, 0 dB, to the ulps of the curve's roundings.
#[test]
fn the_limiter_corners_are_allocation_free_finite_and_under_the_ceiling() {
    use tutti_types::UnitParam::{Ceiling, Release, Threshold};
    const HOSTILE_DB_CEILING: &[f32] =
        &[-60.0, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -6.0];
    // Four ulps: the ceiling's amplitude, the divide and the product each round (the
    // figure of `CEILING_ULPS` in the limiter's tests in `tutti-dsp`).
    const BOUND: f32 = 1.0 + 4.0 * f32::EPSILON;
    static CORNERS: Corners = Corners {
        cells: &[
            (Threshold, -200.0, 20.0),
            (Ceiling, -60.0, 0.0),
            (Release, 0.0, 10.0),
        ],
        lanes: &[0, 1],
        lane_values: &[HOSTILE_DB_CEILING],
        bound: BOUND,
        frames: 60_000,
    };
    // Thresholds and ceilings of any size, finite or not, in the cells and in the lanes: the
    // output is finite (the gain is clamped, nothing latches) and within what a ceiling of
    // 1e30 dB is held to.
    static HOSTILE: Corners = Corners {
        cells: &[
            (Threshold, f32::NEG_INFINITY, -800.0),
            (Ceiling, f32::NEG_INFINITY, 1.0e30),
        ],
        lanes: &[0, 1],
        lane_values: &[
            &[-1.0e3, -1.0e6, -1.0e10, 1.0e30, -1.0e30, f32::NAN],
            &[1.0e30, -1.0e30, f32::INFINITY, -758.0, -800.0, 0.0],
        ],
        bound: f32::MAX,
        frames: 60_000,
    };
    // The aimed ceiling is hostile and only the threshold is driven: the ceiling is read from
    // its cell (1e30 dB or `+inf`) and held, where a lane would be.
    static HOSTILE_BASE: Corners = Corners {
        cells: &[(Ceiling, 1.0e30, f32::INFINITY), (Threshold, -200.0, 20.0)],
        lanes: &[0],
        lane_values: &[&[-1.0e3, -1.0e6, -1.0e10, f32::NAN, 20.0]],
        bound: f32::MAX,
        frames: 60_000,
    };
    static INFINITE: Corners = Corners {
        cells: &[
            (Threshold, f32::NEG_INFINITY, f32::INFINITY),
            (Ceiling, f32::NEG_INFINITY, 0.0),
        ],
        lanes: &[],
        lane_values: &[],
        bound: BOUND,
        frames: 60_000,
    };
    for lookahead in [Seconds::new(1e-9), Seconds::new(1.0)] {
        for width in [2u16, 6] {
            let bus = BusTopology::assumed(ChannelLayout::from(width)).expect("one bus");
            for corners in [&CORNERS, &HOSTILE, &HOSTILE_BASE, &INFINITE] {
                sweep(
                    || LimiterNode::new(bus, Lookahead::of(lookahead), LimiterControls::DEFAULT),
                    corners,
                    1.5,
                    None,
                );
            }
        }
    }
}

/// The brickwall at every corner: the ceiling from -60 to -0.3 dB, and as a lane of the same
/// with NaN frames, bounds the output at -0.3 dB; with `+-inf` ceilings it passes or mutes
/// and stays finite.
#[test]
fn the_brickwall_corners_are_allocation_free_finite_and_under_the_ceiling() {
    use tutti_types::UnitParam::Ceiling;
    static CORNERS: Corners = Corners {
        cells: &[(Ceiling, -60.0, -0.3)],
        lanes: &[0],
        lane_values: &[&[-60.0, -0.3, f32::NAN, -6.0]],
        bound: 0.966_052,
        frames: 19_200,
    };
    static EXTREMES: Corners = Corners {
        cells: &[(Ceiling, f32::NEG_INFINITY, f32::INFINITY)],
        lanes: &[0],
        lane_values: &[&[f32::NEG_INFINITY, f32::INFINITY, f32::NAN, 24.0]],
        bound: 1.5,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        let bus = BusTopology::unassigned(ChannelLayout::from(width)).expect("one bus");
        for corners in [&CORNERS, &EXTREMES] {
            sweep(
                || BrickwallLimiterNode::with_bus(bus, Db::new(0.0)),
                corners,
                1.5,
                None,
            );
        }
    }
}
