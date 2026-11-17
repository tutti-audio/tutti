//! The Butterworth and Linkwitz-Riley designs, each a [`Sos`] in closed form: the analog
//! prototype's poles, the bilinear transform with prewarp, the poles paired into sections.

use core::f64::consts::PI;

use tutti_types::{Hz, SampleRate};

use super::Sos;
use crate::{Biquad, Complex64};

/// The highest order a design takes: 96 dB per octave.
pub const MAX_ORDER: u8 = 16;

/// What a [`Butterworth`] or [`LinkwitzRiley`] passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SosResponse {
    /// Passes below the cutoff, a gain of 1 at DC.
    LowPass,
    /// Passes above the cutoff, a gain of 1 at Nyquist.
    HighPass,
}

/// What [`Butterworth::design`] and [`LinkwitzRiley::design`] refuse.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum SosError {
    /// An order below the design's lowest or above [`MAX_ORDER`].
    #[error("order {order} is not in {min}..={MAX_ORDER}")]
    OrderOutOfRange {
        /// The rejected order.
        order: u8,
        /// The design's lowest order.
        min: u8,
    },
    /// An odd Linkwitz-Riley order: its order is twice a Butterworth one.
    #[error("Linkwitz-Riley order {0} is not even")]
    OrderOdd(u8),
    /// A cutoff at or below 0 Hz, at or above Nyquist, or not a number.
    #[error("cutoff {cutoff:?} is not inside (0, {nyquist:?})")]
    CutoffOutOfRange {
        /// The rejected cutoff.
        cutoff: Hz,
        /// The Nyquist frequency of the rate.
        nyquist: Hz,
    },
}

/// A Butterworth filter, described without a sample rate: maximally flat in the passband,
/// `|H|^2 = 1 / (1 + (tan(w/2) / g)^(2N))` for the order `N` and `g = tan(pi fc / fs)`
/// (the analog `1 / (1 + (w / wc)^(2N))` at the prewarped frequency), `-3 dB` at the
/// cutoff and `6 N` dB per octave.
///
/// Built as `Butterworth::new(response, order)`; [`design`](Self::design) binds a cutoff
/// and the rate. As scipy's `butter(order, cutoff, output='sos')`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Butterworth {
    response: SosResponse,
    order: u8,
}

impl Butterworth {
    /// A Butterworth filter of `order` (1 to [`MAX_ORDER`]).
    #[must_use]
    pub fn new(response: SosResponse, order: u8) -> Self {
        Self { response, order }
    }

    /// Designs the filter at `cutoff` and `rate`: `order / 2` second-order sections, the
    /// poles nearest the unit circle last, and a first-order one first for an odd order.
    ///
    /// # Errors
    ///
    /// [`SosError::OrderOutOfRange`] for an order outside `1..=`[`MAX_ORDER`] and
    /// [`SosError::CutoffOutOfRange`] for a cutoff outside `(0, Nyquist)`.
    pub fn design(&self, cutoff: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(usize::from(self.order)), rate);
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
        cascade(self.response, self.order, 1, cutoff, rate, into)
    }
}

/// A Linkwitz-Riley filter, described without a sample rate: a Butterworth filter of half
/// the order cascaded with itself, so `-6 dB` at the cutoff and `6 N` dB per octave for the
/// order `N`. `LinkwitzRiley::new(response, 16)` is LR96, Butterworth order 8 squared.
///
/// A low-pass and a high-pass of one order sum to a flat magnitude when the order is a
/// multiple of 4 (they are in phase); at the others (2, 6, 10, 14) they are in antiphase
/// and their difference is the allpass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkwitzRiley {
    response: SosResponse,
    order: u8,
}

impl LinkwitzRiley {
    /// A Linkwitz-Riley filter of `order` (even, 2 to [`MAX_ORDER`]).
    #[must_use]
    pub fn new(response: SosResponse, order: u8) -> Self {
        Self { response, order }
    }

    /// Designs the filter at `cutoff` and `rate`: each section of the Butterworth filter of
    /// half the order twice over.
    ///
    /// # Errors
    ///
    /// [`SosError::OrderOdd`] and [`SosError::OrderOutOfRange`] for an order that is odd or
    /// outside `2..=`[`MAX_ORDER`], and [`SosError::CutoffOutOfRange`] as for a
    /// [`Butterworth`] design.
    pub fn design(&self, cutoff: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(usize::from(self.order)), rate);
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
        if self.order % 2 == 1 {
            return Err(SosError::OrderOdd(self.order));
        }
        if !(2..=MAX_ORDER).contains(&self.order) {
            return Err(SosError::OrderOutOfRange {
                order: self.order,
                min: 2,
            });
        }
        cascade(self.response, self.order / 2, 2, cutoff, rate, into)
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
    response: SosResponse,
    order: u8,
    repeats: usize,
    cutoff: Hz,
    rate: SampleRate,
    into: &mut Sos,
) -> Result<(), SosError> {
    if !(1..=MAX_ORDER).contains(&order) {
        return Err(SosError::OrderOutOfRange { order, min: 1 });
    }
    let nyquist = rate.get() / 2.0;
    let fc = f64::from(cutoff.get());
    if !(fc > 0.0 && fc < nyquist) {
        return Err(SosError::CutoffOutOfRange {
            cutoff,
            nyquist: Hz(nyquist as f32),
        });
    }
    let g = libm::tan(PI * fc / rate.get());
    let n = usize::from(order);
    let section = |a1: f64, a2: f64, first: bool| {
        let (gain, sign) = match response {
            SosResponse::LowPass => ((1.0 + a1 + a2) / if first { 2.0 } else { 4.0 }, 1.0),
            SosResponse::HighPass => ((1.0 - a1 + a2) / if first { 2.0 } else { 4.0 }, -1.0),
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
            SosResponse::LowPass => p * g,
            SosResponse::HighPass => Complex64::new(g, 0.0) / p,
        };
        let z = (1.0 + s) / (1.0 - s);
        push(section(-2.0 * z.re, z.norm_sqr(), false));
    }
    Ok(())
}
