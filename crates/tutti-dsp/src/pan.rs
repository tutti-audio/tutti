//! Pan laws: the left and right amplitudes that place a mono source at a [`Pan`].
//!
//! A [`Balance`](tutti_types::Balance) attenuates the side it moves away from
//! ([`Balance::gains`](tutti_types::Balance::gains)); a pan places a source, and
//! the law says how the two gains trade off along the way.

use tutti_types::{Amplitude, Pan};

/// How a [`Pan`] position splits one source over left and right.
///
/// Each variant is a distinct law; a law a specification fixes is not replaced
/// by another that is close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PanLaw {
    /// `left = sqrt((1 - p) / 2)`, `right = sqrt((1 + p) / 2)`: an equal-power
    /// law (`left² + right² = 1`) with `sqrt(0.5)`, -3 dB, in each side at centre.
    SquareRoot,
    /// MIDI RP-036 (Default Pan Formula): `left = cos(π/4 (1 + p))`,
    /// `right = sin(π/4 (1 + p))`, an equal-power law with `cos(π/4)`, -3 dB, in
    /// each side at centre. Hard left is exactly `(1, 0)` and hard right exactly
    /// `(0, 1)`. The SoundFont 2 pan generator is placed by the same law.
    Rp036,
    /// [`Rp036`](Self::Rp036) scaled by `sqrt(2)`: a pair of gains that leaves a
    /// source's own placement alone at centre, exactly `(1, 1)` there, and
    /// carries RP-036's ratio of the two sides everywhere else. For a channel's
    /// pan applied on top of a voice's own law.
    Rp036UnityCentre,
}

/// The `(left, right)` amplitudes of `pan` under `law`.
///
/// A pan outside `-1..=1` reads as the nearer end, and a NaN as the centre.
///
/// ```
/// use tutti_dsp::{PanLaw, pan_gains};
/// use tutti_types::Pan;
///
/// let (left, right) = pan_gains(Pan::LEFT, PanLaw::Rp036);
/// assert_eq!((left.get(), right.get()), (1.0, 0.0));
/// ```
#[inline]
#[must_use]
pub fn pan_gains(pan: Pan, law: PanLaw) -> (Amplitude, Amplitude) {
    let p = if pan.get().is_nan() {
        0.0
    } else {
        pan.get().clamp(-1.0, 1.0)
    };
    let (left, right) = match law {
        PanLaw::SquareRoot => (libm::sqrtf((1.0 - p) * 0.5), libm::sqrtf((1.0 + p) * 0.5)),
        PanLaw::Rp036 => {
            if p <= -1.0 {
                (1.0, 0.0)
            } else if p >= 1.0 {
                (0.0, 1.0)
            } else {
                let (sin, cos) = libm::sincosf(core::f32::consts::FRAC_PI_4 * (1.0 + p));
                (cos, sin)
            }
        }
        PanLaw::Rp036UnityCentre => {
            // `sqrt(2) cos(π/4 + d)` is `cos d - sin d` and `sqrt(2) sin(π/4 + d)` is
            // `cos d + sin d`, with `d = π/4 p` the angle off the centre: written so,
            // the centre is 1 exactly, where `sqrt(2) cos(π/4)` is one ulp short in `f32`.
            let (sin, cos) = libm::sincosf(core::f32::consts::FRAC_PI_4 * p);
            (cos - sin, cos + sin)
        }
    };
    (Amplitude(left), Amplitude(right))
}

#[cfg(test)]
mod tests;
