//! What the Chebyshev and elliptic designs share: the ripple and attenuation settings, and
//! the bilinear map of one analog low-pass prototype stage into a [`Biquad`].

use tutti_units::Db;

use super::{CutResponse, Sos};
use crate::{Biquad, Complex64};

tutti_units::ranged! {
    /// The passband ripple of a Chebyshev I or elliptic filter: above 0 dB and at most 20 dB,
    /// the peak-to-peak deviation of the passband from unity.
    PassbandRipple(Db), PassbandRippleError, "a ripple above 0 dB and at most 20 dB",
    |raw| raw.get() > 0.0 && raw.get() <= 20.0
}

tutti_units::ranged! {
    /// The stopband attenuation of a Chebyshev II or elliptic filter: above 0 dB and at most
    /// 150 dB, the depth the stopband stays under.
    StopbandAttenuation(Db), StopbandAttenuationError, "an attenuation above 0 dB and at most 150 dB",
    |raw| raw.get() > 0.0 && raw.get() <= 150.0
}

/// The poles of one analog stage, in the left half plane.
#[derive(Clone, Copy, Debug)]
pub(super) enum Poles {
    /// A real pole `p < 0`: a first-order stage.
    Real(f64),
    /// The conjugate pair of `p` (`Im p > 0`).
    Pair(Complex64),
}

/// One stage of an analog low-pass prototype normalized to the edge 1 rad/s: its poles, the
/// frequency of its zero pair on the imaginary axis (`None` for zeros at infinity) and its
/// gain at DC.
#[derive(Clone, Copy, Debug)]
pub(super) struct Stage {
    pub(super) poles: Poles,
    pub(super) zero: Option<f64>,
    pub(super) dc: f64,
}

/// Appends the digital stage of `stage` to `into`, the prototype's edge at `g = tan(pi fc /
/// fs)`.
///
/// The low-pass is `s = g p` and the high-pass `s = g / p` (a zero at infinity goes to
/// `z = -1` and to `z = 1`); the map `z = (1 + s) / (1 - s)` takes poles and zeros to the
/// plane. The numerator is scaled to the stage's DC gain at `z = 1` (low-pass) or its gain at
/// Nyquist (high-pass), the images of `s = 0` and of `s = infinity`.
pub(super) fn push_stage(into: &mut Sos, response: CutResponse, g: f64, stage: Stage) {
    let low = response == CutResponse::LowPass;
    let map = |p: Complex64| {
        let s = if low {
            p * g
        } else {
            Complex64::new(g, 0.0) / p
        };
        (1.0 + s) / (1.0 - s)
    };
    // `z^-1` where the numerator is scaled: 1 at DC, -1 at Nyquist.
    let at = if low { 1.0 } else { -1.0 };
    let biquad = match stage.poles {
        Poles::Real(p) => {
            let a1 = -map(Complex64::new(p, 0.0)).re;
            let b0 = stage.dc * (1.0 + a1 * at) / 2.0;
            Biquad {
                b0,
                b1: b0 * at,
                b2: 0.0,
                a1,
                a2: 0.0,
            }
        }
        Poles::Pair(p) => {
            let z = map(p);
            let (a1, a2) = (-2.0 * z.re, z.norm_sqr());
            let n1 = match stage.zero {
                None => 2.0 * at,
                Some(w) => {
                    let t = if low { g * w } else { g / w };
                    -2.0 * (1.0 - t * t) / (1.0 + t * t)
                }
            };
            let b0 = stage.dc * (1.0 + a1 * at + a2) / (2.0 + n1 * at);
            Biquad {
                b0,
                b1: b0 * n1,
                b2: b0,
                a1,
                a2,
            }
        }
    };
    into.sections.push(biquad);
}
