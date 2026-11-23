//! Regression gate for RT-safety: the panner nodes must not allocate on the
//! process path.
//!
//! The gate lives in the crate that owns the panners: `RtAllocCheck`
//! is a global allocator, which only a test binary of that crate may do.

use tutti_buffers::{AmbisonicFormat, AmbisonicOrder, Bed, ChannelTopology, MaxBlock};
use tutti_graph::ParamNode;
use tutti_graph::contract::{BlockRig, Direct};
use tutti_graph::{IntoNode, Node};
use tutti_nodes::testing::aim;
use tutti_nodes::{
    AmbisonicDecoderConfig, AmbisonicDecoderNode, AmbisonicEncoderConfig, AmbisonicEncoderNode,
    AmbisonicRotatorConfig, AmbisonicRotatorNode, Decoding, VbapPannerConfig, VbapPannerNode,
};
use tutti_units::SampleRate;
use tutti_units::{ArcDegrees, Elevation};
use tutti_units::{Azimuth, UnitParam};

/// `node` alone in a graph (`BlockRig`) at 48 kHz in 64-frame blocks, fed a
/// DC pair: `warm` blocks, then 256 under `RtScope`.
///
/// The inputs carry signal, and the gate checks it came out, so a gate over a node that renders nothing cannot pass
/// having walked no DSP.
fn gate<N: IntoNode>(node: N, warm: usize) {
    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        c.fill(0.5);
    }
    for _ in 0..warm {
        rig.block();
    }
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .chain(rig.output().plane(1).samples())
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate would walk no DSP"
    );
    for _ in 0..256 {
        rig.block();
    }
}

#[test]
fn vbap_panner_stereo_process_is_allocation_free() {
    let node = vbap::<f32>(tutti_buffers::Bed::Stereo);
    aim(&node.param_set(), Azimuth::new(30.0), Elevation::new(0.0));
    // One warm-up block.
    gate(node, 1);
}

/// The rear-arc fold branch is allocation-free.
///
/// `vbap_panner_stereo_process_is_allocation_free` above pans to 30 degrees,
/// which is inside the speaker pair and takes neither branch `SpeakerLayout::gains_into`
/// added. A bearing past the lateral axis on a front-only layout takes the
/// fold: the azimuth is mirrored before the solve.
///
/// The fold itself is scalar arithmetic on an `Azimuth` and cannot allocate;
/// what this pins is that it does not push the solve onto a different path
/// inside the solve that does.
#[test]
fn vbap_folded_rear_arc_is_allocation_free() {
    let mut node = vbap::<f32>(tutti_buffers::Bed::Stereo);
    // 200 degrees wraps to -160, well past the lateral axis: the fold fires and
    // mirrors it to -20. Plain VBAP would leave this bearing silent.
    aim(&node.param_set(), Azimuth::new(200.0), Elevation::new(0.0));
    // `reset` seats the de-zipper on the folded bearing, so the assertion
    // window measures the steady state rather than the ramp into it.
    Node::reset(&mut node);
    gate(node, 1);
}

/// The elevation-retreat branch is allocation-free.
///
/// The other branch `SpeakerLayout::gains_into` added, and the only one that re-enters
/// the solver more than once per sample. A 2D ring has no geometry at
/// exactly the poles — the horizontal projection of the direction vector is the
/// zero vector — so the first solve returns all zeros and the retreat halves the
/// elevation and solves again.
///
/// Quad rather than stereo because the pole is where a 2D ring degenerates, and
/// quad is the smallest such layout. The loop is bounded by
/// `MAX_ELEVATION_RETREATS`, so what this pins is that *iterating* it allocates
/// nothing: the same caller-owned scratch is rewritten in place each round, and
/// nothing per-round reaches the heap.
#[test]
fn vbap_elevation_retreat_is_allocation_free() {
    let mut node = vbap::<f32>(tutti_buffers::Bed::Quad);
    // Straight up: a 2D ring cannot solve this, so the retreat runs.
    aim(&node.param_set(), Azimuth::new(0.0), Elevation::new(90.0));
    Node::reset(&mut node);
    gate(node, 1);
}

/// Values a param is driven or written with: ordinary, extreme and hostile
/// (NaN, infinities, magnitudes that overflow an offset, a denormal).
const HOSTILE: [f32; 20] = [
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    1e30,
    -1e30,
    3e37,
    f32::MAX,
    -f32::MAX,
    0.0,
    -0.0,
    1e-40,
    720.5,
    -540.25,
    91.0,
    -91.0,
    180.0,
    -180.0,
    0.5,
    3.0,
    12.0,
];

/// The block lengths a host may call with: none a multiple of the 16-frame
/// grid, so a grid segment is carried across calls.
const BLOCKS: [usize; 4] = [1, 7, 33, 64];

/// Frames of a rendered block: finite, the LFE silent, the power sum of the
/// speakers within the bound a unit-power gain pair gives an input pair of
/// 0.5 and 0.5, `(|l| + |r|)^2 = 1`. Returns the least power over the block.
fn check_block(
    rig: &Direct<VbapPannerNode<f32>>,
    channels: usize,
    lfe: Option<usize>,
    len: usize,
    health: &mut (bool, bool, f32),
) -> f32 {
    let mut least = f32::MAX;
    for frame in 0..len {
        let mut power = 0.0f32;
        for c in 0..channels {
            let s = rig.output().plane(c).samples()[frame];
            health.0 &= s.is_finite();
            power += s * s;
            health.1 |= Some(c) == lfe && s != 0.0;
        }
        health.2 = health.2.max(power);
        least = least.min(power);
    }
    least
}

/// The corner sweep of every bed: each placed bed up to the widest, with its
/// four params moving every block, as cells and as control signals, through ordinary,
/// extreme and hostile values, in blocks that are not multiples of the grid,
/// with resets and a prepare at another rate between blocks, and a fork, allocates
/// nothing, renders finite audio, never feeds the LFE, and keeps the power sum
/// of the speakers within `(|l| + |r|)^2`. A source then held (a mono
/// fold, every param still) settles within 50 ms and plays every frame at the
/// unit power of its gains.
#[test]
fn vbap_every_bed_under_hostile_cells_and_signals_is_allocation_free_finite_and_bounded() {
    use tutti_buffers::{MaxBlock, Speaker};
    use tutti_graph::Prepare;
    use tutti_units::Samples;

    const FRAMES: usize = 64;
    let params = [
        UnitParam::Azimuth,
        UnitParam::Elevation,
        UnitParam::Spread,
        UnitParam::StereoWidth,
    ];
    for bed in Bed::ALL {
        let Ok(node) = VbapPannerNode::<f32>::try_from(VbapPannerConfig::new(bed.into())) else {
            assert_eq!(bed, Bed::Mono);
            continue;
        };
        let lfe = bed.topology().index_of(Speaker::LowFrequency);
        let channels = bed.width().count() as usize;
        assert!(channels <= tutti_buffers::MAX_CHANNELS);
        let controls = node.param_set();
        let fork = node.fork_fresh();
        let mut rig = Direct::new(node, SampleRate::SR_48K, FRAMES);
        let mut forked = Direct::new(fork, SampleRate::SR_48K, FRAMES);
        for d in [&mut rig, &mut forked] {
            for c in d.inputs_mut().iter_mut() {
                let c = c.into_samples_mut();
                c.fill(0.5);
            }
        }
        let fork_controls = forked.node.param_set();
        let mut health = (true, false, 0.0f32);
        let mut signal = vec![0.0f32; FRAMES];

        // The hostile phase: every param a cell or a control signal, by turns.
        for step in 0..600usize {
            let pick = |k: usize| HOSTILE[(step * (k + 3) / 2 + k) % HOSTILE.len()];
            for (k, &param) in params.iter().enumerate() {
                if (step + k) % 3 == 0 {
                    // A control signal of hostile values, per frame.
                    for (i, v) in signal.iter_mut().enumerate() {
                        *v = if i % 5 == 0 {
                            pick(k + i)
                        } else {
                            (i * 7 + step) as f32
                        };
                    }
                    rig.feed(k, Some(&signal));
                } else {
                    rig.feed(k, None);
                    controls.set(param, f64::from(pick(k)));
                }
            }
            let len = BLOCKS[step % BLOCKS.len()];
            rig.set_block_len(len);
            assert_no_alloc::assert_no_alloc(|| {
                if step % 97 == 96 {
                    rig.node.reset();
                }
                if step % 131 == 130 {
                    let rate = [SampleRate::SR_44K1, SampleRate::SR_96K, SampleRate::SR_48K]
                        [step / 131 % 3];
                    rig.node
                        .prepare(&Prepare::new(rate, MaxBlock::of(Samples::new(FRAMES))));
                }
            });
            rig.block();
            let _ = check_block(&rig, channels, lfe, len, &mut health);
        }
        // The fork, moved by cells, under the same window.
        for step in 0..200usize {
            for (k, &param) in params.iter().enumerate() {
                fork_controls.set(param, f64::from(HOSTILE[(step + k * 5) % HOSTILE.len()]));
            }
            let len = BLOCKS[step % BLOCKS.len()];
            forked.set_block_len(len);
            forked.block();
            let _ = check_block(&forked, channels, lfe, len, &mut health);
        }
        assert!(health.0, "{bed:?}: a non-finite sample reached the output");
        assert!(!health.1, "{bed:?}: the LFE was fed");
        assert!(health.2 <= 1.0 + 1e-3, "{bed:?}: power sum {}", health.2);

        // Held: control signals off, a mono fold at an ordinary bearing, the ramp
        // past; every frame is the unit power of its gains (0.5^2 for the
        // folded input 0.5), within a hair.
        for k in 0..4 {
            rig.feed(k, None);
        }
        controls.set(UnitParam::Azimuth, 20.0);
        controls.set(UnitParam::Elevation, 10.0);
        controls.set(UnitParam::Spread, 0.0);
        controls.set(UnitParam::StereoWidth, 0.0);
        let mut least = f32::MAX;
        for step in 0..400usize {
            let len = BLOCKS[step % BLOCKS.len()];
            rig.set_block_len(len);
            rig.block();
            // 400 blocks of 26 frames on average: past 50 ms by far.
            if step >= 300 {
                least = least.min(check_block(&rig, channels, lfe, len, &mut health));
            }
        }
        assert!(
            least >= 0.25 * 0.99 && health.2 <= 1.0 + 1e-3,
            "{bed:?}: a held source's frame has power {least}, want 0.25"
        );
    }
}

/// Minimal format-valid HRIR sphere (a tetrahedron with delta impulse
/// responses) — enough for the crate to parse and convolve without a dataset.
fn synthetic_hrir_sphere(sample_rate: u32, ir_len: usize) -> Vec<u8> {
    let verts: [[f32; 3]; 4] = [
        [1.0, 1.0, 1.0],
        [-1.0, -1.0, 1.0],
        [-1.0, 1.0, -1.0],
        [1.0, -1.0, -1.0],
    ];
    let faces: [[u32; 3]; 4] = [[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]];
    sphere_bytes(sample_rate, ir_len, &verts, &faces)
}

/// A sphere in the HRIR byte format whose every response is a unit delta.
fn sphere_bytes(
    sample_rate: u32,
    ir_len: usize,
    verts: &[[f32; 3]],
    faces: &[[u32; 3]],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(ir_len as u32).to_le_bytes());
    b.extend_from_slice(&(verts.len() as u32).to_le_bytes());
    b.extend_from_slice(&((faces.len() * 3) as u32).to_le_bytes());
    for f in faces {
        for &idx in f {
            b.extend_from_slice(&idx.to_le_bytes());
        }
    }
    for v in verts {
        for &comp in v {
            b.extend_from_slice(&comp.to_le_bytes());
        }
        for _ in 0..2 {
            for i in 0..ir_len {
                b.extend_from_slice(&(if i == 0 { 1.0f32 } else { 0.0 }).to_le_bytes());
            }
        }
    }
    b
}

#[test]
fn hrtf_binaural_process_is_allocation_free() {
    let bytes = synthetic_hrir_sphere(48_000, 64);
    let node = hrtf::<f32>(&bytes).expect("synthetic sphere parses");
    aim(&node.param_set(), Azimuth::new(30.0), Elevation::new(0.0));
    // Three blocks: past the first step's latency, so the gate sees signal.
    // Nothing grows lazily, so nothing needs to warm before the window.
    gate(node, 3);
}

/// The moving path: a new bearing every block keeps the de-zipper in flight,
/// so every step renders through two filters and crossfades, and the face
/// lookup misses its hint and scans. Allocation-free from the first block
/// after insertion.
#[test]
fn hrtf_binaural_moving_source_is_allocation_free() {
    let bytes = synthetic_hrir_sphere(48_000, 64);
    let node = hrtf::<f32>(&bytes).expect("synthetic sphere parses");
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        c.fill(0.5);
    }
    let mut bearing = 0.0f64;
    for _ in 0..256 {
        bearing = if bearing > 170.0 {
            -170.0
        } else {
            bearing + 37.0
        };
        aim(&controls, Azimuth::new(bearing), Elevation::new(20.0));
        rig.block();
    }
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate walked no DSP"
    );
}

/// The nearest-vertex fallback: one face covering only a patch in front, so
/// a source behind the listener misses the hint and every face, and each
/// step scans the vertices for the nearest.
#[test]
fn hrtf_binaural_nearest_vertex_fallback_is_allocation_free() {
    let verts = [
        [0.0, 0.0, -1.0],
        [-0.5, 0.0, -0.866],
        [0.0, 0.5, -0.866],
        [0.0, 0.0, 1.0],
    ];
    let bytes = sphere_bytes(48_000, 64, &verts, &[[0, 1, 2]]);
    let node = hrtf::<f32>(&bytes).expect("the sphere parses");
    aim(&node.param_set(), Azimuth::new(180.0), Elevation::new(0.0));
    gate(node, 3);
}

/// A re-prepare at the rate the node was first prepared at (a new block size) keeps
/// the prepared sphere, so it allocates nothing.
#[test]
fn hrtf_binaural_same_rate_prepare_is_allocation_free() {
    use tutti_graph::Prepare;

    let bytes = synthetic_hrir_sphere(48_000, 64);
    let mut node = hrtf::<f32>(&bytes).expect("synthetic sphere parses");
    let prepare = Prepare::new(
        SampleRate::SR_48K,
        tutti_buffers::MaxBlock::of(tutti_units::Samples::new(256)),
    );
    node.prepare(&prepare);
    assert_no_alloc::assert_no_alloc(|| node.prepare(&prepare));
}

/// A set resampled at prepare (a 48 kHz sphere at 44.1 kHz) renders
/// allocation-free in 100-frame blocks, which do not divide the 128-frame
/// step: most blocks finish one step and start the next, so the render runs
/// with frames of the block still to copy on both sides of it.
#[test]
fn hrtf_binaural_resampled_set_in_split_blocks_is_allocation_free() {
    use tutti_graph::Prepare;

    use tutti_units::{Samples, Tail};

    let bytes = synthetic_hrir_sphere(48_000, 64);
    let mut node = hrtf::<f32>(&bytes).expect("synthetic sphere parses");
    aim(&node.param_set(), Azimuth::new(30.0), Elevation::new(0.0));
    node.prepare(&Prepare::new(
        SampleRate::SR_44K1,
        MaxBlock::of(Samples::new(100)),
    ));
    // 64 taps at 48 kHz are ceil(64 * 44 100 / 48 000) = 59 at 44.1 kHz:
    // the set was resampled, not reused.
    assert_eq!(node.shape().tail, Tail::Finite(Samples::new(59 - 1)));

    let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_44K1, 100);
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        c.fill(0.5);
    }
    for _ in 0..256 {
        rig.block();
    }
    assert!(
        rig.output()
            .plane(0)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6),
        "the node rendered silence; the gate walked no DSP"
    );
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    tutti_nodes::VbapPannerNode::try_from(tutti_nodes::VbapPannerConfig::new(bed.into()))
        .expect("a bed with speakers to pan between")
}

/// A binaural node over the sphere in `bytes`, prepared at `rate`.
fn hrtf<F: tutti_dsp::FftFloat>(
    bytes: &[u8],
) -> Result<tutti_nodes::HrtfBinauralNode<F>, tutti_dsp::HrtfBinauralError> {
    Ok(tutti_nodes::HrtfBinauralNode::from(
        tutti_nodes::HrtfBinauralConfig::new(std::sync::Arc::new(
            tutti_dsp::HrirSphere::from_bytes(bytes)?,
        )),
    ))
}

/// `node` alone in a graph at 48 kHz in 64-frame blocks, its direction moved before every
/// block: 16 warm-up blocks, then 256 under `RtScope`. The inputs carry a
/// non-trivial signal and the output is checked to sound.
fn gate_moving<N: IntoNode<Controls = tutti_graph::ParamSet>>(node: N) {
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for (c, plane) in rig.inputs_mut().iter_mut().enumerate() {
        for (i, x) in plane.into_samples_mut().iter_mut().enumerate() {
            *x = 0.1 * (c as f32 + 1.0) * (((i * 7 + c) % 13) as f32 - 6.0);
        }
    }
    let aim = |i: usize| {
        controls.set(
            UnitParam::Azimuth,
            if i.is_multiple_of(2) { -150.0 } else { 120.0 },
        );
        controls.set(
            UnitParam::Elevation,
            if i.is_multiple_of(3) { 60.0 } else { -20.0 },
        );
    };
    for i in 0..16 {
        aim(i);
        rig.block();
    }
    assert!(
        (0..rig.output().channels()).any(|c| rig
            .output()
            .plane(c)
            .samples()
            .iter()
            .any(|s| s.abs() > 1e-6)),
        "the node rendered silence; the gate would walk no DSP"
    );
    for i in 0..256 {
        aim(i);
        rig.block();
    }
}

#[test]
fn ambisonic_encoder_process_is_allocation_free_with_the_direction_moving() {
    gate_moving(
        AmbisonicEncoderNode::<f32>::try_from(AmbisonicEncoderConfig::new(ambix(
            AmbisonicOrder::MAX,
        )))
        .unwrap(),
    );
}

#[test]
fn ambisonic_rotator_process_is_allocation_free_with_the_front_moving() {
    gate_moving(
        AmbisonicRotatorNode::<f32>::try_from(
            AmbisonicRotatorConfig::new(ambix(AmbisonicOrder::MAX)).roll(ArcDegrees::new(30.0)),
        )
        .unwrap(),
    );
}

#[test]
fn ambisonic_decoder_process_is_allocation_free() {
    for decoding in [Decoding::Sampling, Decoding::AllRad] {
        let node = AmbisonicDecoderNode::<f32>::try_from(
            AmbisonicDecoderConfig::new(Bed::Surround71)
                .input(ambix(AmbisonicOrder::MAX))
                .decoding(decoding),
        )
        .unwrap();
        let (mut rig, _controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
        for (c, plane) in rig.inputs_mut().iter_mut().enumerate() {
            plane.into_samples_mut().fill(0.05 * (c as f32 + 1.0));
        }
        rig.block();
        assert!(
            rig.output()
                .plane(0)
                .samples()
                .iter()
                .any(|s| s.abs() > 1e-6)
        );
        for _ in 0..256 {
            rig.block();
        }
    }
}

/// The AmbiX bus of `order`: ACN, SN3D.
fn ambix(order: AmbisonicOrder) -> ChannelTopology {
    ChannelTopology::Ambisonic(AmbisonicFormat::ambix(order))
}
