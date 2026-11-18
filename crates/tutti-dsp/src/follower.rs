//! [`Follower`]: a quantity that grows toward a target at one rate and falls back at another.

use tutti_buffers::Float;
use tutti_units::{Ballistics, SampleRate, Unit};

use crate::smoother::Smoother;

/// The one-pole coefficients of a follower on a quantity that grows toward a target and falls
/// back: the attack's while the quantity rises and the release's while it falls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Follower<F> {
    attack: F,
    release: F,
}

impl<F: Float> Follower<F> {
    /// Holds: a coefficient of 0 closes none of the gap, so a state stays where it is. What a
    /// bank that is not yet aimed starts with; [`of`](Self::of) replaces it before any frame
    /// runs.
    pub(crate) const NEUTRAL: Self = Self {
        attack: F::ZERO,
        release: F::ZERO,
    };

    /// The follower of `ballistics` at `rate`.
    pub(crate) fn of(ballistics: Ballistics, rate: SampleRate) -> Self {
        let (attack, release) = ballistics.coeffs(rate);
        Self {
            attack: F::from_f64(attack),
            release: F::from_f64(release),
        }
    }

    /// One step of `state` toward `target`, for any unit whose raw is `F`, at the attack's coefficient while the target is
    /// above `state` and at the release's otherwise.
    #[inline]
    pub(crate) fn follow<U: Unit<Raw = F>>(&self, state: U, target: U) -> U {
        U::from_raw(self.follow_raw(state.to_raw(), target.to_raw()))
    }

    /// [`follow`](Self::follow) over the bare float, for a quantity no unit of raw `F` covers
    /// yet; it goes when `Db` and `Amplitude` store `f64`.
    #[inline]
    pub(crate) fn follow_raw(&self, state: F, target: F) -> F {
        let coeff = if target > state {
            self.attack
        } else {
            self.release
        };
        Smoother::toward(state, target, coeff)
    }
}

#[cfg(test)]
mod tests;
