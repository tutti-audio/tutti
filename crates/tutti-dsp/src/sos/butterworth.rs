//! The Butterworth and Linkwitz-Riley designs, each a [`Sos`] in closed form: the analog
//! prototype's poles, the bilinear transform with prewarp, the poles paired into sections.

use core::f64::consts::PI;

use tutti_units::{Db, Hz, Q, SampleRate};

use super::Sos;
use super::analog::{Poles, Stage, push_stage};
use crate::Complex64;
use crate::design::prewarp;

tutti_units::ranged! {
    /// The order of a Butterworth, Chebyshev or elliptic filter, `1..=16`: `6 * order` dB per
    /// octave of asymptotic slope, up to 96. A [`LinkwitzRiley`] filter of this order halves
    /// has twice it.
    ButterworthOrder(u8), ButterworthOrderError, "an order within 1..=16",
    |raw| raw >= 1 && raw <= 16
}

/// What a [`Butterworth`], [`ChebyshevI`](super::ChebyshevI), [`ChebyshevII`](super::ChebyshevII),
/// [`Elliptic`](super::Elliptic) or [`LinkwitzRiley`]
/// passes.
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
/// The analog poles are `exp(j pi (2m + N + 1) / 2N)` on the unit circle, each stage of unit
/// DC gain and with its zeros at infinity; [`push_stage`] takes them to the plane.
fn cascade(
    response: CutResponse,
    order: u8,
    repeats: usize,
    cutoff: Hz,
    rate: SampleRate,
    into: &mut Sos,
) -> Result<(), SosError> {
    let g = prewarp(check_cutoff(cutoff, rate)?, rate);
    let n = usize::from(order);
    into.rate = rate;
    into.sections.clear();
    let mut push = |poles: Poles| {
        let at = into.sections.len();
        push_stage(
            into,
            response,
            g,
            Stage {
                poles,
                zero: None,
                dc: 1.0,
            },
        );
        let stage = into.sections[at];
        (1..repeats).for_each(|_| into.sections.push(stage));
    };
    if n % 2 == 1 {
        push(Poles::Real(-1.0));
    }
    for m in (0..n / 2).rev() {
        let theta = PI * (2 * m + 1) as f64 / (2 * n) as f64;
        push(Poles::Pair(Complex64::new(
            -libm::sin(theta),
            libm::cos(theta),
        )));
    }
    Ok(())
}
