//! [`Lti`]: a linear time-invariant system.

use tutti_units::Hz;

use super::System;
use crate::Complex64;

/// A linear time-invariant system (order 0 is a
/// gain matrix). The queries are the system's invariants, answered from its
/// structure and never from its realization.
///
/// A response `H` exists if and only if the system is linear and commutes with the
/// unit delay. A nonlinear member (the ladder's `tanh`, a compressor) has none; one that
/// has a small-signal model implements `Lti` for that model, as
/// [`LadderModel`](crate::LadderModel) does, and the analysis applies to it.
///
/// The queries describe a member at fixed controls, not the system while its controls
/// move, and two limits follow. The frozen response
/// overstates decay near Nyquist under a delay modulated with linear interpolation: a
/// fractional delay of one half cancels a tone there and the modulation visits every
/// fraction, so the queries answer for static controls only. And a frozen
/// [`spectral_radius`](Self::spectral_radius) below 1 does not imply that the system is
/// stable while its controls move.
///
/// `spectral_radius`, `l1_norm` and `hinf_norm` are **sufficient bounds**: they
/// never fall under the true figure, so no measurement of the system exceeds them.
pub trait Lti: System {
    /// The input signals.
    fn inputs(&self) -> usize {
        1
    }

    /// The output signals.
    fn outputs(&self) -> usize {
        1
    }

    /// `H(e^jw)` from the system's input to output `output`, at each frequency of
    /// `freqs` into `gain` (the same length). Control side: it may allocate. A
    /// system with no response to give fills `gain` with NaN.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]);

    /// An upper bound on the spectral radius of the state map: the slowest
    /// decay per frame. The system is stable when it is below 1.
    fn spectral_radius(&self) -> f64;

    /// An upper bound on `||h||_1`, the l1 norm of the impulse response: the BIBO
    /// gain, so that `|x| <= 1` gives `|y| <= l1_norm` (the guarantee against
    /// overs). Infinity when no bound is known.
    fn l1_norm(&self) -> f64;

    /// An upper bound on `||H||_inf`, the peak of `|H(e^jw)|`: what a response
    /// display scales to. A system with nothing tighter returns
    /// [`l1_norm`](Self::l1_norm), which is never below it.
    fn hinf_norm(&self) -> f64 {
        self.l1_norm()
    }

    /// Whether no state can grow. By default the spectral radius is under 1; a
    /// system whose structure gives a sharper test overrides it (the reverb
    /// network accepts a frozen loop, which has radius 1, and refuses
    /// hostile figures the radius does not look at).
    fn is_stable(&self) -> bool {
        self.spectral_radius() < 1.0
    }
}

/// Fills `gain` for a one-output system: `response(f)` at each of `freqs` for output 0, NaN
/// for any other output, as [`Lti::frequency_response`] answers.
pub(crate) fn single_output(
    output: usize,
    freqs: &[Hz],
    gain: &mut [Complex64],
    mut response: impl FnMut(Hz) -> Complex64,
) {
    for (g, f) in gain.iter_mut().zip(freqs) {
        *g = if output == 0 {
            response(*f)
        } else {
            Complex64::new(f64::NAN, f64::NAN)
        };
    }
}

/// The peak of `mag` over `pts` (angles in `0..=pi`, any order, duplicates allowed): the
/// largest at a point, refined by golden section between its neighbours, then raised by a
/// part in a million so that it is a sufficient bound to that figure.
pub(crate) fn peak_magnitude(mut pts: Vec<f64>, mag: impl Fn(f64) -> f64) -> f64 {
    pts.retain(|w| (0.0..=core::f64::consts::PI).contains(w));
    pts.sort_by(f64::total_cmp);
    pts.dedup();
    let best = pts
        .iter()
        .enumerate()
        .max_by(|a, b| mag(*a.1).total_cmp(&mag(*b.1)))
        .map_or(0, |(i, _)| i);
    let (mut a, mut b) = (
        pts[best.saturating_sub(1)],
        pts[(best + 1).min(pts.len() - 1)],
    );
    let phi = 0.618_033_988_749_894_9;
    for _ in 0..80 {
        let (x, y) = (b - phi * (b - a), a + phi * (b - a));
        if mag(x) < mag(y) {
            a = x;
        } else {
            b = y;
        }
    }
    mag(pts[best]).max(mag(0.5 * (a + b))) * (1.0 + 1e-6)
}
