//! State-variable filter coefficients and the per-sample step (Simper's
//! trapezoidal SVF).
//!
//! One coefficient computation serves eight responses ([`SvfType`]), so
//! switching type is as cheap as any parameter change.

use tutti_buffers::Float;

use crate::{Complex64, Lanes, Lerp};
use tutti_units::{Db, Hz, Q, SampleRate};

/// Which response the state-variable filter presents.
///
/// All eight share one coefficient computation and differ only in the output
/// mix (`m0`/`m1`/`m2`), so switching type is as cheap as any other parameter
/// change. The last three ([`Bell`](Self::Bell), [`LowShelf`](Self::LowShelf),
/// [`HighShelf`](Self::HighShelf)) are the only ones that read the gain
/// parameter; the rest ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum SvfType {
    /// Passes below cutoff, attenuates above it at 12 dB/octave. The default.
    #[default]
    LowPass,
    /// Passes above cutoff, attenuates below it at 12 dB/octave.
    HighPass,
    /// Passes a band around cutoff; [`Q`] sets how narrow. It is the RBJ
    /// cookbook's constant-skirt-gain band-pass: its peak gain at cutoff is `Q`
    /// (0 dB only at `Q = 1`), its skirts the same whatever `Q`.
    BandPass,
    /// Rejects a band around cutoff and passes everything else — the inverse of
    /// [`BandPass`](Self::BandPass). High [`Q`] gives a narrow, deep notch.
    Notch,
    /// Allpass: flat magnitude, frequency-dependent phase shift.
    ///
    /// Passes every frequency at unity and changes only phase, which is what
    /// makes it the building block of a phaser.
    Allpass,
    /// Peaking/bell EQ: boosts or cuts a band around cutoff by the gain
    /// parameter, leaving the rest flat. [`Q`] sets the bandwidth.
    Bell,
    /// Shelf boosting or cutting everything *below* cutoff by the gain
    /// parameter, flat above it.
    LowShelf,
    /// Shelf boosting or cutting everything *above* cutoff by the gain
    /// parameter, flat below it.
    HighShelf,
}

/// Pure SVF coefficient set computed from filter parameters.
///
/// The per-sample recurrence these coefficients feed is
/// `v3 = v0 - ic2; v1 = a1*ic1 + a2*v3; v2 = ic2 + a2*ic1 + a3*v3;
/// ic1 = 2*v1 - ic1; ic2 = 2*v2 - ic2; y = m0*v0 + m1*v1 + m2*v2`
/// (Simper's trapezoidal SVF).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SvfCoeffs {
    /// The prewarped integrator gain, `tan(pi * fc / sr)` (a shelf's divided or
    /// multiplied by `sqrt(A)`): where the response's frequency axis sits.
    pub g: f64,
    /// The damping, `1 / Q` (a bell's `1 / (Q * A)`).
    pub k: f64,
    /// `1 / (1 + g*(g + k))`, with `g = tan(pi * fc / sr)` and `k = 1 / Q`.
    /// A bell uses `k = 1 / (Q * A)`; a low (high) shelf divides (multiplies)
    /// `g` by `sqrt(A)`, `A = 10^(dB / 40)`.
    pub a1: f64,
    /// `g * a1`.
    pub a2: f64,
    /// `g * a2`.
    pub a3: f64,
    /// Output weight of the input `v0`.
    pub m0: f64,
    /// Output weight of the band-pass state `v1`.
    pub m1: f64,
    /// Output weight of the low-pass state `v2`.
    pub m2: f64,
}

impl SvfCoeffs {
    /// The largest gain, in dB either way, [`new`](Self::new) solves for. A
    /// gain further out is this one: 200 dB is a factor of 10^10 in
    /// amplitude, past anything audible, and keeps every coefficient finite.
    pub const MAX_GAIN: Db = Db(200.0);

    /// Computes SVF filter coefficients from parameters (pure function, no state).
    ///
    /// The three tuning parameters are three different units, and taking them as
    /// [`Hz`] / [`Q`] / [`Db`] is what keeps them apart. As bare floats they are
    /// adjacent and interchangeable — the call reads as a run of positional numbers,
    /// `(LowPass, 1000.0, 0.707, 0.0, 44100.0)`, where transposing any two compiles
    /// and detunes the filter.
    pub fn new(
        filter_type: SvfType,
        freq: impl Into<Hz>,
        q: impl Into<Q>,
        gain_db: impl Into<Db>,
        sample_rate: impl Into<SampleRate>,
    ) -> SvfCoeffs {
        let (freq, q, gain_db) = (freq.into().get(), q.into().get(), gain_db.into());
        let sample_rate = sample_rate.into();
        // `tan` diverges at Nyquist itself, so the cutoff stops just short of it.
        let fc = freq.clamp(
            1.0,
            sample_rate
                .nyquist_scaled(crate::filter::NYQUIST_MARGIN)
                .get(),
        );
        let g0 = libm::tan(core::f64::consts::PI * fc / sample_rate.get());
        let q = q.max(0.01);
        // A gain past `MAX_GAIN` either way is that gain: `10^(dB / 40)`
        // underflows to 0 far out (`k = 1 / (Q * A)` is then infinite and the
        // set NaN), and overflows to infinity the other way. A NaN gain is
        // unity (0 dB).
        let gain_db = if gain_db.get().is_nan() {
            Db::UNITY
        } else {
            gain_db.clamp(-Self::MAX_GAIN, Self::MAX_GAIN)
        };
        // The half-gain amplitude `10^(dB / 40)` is half the decibels.
        let a = Db(0.5 * gain_db.get()).to_amplitude().get();

        // RBJ cookbook shapes: a bell's damping is `1/(Q*A)`, and a shelf's
        // midpoint (half the dB gain) sits at `fc`, so its prototype is tuned
        // `sqrt(A)` away.
        let k = match filter_type {
            SvfType::Bell => 1.0 / (q * a),
            _ => 1.0 / q,
        };
        let g = match filter_type {
            SvfType::LowShelf => g0 / a.sqrt(),
            SvfType::HighShelf => g0 * a.sqrt(),
            _ => g0,
        };

        let [a1, a2, a3] = integrator_gains(g, k);

        let (m0, m1, m2) = match filter_type {
            SvfType::LowPass => (0.0, 0.0, 1.0),
            SvfType::HighPass => (1.0, -k, -1.0),
            SvfType::BandPass => (0.0, 1.0, 0.0),
            SvfType::Notch => (1.0, -k, 0.0),
            // Allpass: flat magnitude, all-pass phase. m1 = -2k (vs notch's -k).
            SvfType::Allpass => (1.0, -2.0 * k, 0.0),
            SvfType::Bell => (1.0, k * (a * a - 1.0), 0.0),
            SvfType::LowShelf => (1.0, k * (a - 1.0), a * a - 1.0),
            SvfType::HighShelf => (a * a, k * (1.0 - a) * a, 1.0 - a * a),
        };

        SvfCoeffs {
            g,
            k,
            a1,
            a2,
            a3,
            m0,
            m1,
            m2,
        }
    }

    /// `H(e^jw)` of the set, read from the trapezoidal form: the warped frequency in
    /// units of the prototype's cutoff is `t = tan(w / 2) / g`, and
    /// `H = m0 + (m2 + j m1 t) / (1 - t^2 + j k t)`. Well conditioned at a pole near
    /// `z = 1`, where the direct coefficients are not.
    pub(crate) fn response(&self, w: f64) -> Complex64 {
        let t = libm::tan(w / 2.0) / self.g;
        let d = Complex64::new(1.0 - t * t, self.k * t);
        self.m0 + Complex64::new(self.m2, self.m1 * t) / d
    }
}

/// The coefficients [`svf_step`] reads, in the arithmetic `L` it runs in: a filter
/// node's state precision, or the lane bank's `f32x8`.
///
/// One set serves every channel of a node: the controls are one linked surface, so
/// widening replicates only the integrator state. The fields are named, so a set
/// built by hand (the lane bank's, one cutoff per lane) cannot swap two of them;
/// [`SvfCoeffs`] solves them from the controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfCoefficients<L: Lanes> {
    /// `1 / (1 + g (g + k))`.
    pub a1: L,
    /// `g * a1`.
    pub a2: L,
    /// `g * a2`.
    pub a3: L,
    /// The input's weight in the output.
    pub m0: L,
    /// The band-pass state's weight in the output.
    pub m1: L,
    /// The low-pass state's weight in the output.
    pub m2: L,
}

impl<F: Float + Lanes> From<&SvfCoeffs> for SvfCoefficients<F> {
    /// The solve's `f64` set narrowed once to `F`.
    fn from(c: &SvfCoeffs) -> Self {
        Self {
            a1: F::from_f64(c.a1),
            a2: F::from_f64(c.a2),
            a3: F::from_f64(c.a3),
            m0: F::from_f64(c.m0),
            m1: F::from_f64(c.m1),
            m2: F::from_f64(c.m2),
        }
    }
}

impl<F: Float + Lanes> From<&super::SvfModel> for SvfCoefficients<F> {
    /// The model's own coefficients ([`SvfModel::new`](super::SvfModel::new) sanitises the
    /// controls first), narrowed once to `F`.
    fn from(model: &super::SvfModel) -> Self {
        Self::from(model.coeffs())
    }
}

impl<F: Float + Lanes> Lerp for SvfCoefficients<F> {
    /// Each coefficient blended linearly, `t = k / n` in `F`: an interpolated set is only
    /// ever used for one sample and never stored.
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = F::from_f64(f64::from(k)) / F::from_f64(f64::from(n));
        Self {
            a1: crate::lerp(self.a1, to.a1, t),
            a2: crate::lerp(self.a2, to.a2, t),
            a3: crate::lerp(self.a3, to.a3, t),
            m0: crate::lerp(self.m0, to.m0, t),
            m1: crate::lerp(self.m1, to.m1, t),
            m2: crate::lerp(self.m2, to.m2, t),
        }
    }
}

/// One channel's integrator step (Cytomic/Zavalishin TPT form) over its two
/// state words `[ic1eq, ic2eq]`.
///
/// The pair is kept together per channel (array-of-structures), not split into
/// one array per word: the two updates are symmetric and their loads and
/// stores stay adjacent. Measured, splitting them cost 26% at width 6 — an
/// across-channel layout only pays once a channel-inner loop is explicitly
/// vectorised, which this one is not.
///
/// Written once over [`Lanes`], so the polysynth's lane bank runs this step on eight
/// voices at a time.
#[inline(always)]
pub fn svf_step<L: Lanes>(c: &SvfCoefficients<L>, s: &mut [L; 2], v0: L) -> L {
    let [v1, v2] = svf_taps([c.a1, c.a2, c.a3], s, v0);
    c.m0 * v0 + c.m1 * v1 + c.m2 * v2
}

/// The integrator coefficients `[a1, a2, a3]` of a trapezoidal state-variable filter at the
/// prewarped gain `g = tan(pi fc / fs)` and the damping `k = 1 / Q`: `a1 = 1 / (1 + g (g +
/// k))`, `a2 = g a1`, `a3 = g a2`. The one solve of [`SvfCoeffs`] and the crossover's sections.
pub(crate) fn integrator_gains(g: f64, k: f64) -> [f64; 3] {
    let a1 = 1.0 / (1.0 + g * (g + k));
    let a2 = g * a1;
    [a1, a2, g * a2]
}

/// One integrator step over the state `[ic1eq, ic2eq]` with the coefficients `[a1, a2, a3]`:
/// the band-pass and low-pass taps `[v1, v2]` the filter's responses are mixed from. The
/// recurrence of [`svf_step`] and of the crossover's sections.
#[inline(always)]
pub(crate) fn svf_taps<L: Lanes>([a1, a2, a3]: [L; 3], s: &mut [L; 2], v0: L) -> [L; 2] {
    let two = L::from_f32(2.0);
    let [ic1eq, ic2eq] = *s;
    let v3 = v0 - ic2eq;
    let v1 = a1 * ic1eq + a2 * v3;
    let v2 = ic2eq + a2 * ic1eq + a3 * v3;
    *s = [two * v1 - ic1eq, two * v2 - ic2eq];
    [v1, v2]
}

#[cfg(test)]
mod tests;
