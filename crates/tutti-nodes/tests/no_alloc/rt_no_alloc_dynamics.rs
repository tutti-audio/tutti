//! The dynamics nodes' allocation gates beyond the table's held rows: the compressor with its
//! control signals fed and its cells moving, the lookahead limiter through a burst and its
//! release (measured from the first block, where the window-minimum deque fills to its whole
//! window), the brickwall and limiter fed and cloned mid-release, and every node's corners.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::contract::BlockRig;
use tutti_nodes::{
    BrickwallLimiterConfig, BrickwallLimiterNode, CompressorConfig, CompressorControls,
    CompressorNode, GateConfig, GateControls, GateNode, KeySource, LimiterConfig, LimiterControls,
    LimiterNode, Lookahead, MultibandBands, MultibandCompressorConfig, MultibandCompressorNode,
};
use tutti_units::{CompressionRatio, Db, SampleRate, Seconds};

/// The compressor's control signal path with every cell moving too: the threshold port fed a per-frame
/// ramp with a NaN frame (the `ControlSignals` control signal the kernel reads per frame), and each block
/// writes the ratio, the times, the knee and the makeup (a new design on the stream grid, the
/// makeup ramping) through the `ParamSet`, 6 channels, 256 blocks.
#[test]
fn compressor_fed_threshold_and_moving_cells_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_units::UnitParam;

    let mut node = Direct::new(
        CompressorNode::<f32>::from(
            CompressorConfig::new(
                tutti_buffers::ChannelTopology::default_for(tutti_buffers::ChannelLayout::from(
                    6u16,
                ))
                .expect("at most one bus wide"),
            )
            .key(KeySource::Sidechain)
            .controls(CompressorControls {
                threshold: Db::new(-20.0),
                ratio: CompressionRatio::new(4.0),
                attack: Seconds::new(0.003),
                release: Seconds::new(0.08),
                ..CompressorControls::DEFAULT
            }),
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 0.8 } else { -0.8 };
        }
    }
    let mut signal: Vec<f32> = (0..64).map(|i| -30.0 + 0.3 * i as f32).collect();
    signal[9] = f32::NAN;
    node.feed(0, Some(&signal));
    let set = node.node.param_set();
    for _ in 0..16 {
        node.block();
    }
    for b in 0..256 {
        let k = (b % 40) as f32;
        assert!(set.set(UnitParam::Ratio, f64::from(2.0 + 0.2 * k)));
        assert!(set.set(UnitParam::Attack, f64::from(0.0005 + 0.0002 * k)));
        assert!(set.set(UnitParam::Release, f64::from(0.01 + 0.005 * k)));
        assert!(set.set(UnitParam::Knee, f64::from(0.3 * k)));
        assert!(set.set(UnitParam::GainDb, f64::from(k - 20.0)));
        node.block();
    }
}

/// A lookahead limiter through a burst and its release, every block under
/// `RtScope` from the first. Whenever the envelope rises (from the
/// node's initial zero onto the burst, and through the release after it)
/// every gain is larger than the last, so the window-minimum deque keeps
/// every push and fills to the whole lookahead window. The constant-drive
/// gates above warm up outside the gate, so the one fill that would grow
/// the deque happens before they measure.
#[test]
fn limiter_node_burst_and_release_are_allocation_free() {
    let node = LimiterNode::<f32>::from(LimiterConfig::new(ChannelTopology::STEREO).controls(
        LimiterControls {
            threshold: Db::new(-3.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        },
    ));
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    let fill = |rig: &mut BlockRig, amplitude: f32| {
        for c in rig.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
            for (i, x) in c.iter_mut().enumerate() {
                *x = if i % 2 == 0 { amplitude } else { -amplitude };
            }
        }
    };
    // 0.1 s of release at 48 kHz is 75 blocks of 64: each phase runs well
    // past the 240-frame window, so the deque fills and turns over.
    fill(&mut rig, 4.0);
    for _ in 0..200 {
        rig.block();
    }
    fill(&mut rig, 0.05);
    for _ in 0..200 {
        rig.block();
    }
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the limiter rendered silence after the release"
    );
}

/// The brickwall with its ceiling port fed a per-frame ramp (the `ControlSignals` control signal the
/// kernel reads each frame), 6 channels, 256 blocks.
/// `Direct` feeds the port, which `BlockRig` cannot.
#[test]
fn brickwall_limiter_fed_ceiling_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
            ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        )),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let ceiling: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    node.feed(0, Some(&ceiling));
    node.block();
    let first = tutti_units::Db::new(-12.0).to_amplitude().get() as f32;
    assert!(
        (node.output().plane(5).samples()[0] - first).abs() < 1e-5,
        "the fed ceiling did not reach the clip: {}",
        node.output().plane(5).samples()[0]
    );
    for _ in 0..256 {
        node.block();
    }
}

/// The brickwall's control signal path with the cell moving too, and a control signal carrying every
/// value the clip must survive (NaN, `+-inf`, a mute): each block writes the cell to a
/// new ceiling through the `ParamSet` (a new design, retargeted on the audio thread),
/// 256 blocks.
#[test]
fn brickwall_limiter_moving_cell_and_hostile_signal_are_allocation_free() {
    use tutti_graph::ParamNode;
    use tutti_graph::contract::Direct;
    use tutti_units::UnitParam;

    let mut node = Direct::new(
        BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
            ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
            Db::new(-0.3),
        )),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.5 } else { -1.5 };
        }
    }
    let mut signal: Vec<f32> = (0..64).map(|i| -12.0 + 0.1875 * i as f32).collect();
    (signal[3], signal[4], signal[5], signal[6]) =
        (f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN);
    node.feed(0, Some(&signal));
    let set = node.node.param_set();
    node.block();
    for b in 0..256 {
        let db = -20.0 + (b % 40) as f32 * 0.5;
        assert!(set.set(
            UnitParam::Ceiling,
            if b % 7 == 0 { f64::NAN } else { f64::from(db) }
        ));
        node.block();
    }
}

/// The lookahead limiter with its ceiling and threshold ports fed per frame
/// (the path that reads both per sample instead of through the block's
/// curve), 6 channels, 256 blocks, the first
/// measured after a warm-up that has the limiter reducing.
#[test]
fn limiter_node_fed_ceiling_and_threshold_process_is_allocation_free() {
    use tutti_graph::contract::Direct;

    let mut node = Direct::new(
        LimiterNode::<f32>::from(
            LimiterConfig::new(
                ChannelTopology::default_for(ChannelLayout::from(6u16))
                    .expect("at most one bus wide"),
            )
            .controls(LimiterControls::DEFAULT),
        ),
        SampleRate::SR_48K,
        64,
    );
    for c in node.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
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
        node.output()
            .plane(0)
            .samples()
            .iter()
            .any(|y| y.abs() < 1.0),
        "the limiter was not reducing; the gate would walk no gain curve"
    );
    for _ in 0..256 {
        node.block();
    }
}

/// A lookahead limiter cloned mid-release, and the clone processed on
/// through the rest of the release, with no
/// `prepare` between (which would rebuild the deque at full capacity and
/// hide a short one). At the clone the window-minimum deque holds about
/// one block of a rising run, fewer entries than its window; the clone must
/// still hold the whole window's allocation, or the release grows it on
/// the audio thread.
#[test]
fn a_limiter_cloned_mid_release_processes_allocation_free() {
    use tutti_graph::contract::Direct;

    let node = LimiterNode::<f32>::from(LimiterConfig::new(ChannelTopology::STEREO).controls(
        LimiterControls {
            threshold: Db::new(-3.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        },
    ));
    let mut d = Direct::new(node, SampleRate::SR_48K, 64);
    let fill = |d: &mut Direct<LimiterNode<f32>>, amplitude: f32| {
        for c in d.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
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
    for _ in 0..200 {
        d.block();
    }
    assert!(
        d.output().plane(0).iter().any(|s| s.abs() > 1e-6),
        "the clone rendered silence after the release"
    );
}

/// What a corner sweep drives: the controls a node has at their two ends (a write each block
/// from one end to the other, so a design is in flight on all of them), the ports a control signal
/// can drive, and the bound its output keeps.
pub(crate) struct Corners {
    /// Each control and its two ends.
    pub(crate) cells: &'static [(tutti_units::UnitAddr, f32, f32)],
    /// The param ports that read a control signal per frame.
    pub(crate) signals: &'static [usize],
    /// The control signal values: a control signal cycles through them, so every frame is a corner or hostile.
    pub(crate) signal_values: &'static [&'static [f32]],
    /// The most a sample may reach: the largest ceiling or gain the corners ask for, times
    /// the input.
    pub(crate) bound: f32,
    /// The frames a window renders, at least: enough to fill and wrap the longest ring.
    pub(crate) frames: usize,
}

/// `node` at every corner of `corners`, in blocks of 1, 7, 33 and 64 frames, with the control signals
/// fed none and then each of their value sets: each cell written to an end every block
/// (alternating, then all low, all high), the input a square at `amplitude` with silent
/// stretches (a parking node parks and wakes), a key sample (from channel `key_from`) that is
/// infinite every 50 blocks,
/// and a `reset` inside the window at 90 and 99 percent of it (after a ring of
/// `corners.frames` has wrapped); between windows the node is prepared again and forked. Every output sample is
/// finite and within the bound.
pub(crate) fn sweep<N: tutti_graph::Node + tutti_graph::ParamNode>(
    make: impl Fn() -> N,
    corners: &Corners,
    amplitude: f32,
    key_from: Option<usize>,
) {
    use tutti_buffers::MaxBlock;
    use tutti_graph::Prepare;
    use tutti_graph::contract::Direct;
    use tutti_units::Samples;

    let fill = |d: &mut Direct<N>, amplitude: f32| {
        for c in d.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
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
            .chain(corners.signal_values.iter().copied().map(Some))
            .collect();
        for (v, values) in variants.iter().enumerate() {
            for &port in corners.signals {
                let signal: Option<Vec<f32>> =
                    values.map(|vals| (0..64).map(|i| vals[(i + port) % vals.len()]).collect());
                d.feed(port, signal.as_deref());
            }
            for phase in 0..3usize {
                let (mut finite, mut within) = (true, true);
                fill(&mut d, amplitude);
                for b in 0..blocks {
                    // Silent from 65 to 80 percent of the window: a node that parks
                    // parks and wakes.
                    if b == blocks * 65 / 100 {
                        for c in d.inputs_mut().iter_mut() {
                            let c = c.into_samples_mut();
                            c.fill(0.0);
                        }
                    } else if b == blocks * 80 / 100 {
                        for c in d.inputs_mut().iter_mut() {
                            let c = c.into_samples_mut();
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
                        set.set(addr, if high { f64::from(hi) } else { f64::from(lo) });
                    }
                    if b == blocks * 90 / 100 || b == blocks * 99 / 100 {
                        assert_no_alloc::assert_no_alloc(|| d.node.reset());
                    }
                    if let Some(k) = key_from {
                        let hot = b % 50 == 5 && b < blocks * 65 / 100;
                        let silent = (blocks * 65 / 100..blocks * 80 / 100).contains(&b);
                        for mut c in d.inputs_mut().iter_mut().skip(k) {
                            let c = c.samples_mut();
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
                        for &s in d.output().plane(c).samples() {
                            finite &= s.is_finite();
                            within &= s.abs() <= corners.bound;
                        }
                    }
                }
                assert!(
                    finite,
                    "block {block}, signals {v}, phase {phase}: not finite"
                );
                assert!(
                    within,
                    "block {block}, signals {v}, phase {phase}: past the bound"
                );
            }
            // Between windows: prepared again, and forked, the control side's own work.
            d.node.prepare(&Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(64)),
            ));
            let fork = d.node.fork_fresh();
            *d.node = fork;
            d.node.prepare(&Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(64)),
            ));
            set = d.node.param_set();
        }
    }
}

/// The address of param `kind` at `index` (0 for a node with one of each).
pub(crate) const fn addr(kind: tutti_units::UnitParam, index: u8) -> tutti_units::UnitAddr {
    tutti_units::UnitAddr::new(kind, index)
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
/// both ends and as a hostile control signal, knee, ratio (below 1 and 1000), times (instant and 10 s)
/// and makeup (to a clamped 1e30 dB), with an infinite key sample now and then.
#[test]
fn the_compressor_corners_are_allocation_free_and_finite() {
    use tutti_units::UnitParam::{Attack, GainDb, Knee, Ratio, Release, Threshold};
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Threshold, 0), -200.0, 60.0),
            (addr(Knee, 0), 0.0, 200.0),
            (addr(Ratio, 0), 0.25, 1000.0),
            (addr(Attack, 0), 0.0, 10.0),
            (addr(Release, 0), 0.0, 10.0),
            (addr(GainDb, 0), -120.0, 24.0),
        ],
        signals: &[0],
        signal_values: &[HOSTILE_DB],
        // 24 dB of makeup on a 1.5 square: 23.8.
        bound: 24.0,
        frames: 19_200,
    };
    static CLAMPED: Corners = Corners {
        cells: &[
            (addr(GainDb, 0), -1.0e30, 1.0e30),
            (addr(Ratio, 0), 1.0, 1.0),
        ],
        signals: &[0],
        signal_values: &[HOSTILE_DB],
        // The makeup is held to 400 dB: 1e20 on a 1.5 square, a finite gain.
        bound: 1.0e21,
        frames: 19_200,
    };
    // A knee past 400 dB is held there: its square and its double would overflow.
    static HUGE_KNEE: Corners = Corners {
        cells: &[
            (addr(Knee, 0), 1.0e20, 3.0e38),
            (addr(Threshold, 0), -200.0, 60.0),
            (addr(Ratio, 0), 1.0, 1000.0),
        ],
        signals: &[0],
        signal_values: &[HOSTILE_DB],
        bound: 1.5,
        frames: 19_200,
    };
    for width in [2u16, 6] {
        let bus = ChannelTopology::default_for(ChannelLayout::from(width)).expect("one bus");
        let key = Some(usize::from(width));
        for corners in [&CORNERS, &CLAMPED, &HUGE_KNEE] {
            sweep(
                || {
                    CompressorNode::<f32>::from(
                        CompressorConfig::new(bus)
                            .key(KeySource::Sidechain)
                            .controls(CompressorControls::DEFAULT),
                    )
                },
                corners,
                1.5,
                key,
            );
            sweep(
                || {
                    CompressorNode::<f32>::from(
                        CompressorConfig::new(bus).controls(CompressorControls::DEFAULT),
                    )
                },
                corners,
                1.5,
                None,
            );
        }
    }
}

/// The gate at every corner: the threshold at both ends and as a hostile control signal, times from
/// instant to 5 s (the hold is 240 000 frames), and the range from 120 dB down to above 0 dB,
/// with an infinite key sample now and then.
#[test]
fn the_gate_corners_are_allocation_free_and_finite() {
    use tutti_units::UnitParam::{Attack, Hold, Range, Release, Threshold};
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Threshold, 0), -200.0, 20.0),
            (addr(Attack, 0), 0.0, 5.0),
            (addr(Hold, 0), 0.0, 5.0),
            (addr(Release, 0), 0.0, 5.0),
            (addr(Range, 0), -120.0, 12.0),
        ],
        signals: &[0],
        signal_values: &[HOSTILE_DB],
        bound: 1.5,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        let bus = ChannelTopology::default_for(ChannelLayout::from(width)).expect("one bus");
        sweep(
            || {
                GateNode::<f32>::from(
                    GateConfig::new(bus)
                        .key(KeySource::Sidechain)
                        .controls(GateControls::DEFAULT),
                )
            },
            &CORNERS,
            1.5,
            Some(usize::from(width)),
        );
        sweep(
            || GateNode::<f32>::from(GateConfig::new(bus).controls(GateControls::DEFAULT)),
            &CORNERS,
            1.5,
            None,
        );
    }
}

/// The limiter at every corner, at the shortest lookahead (one frame) and the longest (one
/// second: 48 000 frames, so a window of 60 000 frames wraps the ring and a reset lands after
/// it), stereo and 6 wide: the threshold at both ends and as a hostile control signal, the ceiling from
/// -60 to 0 dB and as a control signal of the same with non-finite frames, the release from 1 ms to 10 s.
/// The output stays within the ceiling's top, 0 dB, to the ulps of the curve's roundings.
#[test]
fn the_limiter_corners_are_allocation_free_finite_and_under_the_ceiling() {
    use tutti_units::UnitParam::{Ceiling, Release, Threshold};
    const HOSTILE_DB_CEILING: &[f32] =
        &[-60.0, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -6.0];
    // Four ulps: the ceiling's amplitude, the divide and the product each round (the
    // figure of `CEILING_ULPS` in the limiter's tests in `tutti-dsp`).
    const BOUND: f32 = 1.0 + 4.0 * f32::EPSILON;
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Threshold, 0), -200.0, 20.0),
            (addr(Ceiling, 0), -60.0, 0.0),
            (addr(Release, 0), 0.0, 10.0),
        ],
        signals: &[0, 1],
        signal_values: &[HOSTILE_DB_CEILING],
        bound: BOUND,
        frames: 60_000,
    };
    // Thresholds and ceilings of any size, finite or not, in the cells and in the control signals: the
    // output is finite (the gain is clamped, nothing latches) and within what a ceiling of
    // 1e30 dB is held to.
    static HOSTILE: Corners = Corners {
        cells: &[
            (addr(Threshold, 0), f32::NEG_INFINITY, -800.0),
            (addr(Ceiling, 0), f32::NEG_INFINITY, 1.0e30),
        ],
        signals: &[0, 1],
        signal_values: &[
            &[-1.0e3, -1.0e6, -1.0e10, 1.0e30, -1.0e30, f32::NAN],
            &[1.0e30, -1.0e30, f32::INFINITY, -758.0, -800.0, 0.0],
        ],
        bound: f32::MAX,
        frames: 60_000,
    };
    // The set ceiling is hostile and only the threshold is driven: the ceiling is read from
    // its cell (1e30 dB or `+inf`) and held, where a control signal would be.
    static HOSTILE_BASE: Corners = Corners {
        cells: &[
            (addr(Ceiling, 0), 1.0e30, f32::INFINITY),
            (addr(Threshold, 0), -200.0, 20.0),
        ],
        signals: &[0],
        signal_values: &[&[-1.0e3, -1.0e6, -1.0e10, f32::NAN, 20.0]],
        bound: f32::MAX,
        frames: 60_000,
    };
    static INFINITE: Corners = Corners {
        cells: &[
            (addr(Threshold, 0), f32::NEG_INFINITY, f32::INFINITY),
            (addr(Ceiling, 0), f32::NEG_INFINITY, 0.0),
        ],
        signals: &[],
        signal_values: &[],
        bound: BOUND,
        frames: 60_000,
    };
    for lookahead in [Seconds::new(1e-9), Seconds::new(1.0)] {
        for width in [2u16, 6] {
            let bus = ChannelTopology::default_for(ChannelLayout::from(width)).expect("one bus");
            for corners in [&CORNERS, &HOSTILE, &HOSTILE_BASE, &INFINITE] {
                // A 1 ns lookahead has no ring to wrap: the shorter window covers it.
                let corners = Corners {
                    frames: if lookahead == Seconds::new(1.0) {
                        corners.frames
                    } else {
                        19_200
                    },
                    cells: corners.cells,
                    signals: corners.signals,
                    signal_values: corners.signal_values,
                    bound: corners.bound,
                };
                sweep(
                    || {
                        LimiterNode::<f32>::from(
                            LimiterConfig::new(bus)
                                .lookahead(Lookahead::of(lookahead))
                                .controls(LimiterControls::DEFAULT),
                        )
                    },
                    &corners,
                    1.5,
                    None,
                );
            }
        }
    }
}

/// The brickwall at every corner: the ceiling from -60 to -0.3 dB, and as a control signal of the same
/// with NaN frames, bounds the output at -0.3 dB; with `+-inf` ceilings it passes or mutes
/// and stays finite.
#[test]
fn the_brickwall_corners_are_allocation_free_finite_and_under_the_ceiling() {
    use tutti_units::UnitParam::Ceiling;
    static CORNERS: Corners = Corners {
        cells: &[(addr(Ceiling, 0), -60.0, -0.3)],
        signals: &[0],
        signal_values: &[&[-60.0, -0.3, f32::NAN, -6.0]],
        bound: 0.966_052,
        frames: 19_200,
    };
    static EXTREMES: Corners = Corners {
        cells: &[(addr(Ceiling, 0), f32::NEG_INFINITY, f32::INFINITY)],
        signals: &[0],
        signal_values: &[&[f32::NEG_INFINITY, f32::INFINITY, f32::NAN, 24.0]],
        bound: 1.5,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        let bus = ChannelTopology::unassigned(ChannelLayout::from(width)).expect("one bus");
        for corners in [&CORNERS, &EXTREMES] {
            sweep(
                || {
                    BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                        bus,
                        Db::new(0.0),
                    ))
                },
                corners,
                1.5,
                None,
            );
        }
    }
}

/// A multiband's band cells and crossovers at their two ends: each of four bands' threshold,
/// knee, ratio, times, makeup and both toggles, then three crossovers at the ends in
/// `cutoffs`. A node of fewer bands answers to the first of them.
const fn multiband_cells(cutoffs: [(f32, f32); 3]) -> [(tutti_units::UnitAddr, f32, f32); 35] {
    use tutti_units::UnitAddr;
    use tutti_units::UnitParam::{
        Attack, Bypass, Cutoff, GainDb, Knee, Ratio, Release, Solo, Threshold,
    };
    let per_band = [
        (Threshold, -200.0, 60.0),
        (Knee, 0.0, 200.0),
        (Ratio, 0.25, 1000.0),
        (Attack, 0.0, 10.0),
        (Release, 0.0, 10.0),
        (GainDb, -120.0, 24.0),
        (Bypass, 0.0, 1.0),
        (Solo, 0.0, 1.0),
    ];
    let mut cells = [(UnitAddr::new(Threshold, 0), 0.0, 0.0); 35];
    let mut b = 0;
    while b < 4 {
        let mut k = 0;
        while k < 8 {
            let (kind, lo, hi) = per_band[k];
            cells[b * 8 + k] = (UnitAddr::new(kind, b as u8), lo, hi);
            k += 1;
        }
        b += 1;
    }
    let mut i = 0;
    while i < 3 {
        cells[32 + i] = (UnitAddr::new(Cutoff, i as u8), cutoffs[i].0, cutoffs[i].1);
        i += 1;
    }
    cells
}

/// The multiband at every corner, 2 to 4 bands over widths 1, 2 and 6: every band's threshold,
/// knee, ratio, times, makeup, bypass and solo at both ends and alternating; the splits moving
/// the whole range in order, then at 0, at Nyquist and past it, infinite and out of order (a
/// set that is refused leaves the last good one standing); the first two bands' thresholds
/// as hostile control signals.
#[test]
fn the_multiband_corners_are_allocation_free_and_finite() {
    static MOVING: Corners = Corners {
        cells: &multiband_cells([(20.0, 300.0), (400.0, 3_000.0), (4_000.0, 23_000.0)]),
        signals: &[0, 6],
        signal_values: &[HOSTILE_DB],
        // Four bands of up to 24 dB of makeup on a 1.5 square, with the overshoot of a
        // moving crossover: a bound the corners stay far under.
        bound: 1.0e6,
        frames: 6_400,
    };
    static HOSTILE: Corners = Corners {
        cells: &multiband_cells([
            (0.0, 24_000.0),
            (100.0, 20_000.0),
            (f32::NEG_INFINITY, f32::INFINITY),
        ]),
        signals: &[0, 6],
        signal_values: &[HOSTILE_DB],
        bound: 1.0e6,
        frames: 6_400,
    };
    static CLAMPED: Corners = Corners {
        cells: &[
            (addr(tutti_units::UnitParam::GainDb, 0), -1.0e30, 1.0e30),
            (
                addr(tutti_units::UnitParam::GainDb, 1),
                f32::NAN,
                f32::INFINITY,
            ),
            (addr(tutti_units::UnitParam::Ratio, 0), 1.0, 1.0),
        ],
        signals: &[0, 6],
        signal_values: &[HOSTILE_DB],
        // The makeup is held to 400 dB: 1e20 on a 1.5 square, a finite gain.
        bound: 1.0e30,
        frames: 6_400,
    };
    // A knee past 400 dB is held there, on every band.
    static HUGE_KNEE: Corners = Corners {
        cells: &[
            (addr(tutti_units::UnitParam::Knee, 0), 1.0e20, 3.0e38),
            (addr(tutti_units::UnitParam::Knee, 1), 3.0e38, 1.0e20),
            (addr(tutti_units::UnitParam::Knee, 2), 1.0e20, 1.0e20),
            (addr(tutti_units::UnitParam::Knee, 3), 3.0e38, 3.0e38),
            (addr(tutti_units::UnitParam::Threshold, 0), -200.0, 60.0),
            (addr(tutti_units::UnitParam::Ratio, 0), 1.0, 1000.0),
        ],
        signals: &[0, 6],
        signal_values: &[HOSTILE_DB],
        bound: 1.0e6,
        frames: 6_400,
    };
    for bands in 2..=4 {
        for width in [1u16, 2, 6] {
            for corners in [&MOVING, &HOSTILE, &CLAMPED, &HUGE_KNEE] {
                sweep(
                    || {
                        MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
                            ChannelTopology::default_for(ChannelLayout::from(width)).unwrap(),
                            MultibandBands::new(bands).unwrap(),
                        ))
                    },
                    corners,
                    1.5,
                    None,
                );
            }
        }
    }
}
