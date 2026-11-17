//! [`PeakFollower`]: a level followed by a one-pole attack and release.

use tutti_types::{SampleRate, Seconds};

use crate::Smoother;

/// The envelope of a level (an amplitude, at least 0) by a one-pole smoother with an attack
/// while the level is above it and a release while it is below:
///
/// `env <- env + c * (level - env)`, `c = 1 - exp(-1 / (tau * rate))` ([`Seconds::one_pole_coeff`]),
/// `tau` the attack or the release.
///
/// After `n` frames of a constant level `L` from `e0`, the envelope is `L + (e0 - L) a^n` with
/// `a = 1 - c = exp(-1 / (tau * rate))`: the closed form every follower test here uses. A time
/// of 0 follows the level exactly and an infinite one holds it.
///
/// The state is `f32`. A follower is at 0 until it hears a level, and holds its coefficients
/// as instant (`c = 1`) until [`set`](Self::set) gives it times.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeakFollower {
    env: f32,
    attack: f32,
    release: f32,
}

impl PeakFollower {
    /// A follower at 0 that follows its level exactly.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            env: 0.0,
            attack: 1.0,
            release: 1.0,
        }
    }

    /// This follower's times at `rate`; the envelope is kept.
    pub fn set(&mut self, attack: Seconds, release: Seconds, rate: SampleRate) {
        self.attack = attack.one_pole_coeff(rate);
        self.release = release.one_pole_coeff(rate);
    }

    /// One frame's `level` into the envelope, which is returned. `level` is at least 0 and not
    /// NaN: a NaN would hold the envelope there for good.
    #[inline]
    pub fn follow(&mut self, level: f32) -> f32 {
        let coeff = if level > self.env {
            self.attack
        } else {
            self.release
        };
        self.env = Smoother::toward(self.env, level, coeff);
        self.env
    }

    /// The envelope.
    #[must_use]
    pub fn value(&self) -> f32 {
        self.env
    }

    /// Back to 0, the times kept.
    pub fn reset(&mut self) {
        self.env = 0.0;
    }
}

impl Default for PeakFollower {
    fn default() -> Self {
        Self::new()
    }
}
