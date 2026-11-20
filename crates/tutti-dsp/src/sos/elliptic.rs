//! The elliptic (Cauer) design, a [`Sos`] in closed form: the selectivity from the degree
//! equation, the prototype's zeros and poles from the Jacobi functions (Orfanidis, "Lecture
//! Notes on Elliptic Filter Design"), the bilinear transform with prewarp, the stages paired
//! into sections.

use core::f64::consts::PI;

use tutti_units::{Hz, SampleRate};

use super::analog::{PassbandRipple, Poles, Stage, StopbandAttenuation, push_stage};
use super::butterworth::check_cutoff;
use super::chebyshev::{dc_gain, ripple_epsilon};
use super::jacobi::{Modulus, cd, complete_k, incomplete_k, sn_cn_dn};
use super::{ButterworthOrder, CutResponse, Sos, SosError};
use crate::Complex64;
use crate::design::prewarp;

/// The two tolerances of an elliptic filter: a passband ripple and a stopband attenuation
/// that is above it, so a transition lies between the bands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EllipticTolerances {
    ripple: PassbandRipple,
    attenuation: StopbandAttenuation,
}

impl EllipticTolerances {
    /// The tolerances `ripple` and `attenuation`; `None` when the attenuation is not above
    /// the ripple.
    #[must_use]
    pub fn new(ripple: PassbandRipple, attenuation: StopbandAttenuation) -> Option<Self> {
        (attenuation.get() > ripple.get()).then_some(Self {
            ripple,
            attenuation,
        })
    }

    /// The passband ripple.
    #[must_use]
    pub fn ripple(self) -> PassbandRipple {
        self.ripple
    }

    /// The stopband attenuation, above the ripple.
    #[must_use]
    pub fn attenuation(self) -> StopbandAttenuation {
        self.attenuation
    }
}

/// An elliptic (Cauer) filter, described without a sample rate: equiripple in the passband
/// and in the stopband, the steepest transition of any order-`N` filter with those ripples.
/// The cutoff is the passband edge, where the gain is exactly `-ripple` dB; the stopband edge
/// follows from the order, the ripple and the attenuation (the degree equation), and from
/// there on the gain stays under `-attenuation` dB.
///
/// DC is 0 dB for an odd order and `-ripple` dB for an even one. Built as
/// `Elliptic::new(response, order, tolerances)`; [`design`](Self::design) binds a
/// cutoff and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Elliptic {
    response: CutResponse,
    order: ButterworthOrder,
    tolerances: EllipticTolerances,
}

impl Elliptic {
    /// An elliptic filter of `order` within `tolerances`.
    #[must_use]
    pub fn new(
        response: CutResponse,
        order: ButterworthOrder,
        tolerances: EllipticTolerances,
    ) -> Self {
        Self {
            response,
            order,
            tolerances,
        }
    }

    /// The selectivity `k = w_passband / w_stopband` the order gives: `0 < k < 1`, nearer 1
    /// the steeper the transition. The solution of the degree equation `N = K(k) K'(k1) /
    /// (K'(k) K(k1))` with the discrimination `k1 = e_ripple / e_attenuation`.
    #[must_use]
    pub fn selectivity(&self) -> f64 {
        self.moduli().0.k()
    }

    /// The stopband edge of the design at the passband edge `cutoff`: the frequency from
    /// which the gain stays under `-attenuation` dB, above `cutoff` for a low-pass and below
    /// it for a high-pass.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design).
    pub fn stopband_edge(&self, cutoff: Hz, rate: SampleRate) -> Result<Hz, SosError> {
        let g = prewarp(check_cutoff(cutoff, rate)?, rate);
        let k = self.selectivity();
        let edge = match self.response {
            CutResponse::LowPass => g / k,
            CutResponse::HighPass => g * k,
        };
        Ok(Hz::new(libm::atan(edge) * rate.get() / PI))
    }

    /// The moduli `k` of the selectivity and `k1` of the discrimination.
    fn moduli(&self) -> (Modulus, Modulus) {
        let (ripple, attenuation) = (
            self.tolerances.ripple.get(),
            self.tolerances.attenuation.get(),
        );
        let e_ripple = ripple_epsilon(ripple);
        let e_attenuation = ripple_epsilon(attenuation);
        let k1 = Modulus::new(e_ripple / e_attenuation);
        // K'(k) / K(k) = K'(k1) / (N K(k1)): the nome of k is the N-th root of the nome of k1.
        let ratio = complete_k(k1.complement()) / complete_k(k1);
        let q = libm::exp(-PI * ratio / f64::from(self.order.get()));
        (Modulus::from_nome(q), k1)
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
        let (m, k1) = self.moduli();
        into.rate = rate;
        into.sections.clear();
        let n = usize::from(self.order.get());
        let k = complete_k(m);
        // The imaginary part of the pole argument, from sn(j y, k1) = j / e_ripple.
        let k1c = k1.complement();
        let y = incomplete_k(
            libm::atan(1.0 / ripple_epsilon(self.tolerances.ripple.get())),
            k1c,
        ) * k
            / (n as f64 * complete_k(k1));
        // The functions of the imaginary argument are read in the complementary modulus.
        let (s1, c1, d1) = sn_cn_dn(y, m.complement());
        let dc = dc_gain(n, self.tolerances.ripple.get());
        if n % 2 == 1 {
            let stage = Stage {
                poles: Poles::Real(-s1 / c1),
                zero: None,
                dc: 1.0,
            };
            push_stage(into, self.response, g, stage);
        }
        for i in (1..=n / 2).rev() {
            let x = (2 * i - 1) as f64 / n as f64 * k;
            let zero = 1.0 / (m.k() * cd(x, m));
            // p = j sn(x + K - j y, k), by the addition formula with the imaginary argument in k'.
            let (s, c, d) = sn_cn_dn(x + k, m);
            let denominator = c1 * c1 + m.k() * m.k() * s * s * s1 * s1;
            let pole = Complex64::new(c * d * s1 * c1, s * d1) / denominator;
            let stage = Stage {
                poles: Poles::Pair(pole),
                zero: Some(zero),
                dc,
            };
            push_stage(into, self.response, g, stage);
        }
        Ok(())
    }
}
