//! [`Sos`]: a cascade of second-order sections, the design and analysis form of an IIR
//! filter of any order, and the [`Butterworth`], [`ChebyshevI`], [`ChebyshevII`], [`Elliptic`],
//! [`LinkwitzRiley`], [`MatchedBell`], [`Tilt`] and [`KWeighting`] designs that make one.
//!
//! A [`Sos`] is plain data: a list of [`Biquad`]s whose product is the response. A first-order section is a [`Biquad`] with `b2 = a2 = 0`.
//! [`SosBank`] runs one as a cascade of trapezoidal state-variable sections (each section's
//! `svf_form` is its equivalent), never as direct-form biquads, which misbehave when their
//! coefficients move and at a low cutoff in `f32`.

mod analog;
mod bank;
mod bell;
mod butterworth;
mod chebyshev;
mod elliptic;
mod jacobi;
mod k_weighting;
mod tilt;

use tutti_units::{Db, Hz, SampleRate, Samples, Seconds};

use crate::filter::{integrator_gains, section_l1};
use crate::{Biquad, Complex64, DecayTime, Lti, SvfDesign, System, svf_step};

pub use analog::{
    PassbandRipple, PassbandRippleError, StopbandAttenuation, StopbandAttenuationError,
};
pub use bank::{MAX_SECTIONS, SosBank, SosCoefficients};
pub use bell::MatchedBell;
pub use butterworth::{
    Butterworth, ButterworthOrder, ButterworthOrderError, CutResponse, LinkwitzRiley,
    MAX_GAIN_MAGNITUDE, SosError,
};
pub use chebyshev::{ChebyshevI, ChebyshevII};
pub use elliptic::{Elliptic, EllipticTolerances};
pub use k_weighting::KWeighting;
pub use tilt::Tilt;

/// The longest impulse response [`Sos::tail`](System::tail) follows, in frames: a longer
/// ring-out reads as [`DecayTime::Unbounded`].
const MAX_TAIL_FRAMES: u64 = 1 << 24;

/// A cascade of second-order sections at a sample rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Sos {
    sections: Vec<Biquad>,
    rate: SampleRate,
}

impl Sos {
    /// The cascade of `sections` at `rate`; no sections is a wire.
    #[must_use]
    pub fn new(sections: Vec<Biquad>, rate: SampleRate) -> Self {
        Self { sections, rate }
    }

    /// The sections, in order.
    #[must_use]
    pub fn sections(&self) -> &[Biquad] {
        &self.sections
    }

    /// The rate the coefficients are for.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }
}

/// The trapezoidal state-variable section with the response of `b`.
///
/// The prewarp `g` and damping `k` come from the denominator (`g^2 = (1 + a1 + a2) /
/// (1 - a1 + a2)`, the response's frequency axis; `k` the remaining coefficient) and the
/// output mix `m0, m1, m2` from the numerator (`m0` is the response at Nyquist). A
/// first-order section (`b2 = a2 = 0`) is the state-variable form with a double pole at the
/// section's own, `k = 2`, and a zero cancelling the second: `H(s) = (c0 + c1 s) / (1 + s)`
/// is `(c1 s^2 + (c0 + c1) s + c0) / (1 + s)^2`.
#[must_use]
pub(crate) fn svf_form(b: &Biquad) -> SvfDesign {
    let (g, k, [m0, m1, m2]) = if b.a2 == 0.0 && b.b2 == 0.0 {
        let g = (1.0 + b.a1) / (1.0 - b.a1);
        let c0 = (g + 1.0) * (b.b0 + b.b1) / (2.0 * g);
        let c1 = (g + 1.0) * (b.b0 - b.b1) / 2.0;
        (g, 2.0, [c1, c0 - c1, c0 - c1])
    } else {
        let d = 1.0 - b.a1 + b.a2;
        let g = ((1.0 + b.a1 + b.a2) / d).sqrt();
        let d0 = 4.0 / d;
        let k = (d0 - 1.0 - g * g) / g;
        let m0 = (b.b0 - b.b1 + b.b2) / d;
        let m1 = (d0 * (b.b0 - b.b2) - m0 * (d0 - b.a2 * d0)) / (2.0 * g);
        let m2 = (d0 * b.b0 - m0 * d0 - m1 * g) / (g * g);
        (g, k, [m0, m1, m2])
    };
    let [d, a2, a3] = integrator_gains(g, k);
    SvfDesign {
        g,
        k,
        d,
        a2,
        a3,
        m0,
        m1,
        m2,
    }
}

impl Sos {
    /// The slowest section's decay and its pole's angle: the rate of the response's
    /// envelope and the period of its oscillation, in frames.
    fn slowest(&self) -> (f64, f64) {
        let pole = self
            .sections
            .iter()
            .flat_map(Biquad::poles)
            .max_by(|a, b| a.norm().total_cmp(&b.norm()))
            .unwrap_or_default();
        let radius = pole.norm().clamp(1e-12, 1.0);
        let angle = pole.arg().abs();
        let period = if angle > 1e-9 {
            core::f64::consts::TAU / angle
        } else {
            0.0
        };
        (-1.0 / libm::log(radius), period)
    }
}

impl System for Sos {
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    /// The frames until the impulse response of the whole cascade is the floor under its
    /// peak, found on the response itself in `f64` through the state-variable sections:
    /// the last frame at or above `floor` of the peak, plus a quarter period of the slowest
    /// pole's oscillation (the most a sample can fall short of the envelope's crossing), so the
    /// figure errs long.
    ///
    /// Not the longest of the sections' tails: a pole repeated `m` times (a Linkwitz-Riley
    /// section is a Butterworth one twice) rings as `n^(m-1) r^n`, which peaks `(m - 1)`
    /// time constants (`1 / -ln r`) in, so a cascade whose peak is its slow response (a
    /// low-pass) falls to the floor after one section alone has. A high-pass peaks in its
    /// first frames, and a section alone, relative to its own peak, can ring longer than the
    /// cascade does. A partial-fraction bound is not used: the residues of a
    /// narrow high-order cascade are a ratio of near-equal poles to the order, and their
    /// sum cancels to nothing in `f64`. The run ends once it is past the peak of
    /// the slowest envelope (one time constant per pole) and a window of `3` time constants and a
    /// period stays a tenth of the floor under the peak. [`DecayTime::Unbounded`] when a pole is not
    /// inside the unit circle or the run passes `2^24` frames.
    fn tail(&self, floor: Db) -> DecayTime {
        let radius = self.spectral_radius();
        if radius.is_nan() || radius >= 1.0 {
            return DecayTime::Unbounded;
        }
        let (tau, period) = self.slowest();
        let eps = floor.to_amplitude().get();
        let settled = (2.0 * self.sections.len() as f64 * tau) as u64;
        let window = (3.0 * tau + period).ceil().max(16.0) as u64;
        let sections: Vec<_> = self
            .sections
            .iter()
            .map(|b| crate::SvfCoefficients::<f64>::from(&svf_form(b)))
            .collect();
        let mut state = vec![[0.0f64; 2]; sections.len()];
        let (mut peak, mut last, mut n) = (0.0f64, 0u64, 0u64);
        loop {
            let mut lull = 0.0f64;
            for _ in 0..window {
                let mut y = if n == 0 { 1.0 } else { 0.0 };
                for (c, s) in sections.iter().zip(&mut state) {
                    y = svf_step(c, s, y);
                }
                let a = y.abs();
                peak = peak.max(a);
                if a >= eps * peak {
                    last = n;
                }
                lull = lull.max(a);
                n += 1;
            }
            if !peak.is_finite() || n > MAX_TAIL_FRAMES {
                return DecayTime::Unbounded;
            }
            if n >= settled && lull < 0.1 * eps * peak {
                break;
            }
        }
        let frames = last + 1 + (period / 4.0).ceil() as u64;
        DecayTime::Finite(Seconds::new(frames as f64 / self.rate.get()))
    }
}

impl Lti for Sos {
    /// The product of the sections' responses, each read from its state-variable form.
    fn frequency_response(&self, output: usize, freqs: &[Hz], gain: &mut [Complex64]) {
        let forms: Vec<_> = self.sections.iter().map(svf_form).collect();
        for (g, f) in gain.iter_mut().zip(freqs) {
            *g = if output == 0 {
                let w = f.angle_per_frame(self.rate).get();
                forms
                    .iter()
                    .fold(Complex64::new(1.0, 0.0), |h, s| h * s.response(w))
            } else {
                Complex64::new(f64::NAN, f64::NAN)
            };
        }
    }

    /// The slowest section's pole modulus: the cascade's poles are the sections'.
    fn spectral_radius(&self) -> f64 {
        self.sections
            .iter()
            .map(Biquad::spectral_radius)
            .fold(0.0, f64::max)
    }

    /// The product of the sections' bounds: the l1 norm of a convolution is at most the
    /// product of its factors'.
    fn l1_norm(&self) -> f64 {
        self.sections.iter().map(section_l1).product()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod eq_tests;

#[cfg(test)]
mod ripple_tests;
