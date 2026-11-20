//! The Butterworth and Linkwitz-Riley designs, each a [`Sos`] in closed form: the analog
//! prototype's poles, the bilinear transform with prewarp, the poles paired into sections.

use core::f64::consts::PI;

use tutti_units::{Db, Hz, Q, SampleRate};

use super::Sos;
use crate::design::prewarp;
use crate::{Biquad, Complex64};

tutti_units::ranged! {
    /// The order of a Butterworth filter, `1..=16`: `6 * order` dB per octave, up to 96. A
    /// [`LinkwitzRiley`] filter of this order halves has twice it.
    ButterworthOrder(u8), ButterworthOrderError, "an order within 1..=16",
    |raw| raw >= 1 && raw <= 16
}

/// What a [`Butterworth`] or [`LinkwitzRiley`] passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CutResponse {
    /// Passes below the cutoff, a gain of 1 at DC.
    LowPass,
    /// Passes above the cutoff, a gain of 1 at Nyquist.
    HighPass,
}

/// What the [`Sos`] designs refuse: a cutoff or center that is not inside `(0, Nyquist)`,
/// which depends on the rate, a quality factor that is not above 0, or a gain beyond
/// [`MAX_GAIN_MAGNITUDE`].
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SosError {
    /// A cutoff at or below 0 Hz, at or above Nyquist, or not a number.
    #[error("cutoff {cutoff:?} is not inside (0, {nyquist:?})")]
    CutoffOutOfRange {
        /// The rejected cutoff.
        cutoff: Hz,
        /// The Nyquist frequency of the rate.
        nyquist: Hz,
    },
    /// A quality factor that is not finite and above 0.
    #[error("quality factor {q:?} is not finite and above 0")]
    QOutOfRange {
        /// The rejected quality factor.
        q: Q,
    },
    /// A gain that is not within [`MAX_GAIN_MAGNITUDE`] of 0 dB, or not a number.
    #[error("gain {gain:?} is not within {} dB of unity", MAX_GAIN_MAGNITUDE.get())]
    GainOutOfRange {
        /// The rejected gain.
        gain: Db,
    },
}

/// The largest gain magnitude a bell or tilt design takes: 120 dB, past which a linear
/// amplitude is no longer a usable figure.
pub const MAX_GAIN_MAGNITUDE: Db = Db::new(120.0);

/// `cutoff` when it is inside `(0, Nyquist)` of `rate`.
pub(super) fn check_cutoff(cutoff: Hz, rate: SampleRate) -> Result<Hz, SosError> {
    let nyquist = rate.nyquist();
    if cutoff.get() > 0.0 && cutoff < nyquist {
        Ok(cutoff)
    } else {
        Err(SosError::CutoffOutOfRange { cutoff, nyquist })
    }
}

/// `gain` when it is within [`MAX_GAIN_MAGNITUDE`].
pub(super) fn check_gain(gain: Db) -> Result<Db, SosError> {
    if gain.get().abs() <= MAX_GAIN_MAGNITUDE.get() {
        Ok(gain)
    } else {
        Err(SosError::GainOutOfRange { gain })
    }
}

/// `q` when it is finite and above 0.
pub(super) fn check_q(q: Q) -> Result<Q, SosError> {
    if q.get().is_finite() && q.get() > 0.0 {
        Ok(q)
    } else {
        Err(SosError::QOutOfRange { q })
    }
}

/// The root of the amplitude gain of `db` decibels, `10^(db / 40)`: the `A` of a bell and a tilt.
pub(super) fn gain_root(db: Db) -> f64 {
    Db::new(db.get() / 2.0).to_amplitude().get()
}

/// A Butterworth filter, described without a sample rate: maximally flat in the passband,
/// `|H|^2 = 1 / (1 + (tan(w/2) / g)^(2N))` for the order `N` and `g = tan(pi fc / fs)`
/// (the analog `1 / (1 + (w / wc)^(2N))` at the prewarped frequency), `-3 dB` at the
/// cutoff and `6 N` dB per octave.
///
/// Built as `Butterworth::new(response, order)`; [`design`](Self::design) binds a cutoff
/// and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Butterworth {
    response: CutResponse,
    order: ButterworthOrder,
}

impl Butterworth {
    /// A Butterworth filter of `order`.
    #[must_use]
    pub fn new(response: CutResponse, order: ButterworthOrder) -> Self {
        Self { response, order }
    }

    /// Designs the filter at `cutoff` and `rate`: `order / 2` second-order sections, the
    /// poles nearest the unit circle last, and a first-order one first for an odd order.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for a cutoff outside `(0, Nyquist)`.
    pub fn design(&self, cutoff: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(usize::from(self.order.get())), rate);
        self.design_into(cutoff, rate, &mut sos)?;
        Ok(sos)
    }

    /// [`design`](Self::design) into `into`, reusing its sections' storage: no allocation
    /// when it holds room for them.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design); `into` is then as it was.
    pub fn design_into(
        &self,
        cutoff: Hz,
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        cascade(self.response, self.order.get(), 1, cutoff, rate, into)
    }
}

/// A Linkwitz-Riley filter, described without a sample rate: a Butterworth filter of half
/// the order cascaded with itself, so `-6 dB` at the cutoff and `6 N` dB per octave for the
/// order `N`. `LinkwitzRiley::new(response, ButterworthOrder::of(8))` is LR96, Butterworth order 8
/// squared.
///
/// A low-pass and a high-pass of one order sum to a flat magnitude when the order (twice
/// the half) is a multiple of 4 (they are in phase); at the others (2, 6, 10, 14) they are in antiphase
/// and their difference is the allpass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkwitzRiley {
    response: CutResponse,
    order: ButterworthOrder,
}

impl LinkwitzRiley {
    /// A Linkwitz-Riley filter of twice `half` the order: `half` is the Butterworth order it
    /// squares.
    #[must_use]
    pub fn new(response: CutResponse, half: ButterworthOrder) -> Self {
        Self {
            response,
            order: half,
        }
    }

    /// Designs the filter at `cutoff` and `rate`: each section of the Butterworth filter of
    /// half the order twice over.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] as for a
    /// [`Butterworth`] design.
    pub fn design(&self, cutoff: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(2 * usize::from(self.order.get())), rate);
        self.design_into(cutoff, rate, &mut sos)?;
        Ok(sos)
    }

    /// [`design`](Self::design) into `into`, reusing its sections' storage.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design); `into` is then as it was.
    pub fn design_into(
        &self,
        cutoff: Hz,
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        cascade(self.response, self.order.get(), 2, cutoff, rate, into)
    }
}

/// The sections of a Butterworth filter of `order`, each `repeats` times, into `into`.
///
/// The analog poles are `exp(j pi (2m + N + 1) / 2N)` on the unit circle; the cutoff `g =
/// tan(pi fc / fs)` scales them (low-pass `s = g p`, high-pass `s = g / p`) and the bilinear
/// map `z = (1 + s) / (1 - s)` takes them to the plane. A conjugate pair is the section
/// `1 + a1 z^-1 + a2 z^-2` with `a1 = -2 Re z`, `a2 = |z|^2`, over the zeros at `-1`
/// (low-pass) or `1` scaled to a gain of 1 at DC or Nyquist.
fn cascade(
    response: CutResponse,
    order: u8,
    repeats: usize,
    cutoff: Hz,
    rate: SampleRate,
    into: &mut Sos,
) -> Result<(), SosError> {
    let fc = check_cutoff(cutoff, rate)?;
    let g = prewarp(fc, rate);
    let n = usize::from(order);
    let section = |a1: f64, a2: f64, first: bool| {
        let (gain, sign) = match response {
            CutResponse::LowPass => ((1.0 + a1 + a2) / if first { 2.0 } else { 4.0 }, 1.0),
            CutResponse::HighPass => ((1.0 - a1 + a2) / if first { 2.0 } else { 4.0 }, -1.0),
        };
        if first {
            // The first-order sections have a2 = 0: gain = (1 +- a1) / 2.
            Biquad {
                b0: gain,
                b1: sign * gain,
                b2: 0.0,
                a1,
                a2: 0.0,
            }
        } else {
            Biquad {
                b0: gain,
                b1: sign * 2.0 * gain,
                b2: gain,
                a1,
                a2,
            }
        }
    };
    into.rate = rate;
    into.sections.clear();
    let mut push = |b: Biquad| (0..repeats).for_each(|_| into.sections.push(b));
    if n % 2 == 1 {
        // The pole at -1, mapped to z = (1 - g) / (1 + g) either way.
        push(section(-(1.0 - g) / (1.0 + g), 0.0, true));
    }
    for m in (0..n / 2).rev() {
        let theta = PI * (2 * m + 1) as f64 / (2 * n) as f64;
        let p = Complex64::new(-libm::sin(theta), libm::cos(theta));
        let s = match response {
            CutResponse::LowPass => p * g,
            CutResponse::HighPass => Complex64::new(g, 0.0) / p,
        };
        let z = (1.0 + s) / (1.0 - s);
        push(section(-2.0 * z.re, z.norm_sqr(), false));
    }
    Ok(())
}
