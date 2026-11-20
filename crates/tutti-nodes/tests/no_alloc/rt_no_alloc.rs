//! Regression gate for RT-safety: built-in nodes must not allocate on the
//! process path. The panner tests are `spatial_rt_no_alloc.rs`.

use tutti_buffers::{Mono, MonoMut};
use tutti_nodes::ConvolverNode;
use tutti_units::Drive;
use tutti_units::{SampleRate, Samples, Seconds};

/// A node through `BlockRig`, silent inputs: 16 warm-up blocks (past
/// the first FFT partition, so `process` takes the hot path), then 1 000
/// blocks under `assert_no_alloc`. Its `prepare` (which sizes the fold's
/// scratch) runs when the rig is built, outside the gate.
fn gate<N: tutti_graph::IntoNode>(node: N) {
    gate_at(node, 64);
}

/// [`gate`] at a host block of `block` frames.
fn gate_at<N: tutti_graph::IntoNode>(node: N, block: usize) {
    let (mut rig, _controls) =
        tutti_graph::contract::BlockRig::new(node, tutti_units::SampleRate::SR_48K, block);
    for _ in 0..16 {
        rig.block();
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            rig.block();
        }
    });
}
#[test]
fn convolver_process_is_allocation_free() {
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(2048, Seconds::new(0.3), SampleRate::SR_48K);
    gate(mono_node(Mono::new(&ir), Samples::new(512)));
}
#[test]
fn fir_convolver_process_is_allocation_free() {
    use tutti_dsp::{LinearPhaseDesign, MatchedBell};
    use tutti_nodes::FirConvolverConfig;
    use tutti_units::{Db, Hz, Q};

    let bell = MatchedBell::new(Q::new(1.0), Db::new(6.0))
        .design(Hz::new(1_000.0), SampleRate::SR_48K)
        .unwrap();
    let fir = LinearPhaseDesign::new(Hz::new(100.0))
        .design(SampleRate::SR_48K, &[bell])
        .unwrap();
    gate(
        ConvolverNode::<f32>::try_from(
            FirConvolverConfig::new(tutti_buffers::BusTopology::STEREO, &fir)
                .block_size(Samples::new(64)),
        )
        .unwrap(),
    );
}
#[test]
fn dual_mono_convolver_process_is_allocation_free() {
    use tutti_fixtures::signal::decay_ir;

    let ir_l = decay_ir(2048, Seconds::new(0.3), SampleRate::SR_48K);
    let ir_r = decay_ir(2048, Seconds::new(0.4), SampleRate::SR_48K);
    gate(per_channel_node(&[Mono::new(&ir_l), Mono::new(&ir_r)], Samples::new(512)).unwrap());
}

/// The incremental accumulation runs in every block between two partition
/// boundaries, so it is gated at host blocks that divide the partition, ones
/// that do not (37, 100) and one longer than a partition (1000 against 256).
#[test]
fn convolver_mid_cycle_blocks_are_allocation_free() {
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(8192, Seconds::new(0.3), SampleRate::SR_48K);
    for block in [37usize, 100, 128, 1000] {
        gate_at(mono_node(Mono::new(&ir), Samples::new(256)), block);
    }
}

/// The bare convolver with call lengths from 1 to past two partitions, in an
/// order that lands calls mid-cycle, across the boundary and exactly on it,
/// over a signal that is not silent.
#[test]
fn convolver_ragged_calls_are_allocation_free() {
    use tutti_dsp::Convolver;
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(8192, Seconds::new(0.3), SampleRate::SR_48K);
    let mut c = Convolver::new(Mono::new(&ir), Samples::new(128));
    let x: Vec<f32> = (0..513).map(|i| ((i as f32) * 0.31).sin()).collect();
    let mut y = vec![0.0f32; 513];
    let lens = [1usize, 7, 33, 100, 127, 128, 129, 255, 513, 64];
    for k in 0..40 {
        let n = lens[k % lens.len()];
        c.process(Mono::new(&x[..n]), MonoMut::new(&mut y[..n]));
    }
    assert_no_alloc::assert_no_alloc(|| {
        for k in 0..3_000 {
            let n = lens[k % lens.len()];
            c.process(Mono::new(&x[..n]), MonoMut::new(&mut y[..n]));
        }
    });
}

/// A 5 s IR at a 512 head, which the cost model cuts into a head and tail
/// levels with their own transforms, under ragged calls from 1 frame to past
/// the longest tail partition, over a signal that is not silent. The run is
/// long enough for every level to complete many blocks, and it lands calls on
/// and across each level's forward and inverse frames.
#[test]
fn convolver_tail_levels_are_allocation_free() {
    use tutti_dsp::Convolver;
    use tutti_fixtures::signal::decay_ir;

    let ir = decay_ir(240_000, Seconds::new(1.0), SampleRate::SR_48K);
    let mut c = Convolver::new(Mono::new(&ir), Samples::new(512));
    let x: Vec<f32> = (0..5000).map(|i| ((i as f32) * 0.31).sin()).collect();
    let mut y = vec![0.0f32; 5000];
    let lens = [1usize, 64, 64, 64, 333, 512, 700, 2049, 4999, 64, 127, 3];
    for k in 0..60 {
        let n = lens[k % lens.len()];
        c.process(Mono::new(&x[..n]), MonoMut::new(&mut y[..n]));
    }
    assert_no_alloc::assert_no_alloc(|| {
        for k in 0..1_500 {
            let n = lens[k % lens.len()];
            c.process(Mono::new(&x[..n]), MonoMut::new(&mut y[..n]));
        }
    });
}

/// The fold path is the one with scratch: a width-long frame buffer and a
/// block-long mono buffer, both sized before the first block (the frame at
/// construction, the mono block at `prepare`), never grown in `process`. Six
/// channels, so the fold does real work rather than the stereo average.
#[test]
fn folded_six_channel_convolver_process_is_allocation_free() {
    use tutti_fixtures::signal::decay_ir;

    let irs: Vec<Vec<f32>> = (0..6)
        .map(|c| {
            decay_ir(
                1024,
                tutti_units::Seconds::new(0.2 + 0.05 * c as f64),
                SampleRate::SR_48K,
            )
        })
        .collect();
    let refs: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
    gate(folded_node(&refs, Samples::new(256)).unwrap());
}

/// Every curve of the shaper, plain and at every oversampling factor, unfed and with its drive
/// port fed a per-frame ramp, 1 000 blocks each under `assert_no_alloc`. `Direct` feeds the
/// port, which `BlockRig` cannot.
#[test]
fn distortion_process_is_allocation_free_fed_and_unfed() {
    use tutti_graph::contract::Direct;
    use tutti_nodes::{DistortionNode, OversamplingFactor, ShapeKind};
    use tutti_units::SampleRate;

    let kinds = [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ];
    let drive: Vec<f32> = (0..64).map(|i| 1.0 + 0.0625 * i as f32).collect();
    let factors = [
        None,
        Some(OversamplingFactor::X2),
        Some(OversamplingFactor::X4),
        Some(OversamplingFactor::X8),
    ];
    for (kind, factor) in kinds.into_iter().flat_map(|k| factors.map(|f| (k, f))) {
        // The unfed render's first block, kept to prove the fed arm ran.
        let mut unfed: Vec<Vec<f32>> = Vec::new();
        for fed in [false, true] {
            let node = match factor {
                None => DistortionNode::<f32>::with_channels(3u16, kind, Drive::new(2.0)),
                Some(f) => DistortionNode::<f32>::with_channels(3u16, kind, Drive::new(2.0))
                    .oversampling(f),
            };
            let mut node = Direct::new(node, SampleRate::SR_48K, 64);
            assert_eq!(
                tutti_graph::Node::shape(&*node.node)
                    .latency
                    .samples()
                    .get()
                    > 0,
                factor.is_some(),
                "{kind:?} {factor:?}: oversampling must be in force where a factor is set"
            );
            for (c, x) in node.inputs_mut().iter_mut().enumerate() {
                let x = x.into_samples_mut();
                for (i, s) in x.iter_mut().enumerate() {
                    *s = 0.9 * (((i + 7 * c) % 13) as f32 / 6.0 - 1.0);
                }
            }
            if fed {
                node.feed(0, Some(&drive));
            }
            node.block();
            assert!(
                (0..3).all(|c| node
                    .output()
                    .plane(c)
                    .samples()
                    .iter()
                    .any(|s| s.abs() > 1e-6)),
                "{kind:?} {factor:?} fed={fed} rendered a silent channel; the gate would walk no curve"
            );
            let first: Vec<Vec<f32>> = (0..3)
                .map(|c| node.output().plane(c).samples().to_vec())
                .collect();
            if fed {
                assert_ne!(
                    first, unfed,
                    "{kind:?} {factor:?}: the fed drive changed nothing; the gate would walk the unfed arm"
                );
            } else {
                unfed = first;
            }
            assert_no_alloc::assert_no_alloc(|| {
                for _ in 0..1_000 {
                    node.block();
                }
            });
        }
    }
}

/// Every curve at each antialiasing order, alone and oversampled by 4, unfed and with its drive
/// port fed a per-frame ramp, 1 000 blocks each under `assert_no_alloc`.
#[test]
fn distortion_antialiasing_is_allocation_free_fed_and_unfed() {
    use tutti_graph::contract::Direct;
    use tutti_nodes::{Antialiasing, DistortionNode, OversamplingFactor, ShapeKind};
    use tutti_units::SampleRate;

    let kinds = [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ];
    let drive: Vec<f32> = (0..64).map(|i| 1.0 + 0.0625 * i as f32).collect();
    for kind in kinds {
        for order in [Antialiasing::FirstOrder, Antialiasing::SecondOrder] {
            for oversampled in [false, true] {
                for fed in [false, true] {
                    let node = DistortionNode::<f32>::with_channels(3u16, kind, Drive::new(2.0))
                        .antialiasing(order);
                    let node = if oversampled {
                        node.oversampling(OversamplingFactor::X4)
                    } else {
                        node
                    };
                    let mut node = Direct::new(node, SampleRate::SR_48K, 64);
                    for (c, x) in node.inputs_mut().iter_mut().enumerate() {
                        let x = x.into_samples_mut();
                        for (i, s) in x.iter_mut().enumerate() {
                            *s = 0.9 * (((i + 7 * c) % 13) as f32 / 6.0 - 1.0);
                        }
                    }
                    if fed {
                        node.feed(0, Some(&drive));
                    }
                    node.block();
                    assert!(
                        (0..3).all(|c| node
                            .output()
                            .plane(c)
                            .samples()
                            .iter()
                            .any(|s| s.abs() > 1e-6)),
                        "{kind:?} {order:?} fed={fed} rendered a silent channel"
                    );
                    assert_no_alloc::assert_no_alloc(|| {
                        for _ in 0..1_000 {
                            node.block();
                        }
                    });
                }
            }
        }
    }
}

fn unassigned_bus(width: impl Into<tutti_buffers::ChannelLayout>) -> tutti_buffers::BusTopology {
    tutti_buffers::BusTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// A one-channel convolver over `ir`.
fn mono_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    ir: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> tutti_nodes::ConvolverNode<F> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(tutti_buffers::BusTopology::MONO, &[ir])
            .block_size(block_size),
    )
    .expect("one response on a mono bus")
}

/// One response per channel, at as many unassigned channels as responses.
fn per_channel_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(unassigned_bus(width), irs).block_size(block_size),
    )
}

/// Every input folded to mono, then one response per output, at the engine's placement of
/// that many channels.
fn folded_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    let bus = tutti_buffers::BusTopology::assumed(width).expect("a test width fits a bus");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(bus, irs)
            .folded()
            .block_size(block_size),
    )
}
