//! A kernel's ramped control in a graph: the graph ramps a moved cell over
//! [`Prepare::declick`](tutti_graph::Prepare::declick) and the kernel designs from that ramp
//! at its grid points. Driven straight, without a graph, the same cell lands whole.

use tutti_buffers::ChannelLayout;
use tutti_dsp::Complex64;
use tutti_graph::contract::BlockRig;
use tutti_nodes::{LadderFilterConfig, LadderFilterNode, LadderType};
use tutti_units::{Hz, Resonance, SampleRate, UnitParam};

const FS: f64 = 48_000.0;
const HZ: f64 = 1_500.0;
const RES: f64 = 0.4;
/// The graph's ramp, `DECLICK` (5 ms) at 48 kHz.
const RAMP: i64 = 240;
/// The blocks before the write: the write is seen on stream frame `4 * 64`.
const WARM: usize = 4;

/// A cutoff written between blocks follows the graph's linear ramp, 1000 to 1400 Hz over 240
/// frames from the first frame of the block that sees the write. Each 64-frame block's level
/// is the figure of the ladder designed along that line, and the first block is well under
/// the arrived level, which a cutoff landed whole would reach within its first 16 frames.
#[test]
fn a_ladder_cutoff_written_between_blocks_follows_the_graphs_ramp() {
    let node = LadderFilterNode::<f32>::from(
        LadderFilterConfig::new(ChannelLayout::MONO, LadderType::LP24)
            .freq(Hz::new(1_000.0))
            .res(Resonance::new(RES)),
    );
    let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, 64);
    // Two whole periods in 64 frames, so every block's input is the same and its RMS exact;
    // 1e-3 keeps the saturator linear (`tanh x = x - x^3/3`: 3e-7 of the signal).
    for ch in rig.inputs_mut().iter_mut() {
        for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
            *x = 1e-3 * (core::f64::consts::TAU * HZ * i as f64 / FS).sin() as f32;
        }
    }
    let rms =
        |s: &[f32]| (s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
    let mut level = || {
        rig.block();
        rms(rig.output().plane(0).samples())
    };
    let mut low = 0.0;
    for _ in 0..WARM {
        low = level();
    }
    assert!(controls.set(UnitParam::Cutoff, 1_400.0));
    let blocks: Vec<f64> = (0..8).map(|_| level()).collect();

    // The spec: `1 / ((1 + s/wc)^4 + k)`, `k = 4 res`, under the bilinear map prewarped at
    // the cutoff.
    let gain = |g: f64| {
        let one = Complex64::new(1.0, 0.0);
        let zi = Complex64::from_polar(1.0, -2.0 * core::f64::consts::PI * HZ / FS);
        let s = (one - zi) / (one + zi) / g;
        let p = one + s;
        1.0 / (p * p * p * p + 4.0 * RES).norm()
    };
    let g_of = |fc: f64| libm::tan(core::f64::consts::PI * fc / FS);
    // The graph's frame `j` of the ramp is `(j + 1) / 240` of the way, from stream frame
    // `s0 = 256`: a grid point `p` reads step `p - 255`. A design is taken at each grid point
    // (every 16 frames) and the coefficients ramp to it over the 16 frames after: frame `t`
    // is `(t - p + 1) / 16` of the way from the design at the grid point before `p` to `p`'s.
    let s0 = (WARM * 64) as i64;
    let cell = |p: i64| 1_000.0 + 400.0 * ((p - s0 + 1).clamp(0, RAMP) as f64) / RAMP as f64;
    let expect = |block: usize| {
        let mut sum = 0.0;
        for i in 0..64 {
            let t = s0 + 64 * block as i64 + i;
            let p = t - t % 16;
            let w = (t - p + 1) as f64 / 16.0;
            let g = g_of(cell(p - 16)) * (1.0 - w) + g_of(cell(p)) * w;
            sum += gain(g).powi(2);
        }
        1e-3 * core::f64::consts::FRAC_1_SQRT_2 * (sum / 64.0).sqrt()
    };
    // The tone is steady-state at each block's design, so the ramp's lag in the stages costs a
    // few percent; a cutoff that never moved or jumped is off by tens.
    let near = |got: f64, want: f64, what: &str| {
        assert!(
            (got - want).abs() < 0.10 * want,
            "{what}: {got} against {want}"
        );
    };
    near(
        low,
        1e-3 * core::f64::consts::FRAC_1_SQRT_2 * gain(g_of(1_000.0)),
        "before",
    );
    for (j, &l) in blocks.iter().enumerate() {
        near(l, expect(j), &format!("block {j} after the write"));
    }
    let arrived = 1e-3 * core::f64::consts::FRAC_1_SQRT_2 * gain(g_of(1_400.0));
    assert!(
        blocks[0] < 0.8 * arrived,
        "block 0 is on the ramp: {} against {arrived}",
        blocks[0]
    );
}
