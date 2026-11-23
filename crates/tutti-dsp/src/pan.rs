//! Pan laws: the left and right amplitudes that place a mono source at a [`Pan`].
//!
//! A [`Balance`](tutti_units::Balance) attenuates the side it moves away from
//! ([`Balance::gains`](tutti_units::Balance::gains)); a pan places a source, and
//! the law says how the two gains trade off along the way.

use tutti_buffers::Float;
use tutti_units::{Amplitude, Pan};

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
/// use tutti_units::Pan;
///
/// let (left, right) = pan_gains(Pan::LEFT, PanLaw::Rp036);
/// assert_eq!((left.get(), right.get()), (1.0, 0.0));
/// ```
#[inline]
#[must_use]
pub fn pan_gains(pan: Pan, law: PanLaw) -> (Amplitude, Amplitude) {
    let (left, right) = pan_gains_in::<f64>(pan.get(), law);
    (Amplitude::new(left), Amplitude::new(right))
}

/// The `(left, right)` gains of the position `p` under `law`, as data of the float `F`: the
/// law evaluated at the width the signal runs in. A position outside `-1..=1` reads as the
/// nearer end, and a NaN as the centre; the hard ends of [`PanLaw::Rp036`] are exact.
#[inline]
#[must_use]
pub(crate) fn pan_gains_in<F: Float>(p: F, law: PanLaw) -> (F, F) {
    let p = if p.is_nan() {
        F::ZERO
    } else {
        p.clamp(-F::ONE, F::ONE)
    };
    let half = F::lit(0.5);
    match law {
        PanLaw::SquareRoot => (((F::ONE - p) * half).sqrt(), ((F::ONE + p) * half).sqrt()),
        PanLaw::Rp036 => {
            if p <= -F::ONE {
                (F::ONE, F::ZERO)
            } else if p >= F::ONE {
                (F::ZERO, F::ONE)
            } else {
                let angle = F::PI * F::lit(0.25) * (F::ONE + p);
                (angle.cos(), angle.sin())
            }
        }
    }
}

/// The `(left, right)` gains of the balance `b`, as data of the float `F`:
/// [`Balance::gains`](tutti_units::Balance::gains) evaluated at the width the signal runs in,
/// `left = min(1, 1 - b)` and `right = min(1, 1 + b)`. A balance outside `-1..=1` reads as the
/// nearer end, and a NaN as unity on both sides. Comparisons, not `clamp` and `min`, so the
/// selects vectorise where the NaN-propagating forms do not.
#[inline]
#[must_use]
pub fn balance_gains_in<F: Float>(b: F) -> (F, F) {
    let one = F::ONE;
    let b = if b < -one {
        -one
    } else if b > one {
        one
    } else {
        b
    };
    let at_most_one = |x: F| if x < one { x } else { one };
    (at_most_one(one - b), at_most_one(one + b))
}

#[cfg(test)]
mod tests;
