//! [`Follower`]: a quantity that grows toward a target at one rate and falls back at another.

use tutti_units::{Ballistics, SampleRate, Unit};

use crate::smoother::Smoother;

/// The one-pole coefficients of a follower on a quantity that grows toward a target and falls
/// back: the attack's while the quantity rises and the release's while it falls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Follower {
    attack: f32,
    release: f32,
}

impl Follower {
    /// Holds: a coefficient of 0 closes none of the gap, so a state stays where it is. What a
    /// bank that is not yet aimed starts with; [`of`](Self::of) replaces it before any frame
    /// runs.
    pub(crate) const NEUTRAL: Self = Self {
        attack: 0.0,
        release: 0.0,
    };

    /// The follower of `ballistics` at `rate`.
    pub(crate) fn of(ballistics: Ballistics, rate: SampleRate) -> Self {
        let (attack, release) = ballistics.coeffs(rate);
        Self { attack, release }
    }

    /// One step of `state` toward `target`, in any f32 unit, at the attack's coefficient while the target is
    /// above `state` and at the release's otherwise.
    #[inline]
    pub(crate) fn follow<U: Unit<Raw = f32>>(&self, state: U, target: U) -> U {
        let (state, target) = (state.to_raw(), target.to_raw());
        let coeff = if target > state {
            self.attack
        } else {
            self.release
        };
        U::from_raw(Smoother::toward(state, target, coeff))
    }
}
