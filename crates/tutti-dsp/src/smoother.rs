//! The one-pole step `y += c * (x - y)`, the recurrence every de-zippering and
//! envelope-following filter in the engine runs.

use tutti_buffers::Float;

/// One step from `current` toward `target`, closing the fraction `coeff` of the gap
/// (`1` is instant, `0` holds): the recurrence itself, for a caller that keeps the state.
///
/// The coefficient comes in already derived, from the time constant and the rate the state
/// is stepped at (`Seconds::one_pole_coeff` in `tutti-units`). A follower with separate
/// attack and release coefficients picks one per step. A time constant long enough that `coeff`
/// is under half an ulp of the state needs the compensated one-pole in tutti-dsp's dynamics
/// followers, which keeps what a step drops.
///
/// At `coeff = 1` the result is `target` only to within half an ulp of the larger of the two
/// (exactly when they are within 2x of each other). The retention form
/// `(1 - c) * current + c * target` would be exact there, but its `1 - c` rounds to 1 for a
/// long time constant and the value then stops approaching a lower target.
#[inline]
pub fn smooth_toward<F: Float>(current: F, target: F, coeff: F) -> F {
    current + coeff * (target - current)
}

#[cfg(test)]
mod tests;
