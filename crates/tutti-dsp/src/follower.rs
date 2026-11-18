//! [`Smoothed`], [`Pole`] and [`Follower`]: a quantity that moves toward a target by a
//! one-pole step, at one rate growing and another falling.

use tutti_buffers::Float;
use tutti_units::{Ballistics, SampleRate, Seconds};

/// A quantity a [`Pole`] smooths: its value and the part of the last steps the value's
/// rounding dropped.
///
/// A step lands on the value rounded to the float's ulp. When a step is smaller than half an
/// ulp of the value (a coefficient under `2^-24` of the gap, a time constant past a few
/// million frames) the plain recurrence `s + c (t - s)` adds nothing, and a smoother that
/// should release freezes. The dropped part is kept here and added to the next step, so the
/// value follows the one-pole to the rounding of the value itself however long the time
/// constant (error feedback, as in Kahan's compensated sum).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Smoothed<F> {
    value: F,
    dropped: F,
}

impl<F: Float> Smoothed<F> {
    /// At 0 with nothing dropped.
    pub(crate) const ZERO: Self = Self::at(F::ZERO);

    /// At `value` with nothing dropped.
    pub(crate) const fn at(value: F) -> Self {
        Self {
            value,
            dropped: F::ZERO,
        }
    }

    /// The value.
    #[inline]
    pub(crate) fn value(&self) -> F {
        self.value
    }
}

/// The coefficient of a one-pole step at the float `F`, stored in the form that keeps its
/// relative precision, and the step it makes on a [`Smoothed`].
///
/// A step closes the fraction `c` of the gap to the target. A small `c` (a long time
/// constant) is stored as it is and applied as `s + (c (t - s) + dropped)`: the correction and
/// what earlier steps dropped are summed first and added to the value once, and what that
/// addition drops is kept for the next step. A `c` over one half (a short time constant) is
/// stored as its complement `r`, the fraction of the gap the step leaves, designed directly
/// from the time constant, and applied as `t + r (s - t)`: `1 - c` would have rounded `r`
/// away, where `r` itself keeps full relative precision, and the step has no small term to drop.
/// Both cross into `F` once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Pole<F> {
    /// `c`, the fraction of the gap one step closes, at most one half.
    Closes(F),
    /// `r`, the fraction of the gap one step leaves, under one half.
    Leaves(F),
}

impl<F: Float> Pole<F> {
    /// Holds: a step closes none of the gap, so a state stays where it is. What a bank that is
    /// not yet aimed starts with.
    pub(crate) const HOLD: Self = Self::Closes(F::ZERO);

    /// Instant: a step lands on the target.
    pub(crate) const INSTANT: Self = Self::Leaves(F::ZERO);

    /// The pole of a one-pole smoother with time constant `tau` stepped once per frame at
    /// `rate`: the coefficient [`Seconds::one_pole_coeff`] up to one half, and over it the
    /// leave [`Seconds::one_pole_leave`], so the form kept is the one designed without
    /// cancelling.
    pub(crate) fn of(tau: Seconds, rate: SampleRate) -> Self {
        let c = tau.one_pole_coeff(rate);
        if c > 0.5 {
            Self::Leaves(F::from_design(tau.one_pole_leave(rate)))
        } else {
            Self::Closes(F::from_design(c))
        }
    }

    /// One step of `state` toward `target`, per-sample values in `F`; returns the new value.
    #[inline]
    pub(crate) fn step(self, state: &mut Smoothed<F>, target: F) -> F {
        match self {
            Self::Closes(c) => {
                let moved = c * (target - state.value) + state.dropped;
                let value = state.value + moved;
                state.dropped = moved - (value - state.value);
                state.value = value;
            }
            Self::Leaves(r) => *state = Smoothed::at(target + r * (state.value - target)),
        }
        state.value
    }
}

/// The poles of a follower on a quantity that grows toward a target and falls back: the
/// attack's while the quantity rises and the release's while it falls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Follower<F> {
    attack: Pole<F>,
    release: Pole<F>,
}

impl<F: Float> Follower<F> {
    /// Holds: a pole that closes none of the gap, so a state stays where it is. What a bank
    /// that is not yet aimed starts with; [`of`](Self::of) replaces it before any frame runs.
    pub(crate) const NEUTRAL: Self = Self {
        attack: Pole::HOLD,
        release: Pole::HOLD,
    };

    /// The follower of `ballistics` at `rate`.
    pub(crate) fn of(ballistics: Ballistics, rate: SampleRate) -> Self {
        Self {
            attack: Pole::of(ballistics.attack(), rate),
            release: Pole::of(ballistics.release(), rate),
        }
    }

    /// One step of `state` toward `target`, a per-sample value in `F`: at the attack's pole
    /// while the target is above the value and at the release's otherwise; returns the new
    /// value.
    #[inline]
    pub(crate) fn follow_raw(&self, state: &mut Smoothed<F>, target: F) -> F {
        let pole = if target > state.value {
            self.attack
        } else {
            self.release
        };
        pole.step(state, target)
    }
}

#[cfg(test)]
mod tests;
