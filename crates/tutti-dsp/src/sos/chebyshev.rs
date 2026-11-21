//! The Chebyshev type I and type II designs, each a [`Sos`] in closed form: the analog
//! prototype's poles (and, for type II, zeros) from `sinh` and `cosh`, the bilinear transform
//! with prewarp, the stages paired into sections.

use core::f64::consts::PI;

use tutti_units::{Db, Hz, SampleRate};

use super::analog::{PassbandRipple, Poles, Stage, StopbandAttenuation, push_stage};
use super::butterworth::check_cutoff;
use super::{ButterworthOrder, CutResponse, Sos, SosError};
use crate::Complex64;
use crate::design::prewarp;

/// A Chebyshev type I filter, described without a sample rate: equiripple in the passband,
/// monotone in the stopband, `|H|^2 = 1 / (1 + e^2 c(w / wc)^2)` for the Chebyshev
/// polynomial `c` of the order `N` and `e^2 = 10^(ripple / 10) - 1`, with the cutoff the passband
/// edge, where the gain is exactly `-ripple` dB.
///
/// The passband holds `N` extrema alternating between 0 dB and `-ripple` dB; DC is 0 dB for
/// an odd order and `-ripple` dB for an even one. Built as `ChebyshevI::new(response, order,
/// ripple)`; [`design`](Self::design) binds a cutoff and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChebyshevI {
    response: CutResponse,
    order: ButterworthOrder,
    ripple: PassbandRipple,
}

impl ChebyshevI {
    /// A Chebyshev I filter of `order` with `ripple` in its passband.
    #[must_use]
    pub fn new(response: CutResponse, order: ButterworthOrder, ripple: PassbandRipple) -> Self {
        Self {
            response,
            order,
            ripple,
        }
    }

    /// Designs the filter at the passband edge `cutoff` and `rate`: `ceil(order / 2)`
    /// sections, a first-order one first for an odd order, the poles nearest the unit circle
    /// last.
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
        let g = prewarp(check_cutoff(cutoff, rate)?, rate);
        into.rate = rate;
        into.sections.clear();
        let n = usize::from(self.order.get());
        let a = libm::asinh(1.0 / ripple_epsilon(self.ripple.get())) / n as f64;
        let dc = dc_gain(n, self.ripple.get());
        let (sinh, cosh) = (libm::sinh(a), libm::cosh(a));
        if n % 2 == 1 {
            let stage = Stage {
                poles: Poles::Real(-sinh),
                zero: None,
                dc: 1.0,
            };
            push_stage(into, self.response, g, stage);
        }
        for k in (1..=n / 2).rev() {
            let theta = chebyshev_angle(k, n);
            let pole = Complex64::new(-sinh * libm::sin(theta), cosh * libm::cos(theta));
            let stage = Stage {
                poles: Poles::Pair(pole),
                zero: None,
                dc,
            };
            push_stage(into, self.response, g, stage);
        }
        Ok(())
    }
}

/// A Chebyshev type II (inverse Chebyshev) filter, described without a sample rate:
/// monotone in the passband, equiripple in the stopband, `|H|^2 = 1 / (1 + 1 / (e^2
/// c(wc / w)^2))` for the order `N` and `e^2 = 1 / (10^(attenuation / 10) - 1)`, with the
/// cutoff the stopband edge, where the gain is exactly `-attenuation` dB.
///
/// The zeros are on the imaginary axis at `wc / cos((2k - 1) pi / 2N)` (the secants, mapped
/// through the prewarp); DC is 0 dB. Built as `ChebyshevII::new(response, order,
/// attenuation)`; [`design`](Self::design) binds a cutoff and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChebyshevII {
    response: CutResponse,
    order: ButterworthOrder,
    attenuation: StopbandAttenuation,
}

impl ChebyshevII {
    /// A Chebyshev II filter of `order` whose stopband stays `attenuation` down.
    #[must_use]
    pub fn new(
        response: CutResponse,
        order: ButterworthOrder,
        attenuation: StopbandAttenuation,
    ) -> Self {
        Self {
            response,
            order,
            attenuation,
        }
    }

    /// Designs the filter at the stopband edge `cutoff` and `rate`: `ceil(order / 2)`
    /// sections, a first-order one first for an odd order, the poles nearest the unit circle
    /// last.
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
        let g = prewarp(check_cutoff(cutoff, rate)?, rate);
        into.rate = rate;
        into.sections.clear();
        let n = usize::from(self.order.get());
        // The inverse of the type I poles with e = sqrt(10^(A/10) - 1) in place of 1 / e.
        let power = self.attenuation.get().to_amplitude().get().powi(2);
        let a = libm::asinh(libm::sqrt(power - 1.0)) / n as f64;
        let (sinh, cosh) = (libm::sinh(a), libm::cosh(a));
        if n % 2 == 1 {
            let stage = Stage {
                poles: Poles::Real(-1.0 / sinh),
                zero: None,
                dc: 1.0,
            };
            push_stage(into, self.response, g, stage);
        }
        for k in (1..=n / 2).rev() {
            let theta = chebyshev_angle(k, n);
            let type_one = Complex64::new(-sinh * libm::sin(theta), cosh * libm::cos(theta));
            let stage = Stage {
                poles: Poles::Pair(Complex64::new(1.0, 0.0) / type_one),
                zero: Some(1.0 / libm::cos(theta)),
                dc: 1.0,
            };
            push_stage(into, self.response, g, stage);
        }
        Ok(())
    }
}

/// `(2k - 1) pi / 2N`: the angle of the `k`-th Chebyshev root.
fn chebyshev_angle(k: usize, n: usize) -> f64 {
    PI * (2 * k - 1) as f64 / (2 * n) as f64
}

/// The ripple factor `e = sqrt(10^(ripple / 10) - 1)`.
pub(super) fn ripple_epsilon(ripple: Db) -> f64 {
    libm::sqrt(ripple.to_amplitude().get().powi(2) - 1.0)
}

/// The DC gain each of the `ceil(n / 2)` stages of an order-`n` prototype of ripple `ripple`
/// takes: the whole filter's gain, 1 for an odd order and `-ripple` dB for an even one, split
/// evenly over its pairs.
pub(super) fn dc_gain(n: usize, ripple: Db) -> f64 {
    if n % 2 == 1 {
        1.0
    } else {
        libm::pow(Db::new(-ripple.get()).to_amplitude().get(), 2.0 / n as f64)
    }
}
