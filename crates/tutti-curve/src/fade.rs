use tutti_types::{SampleRate, Samples, Seconds};

use crate::Law;

/// How long a fade lasts in time: 5 ms, so a fade sounds the same at every
/// rate. [`declick_frames`] turns it into frames at a rate. A fade is the one
/// move between two states (a wire connecting, a mode changing, a node
/// swapped, a transport stop, a loop seam); a [ramp](crate::Piece::ramp_at) is
/// continuous interpolation chained block after block and has a length of its
/// own, never this one.
pub const DECLICK: Seconds = Seconds(0.005);

/// The frames of [`DECLICK`] at `rate`: `round(rate * DECLICK)`, at least one.
#[inline]
#[must_use]
pub fn declick_frames(rate: SampleRate) -> Samples {
    Samples(DECLICK.to_samples(rate).get().max(1))
}

/// The gain law of a fade between two signals.
///
/// Both laws are built on the smootherstep `s(x) = x³(6x² − 15x + 10)` of the
/// fade's position `x`, flat in slope and curvature at both ends, so the
/// spectral splatter of a fade falls as `1/f⁴`. The position of frame `k` of
/// an `n`-frame fade is `x = (k + 1) / (n + 1)`, strictly inside `(0, 1)`: no
/// frame of the fade is wholly the old signal or wholly the new one, and the
/// outgoing and incoming gains mirror each other.
///
/// Which law to use is a property of the two signals, not of taste:
/// [`EqualAmplitude`](Self::EqualAmplitude) when they are in phase,
/// [`EqualPower`](Self::EqualPower) when they are not known to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FadeLaw {
    /// `(1 − s, s)`: the gains sum to one. Right for signals in phase (a node
    /// re-declared with a changed config, the same source before and after a
    /// rebuild), which add in amplitude, so an equal-power fade would bump the
    /// level by up to 3 dB mid-fade. A fade from or to silence takes one side
    /// of it.
    #[default]
    EqualAmplitude,
    /// `(cos(πs/2), sin(πs/2))`: the squares of the gains sum to one. Right
    /// for signals not known to be in phase (a swap to another node, a
    /// sample's loop seam), which add in power.
    EqualPower,
}

impl FadeLaw {
    /// `(out, in)`: the gains of the outgoing and the incoming signal at frame
    /// `k` of a fade `n` frames long, `k < n`, at `x = (k + 1) / (n + 1)`.
    ///
    /// # Panics
    ///
    /// In debug builds, if `k >= n`.
    #[inline]
    #[must_use]
    pub fn gains(self, k: usize, n: usize) -> (f32, f32) {
        debug_assert!(k < n, "frame {k} of a {n}-frame fade");
        let s = Law::Smootherstep.eval((k + 1) as f32 / (n + 1) as f32);
        match self {
            Self::EqualAmplitude => (1.0 - s, s),
            Self::EqualPower => {
                let a = s * core::f32::consts::FRAC_PI_2;
                (libm::cosf(a), libm::sinf(a))
            }
        }
    }

    /// The value `from` moved to `to` at frame `k` of an `n`-frame fade:
    /// `from * out + to * in`, and exactly `to` from frame `n` on.
    #[inline]
    #[must_use]
    pub fn between(self, from: f32, to: f32, k: usize, n: usize) -> f32 {
        if k >= n {
            return to;
        }
        let (out, inn) = self.gains(k, n);
        from * out + to * inn
    }
}

#[cfg(test)]
mod tests;
