//! **What does one block of each per-sample kernel cost, at 64 and 1024
//! frames?**
//!
//! The nodes here are the state-variable filter (held and gliding, at widths 1, 2 and 6, in
//! blocks of 64 and 512), the waveshaper, the Moog ladder, the brickwall clip,
//! the width adapter, the dynamics (the lookahead limiter, the gate and the
//! compressor) and the ring-buffer family (the feedback delay, the lookahead
//! limiter's ring and the convolver's dry
//! alignment), and one graph, the fan-in sum (the op, through the graph). Each case
//! drives one node through `process` with whole blocks
//! (`tutti_graph::contract::Direct`) over broadband noise, after a few
//! warm-up blocks. A `fed` case feeds the node's first param port per frame,
//! as modulation does.
//!
//! Throughput is in channel-samples (frames times the wider side's width), so
//! rows of different widths compare per sample.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench node_kernels
//! ```

use std::hint::black_box;
use tutti_buffers::{ChannelLayout, ChannelTopology, Float, MaxBlock};
use tutti_dsp::LadderSaturation;
use tutti_graph::EdgeId;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::Node;
use tutti_graph::contract::Direct;
use tutti_nodes::{
    BrickwallLimiterConfig, BrickwallLimiterNode, ChannelTimes, CompressorConfig,
    CompressorControls, CompressorNode, DelayConfig, DelayControls, DelayLayout, DelayNode,
    DistortionConfig, DistortionNode, FdnConfig, FdnNode, FreeverbConfig, FreeverbNode, GateConfig,
    GateControls, GateNode, KeySource, LadderFilterConfig, LadderFilterNode, LadderType,
    LimiterConfig, LimiterControls, LimiterNode, MaxDelay, ShapeKind, SvfFilterConfig,
    SvfFilterNode, SvfType, TubeStageConfig, TubeStageNode, Waveshape,
};
use tutti_rt::SplitMix64;
use tutti_units::{
    CompressionRatio, Db, Drive, Feedback, Hz, Q, Resonance, SampleRate, Samples, Seconds,
};

const BLOCKS: [usize; 2] = [64, 1024];
const SVF_BLOCKS: [usize; 2] = [64, 512];
const SR: SampleRate = SampleRate::SR_48K;

/// Broadband input from a seeded `SplitMix64`.
fn fill_noise<N: Node>(d: &mut Direct<N>) {
    let mut rng = SplitMix64::new(1);
    for ch in d.inputs_mut().iter_mut() {
        let ch = ch.into_samples_mut();
        for s in ch.iter_mut() {
            *s = f32::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0;
        }
    }
}

/// Bench `make()` as `group/case/block` at every block size, `fed` (built
/// for the block length) on its first param port.
fn run<N: Node>(
    c: &mut Criterion,
    group: &str,
    case: &str,
    make: impl Fn() -> N,
    fed: Option<fn(usize) -> Vec<f32>>,
) {
    run_blocks(c, group, case, &BLOCKS, make, fed);
}

/// [`run`] over `blocks`.
fn run_blocks<N: Node>(
    c: &mut Criterion,
    group: &str,
    case: &str,
    blocks: &[usize],
    make: impl Fn() -> N,
    fed: Option<fn(usize) -> Vec<f32>>,
) {
    let mut g = c.benchmark_group(group);
    for &block in blocks {
        let mut d = Direct::new(make(), SR, block);
        let shape = d.node.shape();
        let width = shape.audio_in().count().max(shape.audio_out().count()) as usize;
        g.throughput(Throughput::Elements((block * width) as u64));
        fill_noise(&mut d);
        let values = fed.map(|f| f(block));
        if let Some(v) = &values {
            d.feed(0, Some(v));
        }
        for _ in 0..8 {
            d.block();
        }
        g.bench_function(BenchmarkId::new(case, block), |b| {
            b.iter(|| {
                d.block();
                black_box(d.output().plane(0).samples()[block - 1]);
            })
        });
    }
    g.finish();
}

/// The graph's fan-in (`Op::Sum`), measured
/// through the graph: `sources` groups of `width` main input channels, each
/// output channel summing its channel of every group. No node runs, so the
/// figure is the op's.
fn channel_sum(c: &mut Criterion) {
    use tutti_buffers::ChannelLayout;
    use tutti_graph::{Editor, PortSource, Prepare, TransportPosition};
    use tutti_units::Samples;

    let mut g = c.benchmark_group("channel_sum");
    for (case, sources, width) in [("8x2", 8u16, 2u16), ("16x1", 16, 1), ("2x6", 2, 6)] {
        for block in BLOCKS {
            let (mut ed, mut exec) =
                Editor::new(Prepare::new(SR, MaxBlock::of(Samples::new(block))));
            ed.resize_inputs(ChannelLayout::from(sources * width));
            ed.wire_outputs(vec![PortSource::Zero; width as usize])
                .unwrap();
            for channel in 0..width {
                for k in 0..sources {
                    ed.connect_edge(EdgeId::into_output(
                        (channel as usize) as u16,
                        PortSource::MainInput(k * width + channel),
                    ))
                    .unwrap();
                }
            }
            ed.commit().expect("commits");
            exec.apply_pending();
            ed.collect();

            let mut rng = SplitMix64::new(1);
            let input: Vec<Vec<f32>> = (0..sources * width)
                .map(|_| {
                    (0..block)
                        .map(|_| f32::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0)
                        .collect()
                })
                .collect();
            let input: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut out = vec![vec![0.0f32; block]; width as usize];
            let t = TransportPosition::default();
            g.throughput(Throughput::Elements((block * width as usize) as u64));
            g.bench_function(BenchmarkId::new(case, block), |b| {
                b.iter(|| {
                    let mut outs: Vec<&mut [f32]> = out.iter_mut().map(Vec::as_mut_slice).collect();
                    exec.process(block, &t, black_box(&input), &mut outs);
                    black_box(outs[0][block - 1]);
                })
            });
        }
    }
    g.finish();
}

/// A drive that moves every frame, between 1 and 4.
fn drive_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| 1.0 + 3.0 * i as f32 / n as f32).collect()
}

fn distortion(c: &mut Criterion) {
    run(
        c,
        "distortion",
        "tanh_2",
        || {
            DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(ShapeKind::Tanh))
                    .drive(Drive::new(2.0)),
            )
        },
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2",
        || {
            DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(ShapeKind::Softsign))
                    .drive(Drive::new(2.0)),
            )
        },
        None,
    );
    for (case, kind) in [
        ("atan_2", ShapeKind::Atan),
        ("crush_2", ShapeKind::Crush),
        ("softcrush_2", ShapeKind::SoftCrush),
    ] {
        run(
            c,
            "distortion",
            case,
            || {
                DistortionNode::<f32>::from(
                    DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(kind))
                        .drive(Drive::new(2.0)),
                )
            },
            None,
        );
    }
    run(
        c,
        "distortion",
        "hardclip_6",
        || {
            DistortionNode::<f32>::from(
                DistortionConfig::new(6u16, Waveshape::new(ShapeKind::HardClip))
                    .drive(Drive::new(2.0)),
            )
        },
        None,
    );
    run(
        c,
        "distortion",
        "softsign_2_fed",
        || {
            DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, Waveshape::new(ShapeKind::Softsign))
                    .drive(Drive::new(2.0)),
            )
        },
        Some(drive_sweep),
    );
}

/// The 24 dB ladder at 1 kHz with resonance 0.7 (held), two channels, in each saturation model.
fn ladder(c: &mut Criterion) {
    for (case, saturation) in [
        ("lp24_res07_2", LadderSaturation::PerStage),
        ("lp24_res07_2_input", LadderSaturation::Input),
    ] {
        run(
            c,
            "ladder",
            case,
            || {
                LadderFilterNode::<f32>::from(
                    LadderFilterConfig::new(ChannelLayout::STEREO, LadderType::LP24)
                        .freq(Hz::new(1_000.0))
                        .res(Resonance::new(0.7))
                        .saturation(saturation),
                )
            },
            None,
        );
    }
}

/// A ceiling that moves every frame, between -6 and -1 dB.
fn ceiling_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| -6.0 + 5.0 * i as f32 / n as f32).collect()
}

fn brickwall(c: &mut Criterion) {
    run(
        c,
        "brickwall",
        "2",
        || {
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::STEREO,
                Db::new(-3.0),
            ))
        },
        None,
    );
    run(
        c,
        "brickwall",
        "6",
        || {
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
                Db::new(-3.0),
            ))
        },
        None,
    );
    run(
        c,
        "brickwall",
        "6_fed",
        || {
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
                Db::new(-3.0),
            ))
        },
        Some(ceiling_sweep),
    );
}

/// A quarter-second delay at 0.4 feedback, with no cross-feedback.
fn delay_at_quarter() -> DelayControls {
    DelayControls {
        time: Seconds::new(0.25),
        feedback: Feedback::new(0.4),
        ..DelayControls::DEFAULT
    }
}

fn delay(c: &mut Criterion) {
    run(
        c,
        "delay",
        "1",
        || {
            DelayNode::<f32>::from(
                DelayConfig::new(DelayLayout::new(
                    ChannelTopology::MONO,
                    MaxDelay::of(Seconds::new(1.0)),
                ))
                .controls(delay_at_quarter()),
            )
        },
        None,
    );
    run(
        c,
        "delay",
        "6",
        || {
            DelayNode::<f32>::from(
                DelayConfig::new(DelayLayout::new(
                    ChannelTopology::default_for(ChannelLayout::from(6u16))
                        .expect("at most one bus wide"),
                    MaxDelay::of(Seconds::new(1.0)),
                ))
                .controls(delay_at_quarter()),
            )
        },
        None,
    );
    run(
        c,
        "delay",
        "2_cross",
        || {
            let layout =
                DelayLayout::builder(ChannelTopology::STEREO, MaxDelay::of(Seconds::new(1.0)))
                    .times(ChannelTimes::try_from(&[Seconds::new(0.3)][..]).expect("a time"))
                    .build()
                    .unwrap();
            DelayNode::<f32>::from(DelayConfig::new(layout).controls(DelayControls {
                cross_feedback: Feedback::new(0.3),
                ..delay_at_quarter()
            }))
        },
        None,
    );
}

/// A cutoff that moves every frame, between 200 Hz and 4 kHz.
fn cutoff_sweep(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 200.0 + 3_800.0 * i as f32 / n as f32)
        .collect()
}

/// The low-pass SVF at 1 kHz, held and with the cutoff gliding per frame, at widths 1, 2 and 6.
fn svf(c: &mut Criterion) {
    for width in [1u16, 2, 6] {
        let make = move || {
            SvfFilterNode::<f32>::from(
                SvfFilterConfig::new(width, SvfType::LowPass)
                    .freq(Hz::new(1_000.0))
                    .q(Q::new(0.707)),
            )
        };
        run_blocks(c, "svf", &format!("held_{width}"), &SVF_BLOCKS, make, None);
        run_blocks(
            c,
            "svf",
            &format!("gliding_{width}"),
            &SVF_BLOCKS,
            make,
            Some(cutoff_sweep),
        );
    }
}

fn limiter(c: &mut Criterion) {
    run(
        c,
        "limiter",
        "2",
        || {
            LimiterNode::<f32>::from(
                LimiterConfig::new(ChannelTopology::STEREO).controls(LimiterControls::DEFAULT),
            )
        },
        None,
    );
    run(
        c,
        "limiter",
        "6",
        || {
            LimiterNode::<f32>::from(
                LimiterConfig::new(
                    ChannelTopology::default_for(ChannelLayout::from(6u16))
                        .expect("at most one bus wide"),
                )
                .controls(LimiterControls::DEFAULT),
            )
        },
        None,
    );
    // Over the threshold on every frame, and not at all: the two branches of
    // the gain curve.
    run(
        c,
        "limiter",
        "2_quiet",
        || {
            LimiterNode::<f32>::from(LimiterConfig::new(ChannelTopology::STEREO).controls(
                LimiterControls {
                    threshold: Db::new(6.0),
                    ceiling: Db::new(6.0),
                    ..LimiterControls::DEFAULT
                },
            ))
        },
        None,
    );
    run(
        c,
        "limiter",
        "2_fed",
        || {
            LimiterNode::<f32>::from(
                LimiterConfig::new(ChannelTopology::STEREO).controls(LimiterControls::DEFAULT),
            )
        },
        Some(threshold_sweep),
    );
}

/// A threshold that moves every frame, between -30 and -10 dB.
fn threshold_sweep(n: usize) -> Vec<f32> {
    (0..n).map(|i| -30.0 + 20.0 * i as f32 / n as f32).collect()
}

/// The gate the dynamics rows run: -12 dB, 1 ms, 10 ms, 100 ms, closing to -80 dB.
fn gate_controls() -> GateControls {
    GateControls {
        threshold: Db::new(-12.0),
        attack: Seconds::new(0.001),
        hold: Seconds::new(0.01),
        release: Seconds::new(0.1),
        range: Db::new(-80.0),
        ..GateControls::DEFAULT
    }
}

/// The compressor the dynamics rows run: -20 dB, 4:1, 1 ms, 100 ms.
fn compressor_controls() -> CompressorControls {
    CompressorControls {
        threshold: Db::new(-20.0),
        ratio: CompressionRatio::new(4.0),
        attack: Seconds::new(0.001),
        release: Seconds::new(0.1),
        ..CompressorControls::DEFAULT
    }
}

fn dynamics(c: &mut Criterion) {
    run(
        c,
        "gate",
        "2",
        || {
            GateNode::<f32>::from(
                GateConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(gate_controls()),
            )
        },
        None,
    );
    run(
        c,
        "gate",
        "6",
        || {
            GateNode::<f32>::from(
                GateConfig::new(
                    ChannelTopology::default_for(ChannelLayout::from(6u16))
                        .expect("at most one bus wide"),
                )
                .key(KeySource::Sidechain)
                .controls(gate_controls()),
            )
        },
        None,
    );
    run(
        c,
        "gate",
        "2_fed",
        || {
            GateNode::<f32>::from(
                GateConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(gate_controls()),
            )
        },
        Some(threshold_sweep),
    );
    run(
        c,
        "compressor",
        "2",
        || {
            CompressorNode::<f32>::from(
                CompressorConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(compressor_controls()),
            )
        },
        None,
    );
    run(
        c,
        "compressor",
        "6",
        || {
            CompressorNode::<f32>::from(
                CompressorConfig::new(
                    ChannelTopology::default_for(ChannelLayout::from(6u16))
                        .expect("at most one bus wide"),
                )
                .key(KeySource::Sidechain)
                .controls(compressor_controls()),
            )
        },
        None,
    );
    run(
        c,
        "compressor",
        "2_fed",
        || {
            CompressorNode::<f32>::from(
                CompressorConfig::new(ChannelTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(compressor_controls()),
            )
        },
        Some(threshold_sweep),
    );
}

/// The reverb: one input, two outputs, so the throughput counts two channels.
fn reverb(c: &mut Criterion) {
    run(
        c,
        "reverb",
        "freeverb",
        || FreeverbNode::<f32>::from(FreeverbConfig::new()),
        None,
    );
    run(
        c,
        "reverb",
        "fdn",
        || FdnNode::<f32>::from(FdnConfig::new().layout(tutti_nodes::FdnLayout::default())),
        None,
    );
    fdn_retarget_rising(c);
}

/// A retarget of the FDN bank on a decay profile that rises with frequency (the mid band 2 s
/// over a 0.05 s low band): the design searches the response's peak, which a monotone profile
/// does not. The size alternates so every call designs.
fn fdn_retarget_rising(c: &mut Criterion) {
    use tutti_dsp::{FdnBank, FdnControls, FdnLayout, FreezeControls};

    let mut bank = FdnBank::<f32, FdnLayout>::new(&FdnLayout::default(), SR);
    let mut ramps = bank.ramps();
    let mut controls = FdnControls::DEFAULT;
    controls.decay = [Seconds::new(2.0), Seconds::new(0.05), Seconds::new(2.0)];
    let mut sizes = [0.5, 0.51].into_iter().cycle();
    let mut g = c.benchmark_group("reverb");
    g.bench_function("fdn_retarget_rising", |b| {
        b.iter(|| {
            controls.size = tutti_units::Mix::new(sizes.next().unwrap_or(0.5));
            bank.retarget(&mut ramps, black_box(&controls), &FreezeControls::DEFAULT);
        })
    });
    g.finish();
}
/// The tube stage's solve at the default 4 times oversampling, over noise at the default drive.
fn tube_stage(c: &mut Criterion) {
    run(
        c,
        "tube_stage",
        "stereo_x4",
        || TubeStageNode::<f32>::from(TubeStageConfig::new(ChannelLayout::STEREO)),
        None,
    );
}
fn convolver(c: &mut Criterion) {
    use tutti_buffers::Mono;
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(256, Seconds::new(0.05), SR);
    run(
        c,
        "convolver",
        "1",
        || mono_node(Mono::new(&ir), Samples::new(64)),
        None,
    );
}
criterion_group!(
    benches,
    channel_sum,
    distortion,
    ladder,
    brickwall,
    delay,
    svf,
    limiter,
    dynamics,
    reverb,
    tube_stage,
    convolver
);
criterion_main!(benches);

/// A one-channel convolver over `ir`.
fn mono_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    ir: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> tutti_nodes::ConvolverNode<F> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(tutti_buffers::ChannelTopology::MONO, &[ir])
            .block_size(block_size),
    )
    .expect("one response on a mono bus")
}
