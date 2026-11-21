use super::*;

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;
const TARGET: usize = 512;

/// `w` for a bandwidth at 48 kHz, radians per frame.
fn natural(bandwidth: Hz) -> f64 {
    std::f64::consts::TAU * bandwidth.get() / RATE.get()
}

/// A producer `drift` fast (it writes `1 + drift` frames per output frame) against a
/// controller, block by block. The fill is the whole frames the producer has written less
/// the whole frames the reader has popped, starting at the target. Returns `(fill, ratio)`
/// per block: the fill the controller saw and the ratio it answered with.
fn simulate(
    controller: &mut FillController,
    drift: f64,
    blocks: usize,
    block: usize,
) -> Vec<(usize, f64)> {
    let (mut written, mut read) = (0.0f64, 0.0f64);
    (0..blocks)
        .map(|_| {
            let fill = TARGET as f64 + written.floor() - read.floor();
            let fill = fill as usize;
            let ratio = controller
                .observe(Samples::new(fill), Samples::new(block))
                .get();
            written += (1.0 + drift) * block as f64;
            read += ratio * block as f64;
            (fill, ratio)
        })
        .collect()
}

/// A producer `d` fast from a fill at target: the loop's error is `e(t) = d t e^(-w t)` and
/// its ratio `1 + d (1 - (1 - w t) e^(-w t))`, `t` in frames. The fill is whole frames, so
/// it differs from `e` by under 1 and the ratio by under `kp = 2 w` for the proportional term
/// and `w` for the sum, which a frame of error moves by `w^2 B` a block over a time constant
/// `1 / (w B)` blocks; the block-step integration adds under `2 w B` of the peak
/// `d / (e w)` (0.02 frames at 50 ppm). The
/// fill never leaves the ring (0, 1024), and after `8 / w` (25 s at the default) it is
/// within a frame of the target and the ratio within `kp + w` of `1 + d`.
#[test]
fn the_fill_and_the_ratio_follow_the_critically_damped_response() {
    let w = natural(LoopBandwidth::DEFAULT.hz());
    for drift in [50e-6, -50e-6, 1000e-6] {
        let mut c = FillController::new(Samples::new(TARGET), RATE);
        let trace = simulate(&mut c, drift, 8000, BLOCK);
        let peak = drift.abs() / (std::f64::consts::E * w);
        let settled = (8.0 / w / BLOCK as f64).ceil() as usize;
        for (k, &(fill, ratio)) in trace.iter().enumerate() {
            let t = (k * BLOCK) as f64;
            let decay = (-w * t).exp();
            let error = drift * t * decay;
            let want = 1.0 + drift * (1.0 - (1.0 - w * t) * decay);
            let got = fill as f64 - TARGET as f64;
            assert!(
                (got - error).abs() <= 1.0 + 2.0 * w * BLOCK as f64 * peak,
                "drift {drift}, block {k}: error {got} vs {error}"
            );
            assert!(
                (ratio - want).abs() <= 3.0 * w + 2.0 * w * BLOCK as f64 * drift.abs(),
                "drift {drift}, block {k}: ratio {ratio} vs {want}"
            );
            assert!(
                (1..1024).contains(&fill),
                "drift {drift}, block {k}: fill {fill}"
            );
            if k >= settled {
                assert!(got.abs() < 1.0 + 1e-9, "drift {drift}, block {k}: {got}");
                assert!(
                    (ratio - (1.0 + drift)).abs() <= 3.0 * w,
                    "drift {drift}, block {k}"
                );
            }
        }
    }
}

/// The peak error is `d / (e w)` frames and the time to it `1 / w`, so a bandwidth a tenth as
/// wide has a peak ten times as high: the default's 2.8 frames at 50 ppm, 28 frames at
/// 0.005 Hz (the larger errors dwarf the frame of quantisation).
#[test]
fn the_peak_error_is_the_drift_over_e_times_the_bandwidth() {
    let drift = 50e-6;
    for bandwidth in [0.005, 0.05, 0.5] {
        let bandwidth = LoopBandwidth::of(Hz::new(bandwidth));
        let mut c = FillController::new(Samples::new(TARGET), RATE).bandwidth(bandwidth);
        let w = natural(bandwidth.hz());
        let trace = simulate(&mut c, drift, (6.0 / w / BLOCK as f64) as usize, BLOCK);
        let (at, top) = trace
            .iter()
            .enumerate()
            .map(|(k, t)| (k, t.0 as f64 - TARGET as f64))
            .fold((0, f64::MIN), |m, x| if x.1 > m.1 { x } else { m });
        let peak = drift / (std::f64::consts::E * w);
        assert!(
            (top - peak).abs() <= 1.0 + 0.02 * peak,
            "{bandwidth:?}: peak {top} vs {peak}"
        );
        let when = at as f64 * BLOCK as f64 * w;
        assert!(
            (when - 1.0).abs() <= 0.1 + 2.0 / peak,
            "{bandwidth:?}: at {when} time constants"
        );
    }
}

/// An error that no ratio in range can undo holds the ratio at the end of the range and the
/// sum at the range's width (0.05), so a fill that then falls to 0 reads the closed form
/// `1 + 0.05 - 2 w 512 - w^2 B 512` (proportional term and one block's integration), not the
/// end of the range a sum grown to 44 by 2000 blocks would hold. A fill on target with
/// nothing learned is exactly unity.
#[test]
fn the_ratio_is_held_to_the_resamplers_range_and_does_not_wind_up() {
    let block = Samples::new(BLOCK);
    let w = natural(LoopBandwidth::DEFAULT.hz());
    let mut c = FillController::new(Samples::new(TARGET), RATE);
    assert_eq!(c.observe(Samples::new(TARGET), block), VariableRate::UNITY);
    for _ in 0..2000 {
        assert_eq!(c.observe(Samples::new(1_000_000), block), VariableRate::MAX);
    }
    let back = c.observe(Samples::new(0), block).get();
    let want = 1.05 - 2.0 * w * 512.0 - w * w * BLOCK as f64 * 512.0;
    assert!((back - want).abs() < 1e-12, "{back} vs {want}");
    let mut low = FillController::new(Samples::new(1_000_000), RATE);
    for _ in 0..2000 {
        assert_eq!(low.observe(Samples::new(0), block), VariableRate::MIN);
    }
    let back = low.observe(Samples::new(1_000_000 + 512), block).get();
    let want = 0.95 + 2.0 * w * 512.0 + w * w * BLOCK as f64 * 512.0;
    assert!((back - want).abs() < 1e-12, "{back} vs {want}");
    c.reset();
    assert_eq!(c.observe(Samples::new(TARGET), block), VariableRate::UNITY);
}

/// A bandwidth is `0.001..=1` Hz, refusing NaN and everything outside it.
#[test]
fn the_bandwidth_is_range_checked() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: LoopBandwidth::new,
        get: Hz::from,
        of: Some(LoopBandwidth::of),
        valid: &[Hz::new(0.001), Hz::new(0.05), Hz::new(1.0)],
        invalid: &[
            Hz::new(0.000_999),
            Hz::new(1.001),
            Hz::ZERO,
            Hz::new(-0.05),
            Hz::new(f64::NAN),
            Hz::new(f64::INFINITY),
        ],
    });
}

/// A block longer than a quarter of the loop's time constant would make the discrete loop
/// ring or diverge; `w B` is held to a quarter, so a 1 Hz loop fed 1 s blocks still settles
/// to within a frame of the target (200 blocks, the loop at `w = 0.25 / B`).
#[test]
fn a_long_block_does_not_destabilise_the_loop() {
    let mut c = FillController::new(Samples::new(TARGET), RATE).bandwidth(LoopBandwidth::MAX);
    let trace = simulate(&mut c, 50e-6, 400, 48_000);
    for &(fill, ratio) in &trace[300..] {
        assert!((fill as f64 - TARGET as f64).abs() <= 1.0, "{fill}");
        assert!(
            (ratio - 1.00005).abs() <= 2.0 * 0.25 / 48_000.0 * 2.0,
            "{ratio}"
        );
    }
}
