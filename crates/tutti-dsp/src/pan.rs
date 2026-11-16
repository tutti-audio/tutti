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
    /// `(0, 1)`. A CC 10 pan is RP-036's; the SoundFont voice uses this law for its
    /// channel and zone pan.
    Rp036,
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
    };
    (Amplitude(left), Amplitude(right))
}

#[cfg(test)]
mod tests;
