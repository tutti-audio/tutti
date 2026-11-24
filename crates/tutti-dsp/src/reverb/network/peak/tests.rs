use super::*;

/// The largest `|R|` over a dense grid (dense toward DC), the true peak a bound is held against.
fn dense_peak(g: [f64; 3], pole: [f64; 2]) -> f64 {
    let c = [g[2], g[1] - g[2], g[0] - g[1]];
    let points = 100_000u32;
    (0..=points)
        .map(|k| {
            let w = PI * (f64::from(k) / f64::from(points)).powi(2);
            norm(response(c, pole, w).r)
        })
        .fold(0.0, f64::max)
}

fn pole(fc: f64, rate: f64) -> [f64; 2] {
    [fc, 4_000.0].map(|f| libm::exp(-std::f64::consts::TAU * f / rate))
}

/// Profiles that rise with frequency, with the crossover and rate each is read at: the design's
/// (300 Hz) and the extremes (a 100 Hz crossover at 192 kHz, whose second-derivative bound is
/// the largest, at the largest gain a design holds).
fn rising() -> Vec<([f64; 3], [f64; 2])> {
    vec![
        ([0.05, 0.99, 0.99], pole(300.0, 48_000.0)),
        ([0.0, 0.9999, 0.9999], pole(300.0, 48_000.0)),
        ([0.04, 0.85, 0.98], pole(300.0, 44_100.0)),
        ([0.05, 0.9999, 0.9999], pole(100.0, 192_000.0)),
        ([0.5, 0.999, 0.7], pole(20.0, 192_000.0)),
    ]
}

/// A profile whose tolerance (`1e-12` at a largest gain a billionth under 1) cannot be met in the
/// shipped budget: it spends it, and the bound is still above the dense peak.
#[test]
fn the_shipped_budget_is_spent_by_a_profile_nothing_designs_and_still_bounds() {
    let g = [0.0, 1.0 - 1e-9, 1.0 - 1e-9];
    let pole = pole(300.0, 48_000.0);
    let (bound, evals) = searched(g, pole, BUDGET);
    assert!(evals >= BUDGET, "{evals} samples");
    assert!(evals <= BUDGET + 1, "{evals} samples");
    assert!(bound >= dense_peak(g, pole));
}

/// A spent budget stops the splitting and returns the intervals' bounds as they are: still at or
/// above the true peak of a dense sampling, only looser, and no more samples are taken than the
/// budget (or the first pass, when that is larger). A budget of 18 is spent by the first split.
/// The shipped budget is not spent by these profiles.
#[test]
fn a_spent_budget_still_bounds_the_peak() {
    for (g, pole) in rising() {
        let truth = dense_peak(g, pole);
        let (spent, evals) = searched(g, pole, 18);
        assert!(evals <= 18, "{g:?}: {evals} samples on a budget of 18");
        assert!(
            spent >= truth,
            "{g:?}: the bound {spent} is under the peak {truth}"
        );
        let (full, evals) = searched(g, pole, BUDGET);
        assert!(evals < BUDGET, "{g:?}: the shipped budget is spent");
        assert!(full >= truth, "{g:?}: the bound {full} is under {truth}");
        assert!(
            spent > full,
            "{g:?}: the spent budget's bound {spent} is not looser than {full}"
        );
    }
}

/// The design's scaling on a spent budget holds the peak under the largest gain, which is under
/// 1: the scaled gains' dense peak is at most the figure returned.
#[test]
fn the_scaling_on_a_spent_budget_still_holds_the_peak_under_one() {
    for (g, pole) in rising() {
        let (scaled, held) = rescale_within(g.map(Amplitude::new), pole, 18);
        let top = g.iter().copied().fold(0.0, f64::max);
        assert!(held <= top.min(0.9999), "{g:?}: held {held}");
        let truth = dense_peak(scaled.map(Amplitude::get), pole);
        assert!(truth <= held, "{g:?}: the scaled peak {truth} over {held}");
        assert!(truth < 1.0);
    }
}
