//! The tilt: a first-order shelf pivoting about a center, the low end down by half the gain
//! and the high end up by half.

use tutti_units::{Amplitude, Db, Hz, SampleRate, Seconds};

use super::MAX_TAIL_FRAMES;
use super::butterworth::{check_cutoff, check_gain, gain_root};
use super::{Sos, SosError};
use crate::design::prewarp;
use crate::{Biquad, DecayTime};

/// A tilt equalizer, described without a sample rate: the analog first-order shelf
/// `H(s) = (A s + 1) / (s + A)` in `s / wc`, for the center `wc` and `A = 10^(gain / 40)`.
/// Its magnitude is `-gain / 2` dB at DC, `+gain / 2` dB far above the center and 0 dB at the
/// center, the geometric mean of its pole and zero.
///
/// The digital filter is its bilinear transform prewarped at the center, so the response
/// at the center is exactly 0 dB, at DC `-gain / 2` dB and at Nyquist `+gain / 2` dB, and
/// at any frequency `f` the analog magnitude at `tan(pi f / fs) / tan(pi fc / fs)` times
/// the center.
///
/// Built as `Tilt::new(gain)`; [`design`](Self::design) binds a center and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tilt {
    gain: Db,
}

impl Tilt {
    /// A tilt of `gain` between DC and Nyquist: positive brightens (the high end up, the low
    /// end down), negative darkens, a wire at 0 dB.
    #[must_use]
    pub fn new(gain: Db) -> Self {
        Self { gain }
    }

    /// Designs the tilt at `center` and `rate`: one first-order section.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for a center outside `(0, Nyquist)` and
    /// [`SosError::GainOutOfRange`] for a gain beyond [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
    pub fn design(&self, center: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(1), rate);
        self.design_into(center, rate, &mut sos)?;
        Ok(sos)
    }

    /// [`design`](Self::design) into `into`, reusing its sections' storage: no allocation
    /// when it holds room for one section.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design); `into` is then as it was.
    pub fn design_into(
        &self,
        center: Hz,
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        let fc = check_cutoff(center, rate)?;
        let db = check_gain(self.gain)?;
        let a = gain_root(db);
        let g = prewarp(fc, rate);
        // `s = (1 - z^-1) / (g (1 + z^-1))` into `(A s + 1) / (s + A)`.
        let d = 1.0 + a * g;
        into.rate = rate;
        into.sections.clear();
        into.sections.push(Biquad {
            b0: (a + g) / d,
            b1: (g - a) / d,
            b2: 0.0,
            a1: (a * g - 1.0) / d,
            a2: 0.0,
        });
        Ok(())
    }

    /// The time after which the tilt's output to any input in `[-1, 1]` that stops stays
    /// under `floor` (an amplitude, for full-scale input): the first frame `n` with
    /// `sum_{m >= n} |h[m]| <= floor`, exact for the single pole.
    ///
    /// The pole is at `-(x - 1) / (x + 1)` for `x = A tan(pi fc / fs)` (`A` the root of the
    /// amplitude gain), of modulus `r = |x - 1| / (x + 1)`, and `h[m] = c (-a1)^(m-1)` for
    /// `m >= 1` with `c = b1 - b0 a1 = 2 g (1 - A^2) / (1 + A g)^2`, so the tail sum is
    /// `c r^(n-1) / (1 - r)` and `n` is the first frame that brings it to `floor`.
    ///
    /// [`DecayTime::Unbounded`] for a ring-out past `2^24` frames or a floor that is not a number or
    /// is infinite.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design).
    pub fn decay_time(
        &self,
        center: Hz,
        rate: SampleRate,
        floor: Db,
    ) -> Result<DecayTime, SosError> {
        let fc = check_cutoff(center, rate)?;
        let a = gain_root(check_gain(self.gain)?);
        let g = prewarp(fc, rate);
        let eps = floor.to_amplitude();
        Ok(finite(tail_frames(a, g, eps), eps, rate))
    }

    /// The time after which the output of every tilt with a center in `[lowest, highest]`
    /// and a gain of magnitude at most `largest`, to any input in `[-1, 1]` that stops,
    /// stays under `floor`: a bound on [`decay_time`](Self::decay_time) over those controls.
    ///
    /// With `x = A g` (`A` the root of the amplitude gain, `g = tan(pi fc / fs)`) the tail
    /// sum of `decay_time` is `S r^(n-1)` for `S = c / (1 - r)` and `r = |x - 1| / (x + 1)`.
    /// Then `S = g |1 - A^2| / ((1 + x) min(x, 1))` is at most `|1 / A - A|`, which is
    /// under `A_max = 10^(largest / 40)`, so `n <= 1 + ln(A_max / floor) / L` for `L = -ln r
    /// = ln((x + 1) / |x - 1|)`. `L` rises on `(0, 1)` and falls on `(1, inf)`, so over
    /// `x` in `[g_lowest / A_max, A_max g_highest]` it is least at an end of that range, and
    /// the bound is taken there. The ends of the centers may be given in either order.
    /// [`DecayTime::Unbounded`] as for [`decay_time`](Self::decay_time).
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for either end of the centers, and
    /// [`SosError::GainOutOfRange`] for `largest` beyond [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
    pub fn decay_time_over(
        lowest: Hz,
        highest: Hz,
        largest: Db,
        rate: SampleRate,
        floor: Db,
    ) -> Result<DecayTime, SosError> {
        let (lo, hi) = (check_cutoff(lowest, rate)?, check_cutoff(highest, rate)?);
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        let a_max = gain_root(check_gain(Db::new(largest.get().abs()))?);
        let g = |fc: Hz| prewarp(fc, rate);
        let eps = floor.to_amplitude();
        let nepers = |x: f64| libm::log((x + 1.0) / (x - 1.0).abs());
        let least = nepers(g(lo) / a_max).min(nepers(a_max * g(hi)));
        let frames = 1.0 + libm::ceil(libm::log(a_max / eps.get()).max(0.0) / least);
        Ok(finite(frames, eps, rate))
    }
}

/// The first frame from which the tail sum of the tilt of root gain `a` at `g = tan(pi fc /
/// fs)` is at most `eps`.
fn tail_frames(a: f64, g: f64, eps: Amplitude) -> f64 {
    let eps = eps.get();
    let x = a * g;
    let r = (x - 1.0).abs() / (x + 1.0);
    let c = 2.0 * g * (1.0 - a * a).abs() / ((1.0 + x) * (1.0 + x));
    if r == 0.0 {
        return if c <= eps { 1.0 } else { 2.0 };
    }
    let sum = c / (1.0 - r);
    if sum <= eps {
        1.0
    } else {
        1.0 + libm::ceil(libm::log(sum / eps) / -libm::log(r))
    }
}

/// `frames` as a ring-out, [`DecayTime::Unbounded`] past `2^24` frames or for a floor `eps` that is
/// not a positive finite amplitude.
fn finite(frames: f64, eps: Amplitude, rate: SampleRate) -> DecayTime {
    if eps.get().is_finite() && eps.get() > 0.0 && frames <= MAX_TAIL_FRAMES as f64 {
        DecayTime::Finite(Seconds::new(frames / rate.get()))
    } else {
        DecayTime::Unbounded
    }
}
