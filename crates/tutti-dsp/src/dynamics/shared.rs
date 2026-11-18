//! What the compressor, gate and limiter banks share: the chunk a pass over a block takes,
//! the ramp length of a gain that moves, and the key's level.

use std::ops::Range;

use tutti_buffers::{Float, Planes};
use tutti_units::{Ballistics, SampleRate, Seconds, Unit};

use crate::smoother::Smoother;

/// The frames a pass over a block takes at a time: the levels of a stretch become its gains
/// in a stack array.
pub(super) const CHUNK: usize = 64;

/// How long a makeup (or a gate's range) that moves takes to arrive: 5 ms, counted on the
/// stream, so the de-zipper does not depend on the block.
pub const GAIN_GLIDE: Seconds = Seconds(0.005);

/// `x`'s magnitude as a level: a NaN reads as quiet (0) and an infinity as the largest
/// finite level, so a detector never sees a value its dB conversion turns into an infinity.
#[inline]
pub(super) fn level(x: f32) -> f32 {
    let a = x.abs();
    if a.is_nan() { 0.0 } else { a.min(f32::MAX) }
}

/// The key's level at each frame of `frames`: the loudest channel of `key`, into `out`
/// (as long as `frames`).
#[inline]
pub(crate) fn key_levels<S: Float>(key: &Planes<'_, S>, frames: Range<usize>, out: &mut [f32]) {
    out.fill(0.0);
    for plane in key.iter() {
        for (m, x) in out.iter_mut().zip(&plane.samples()[frames.clone()]) {
            *m = m.max(level(x.convert_to::<f32>()));
        }
    }
}

/// The widest knee, in dB, a detector's curve runs: the knee term `(1 - 1/ratio) x^2 / (2 knee)`
/// with `x` inside the knee is at most `knee / 2`, and `x^2` stays finite.
pub(super) const MAX_KNEE_DB: f32 = 400.0;

/// The attack and release a control pair names, as a valid [`Ballistics`]: a time under 0 or
/// NaN reads as 0 (instant) and an infinite one holds (coefficient 0), as
/// [`Seconds::one_pole_coeff`] says. `Ballistics::new` refuses those, so a hostile control is
/// brought into range here.
pub(crate) fn floored(attack: Seconds, release: Seconds) -> Ballistics {
    // `max` reads NaN and a negative time as 0. `clamp` caps +inf at `f64::MAX` seconds, whose
    // coefficient is 0 at every rate: `t * rate` overflows to infinity above 1 Hz, and below
    // it `exp(-1 / (t * rate))` of a subnormal exponent is exactly 1.
    let time = |t: Seconds| Seconds(t.get().max(0.0).clamp(0.0, f64::MAX));
    Ballistics::new(time(attack), time(release))
        .expect("a time floored at 0 and capped at f64::MAX is finite and not negative")
}

/// The one-pole coefficients of a follower on a quantity that grows toward a target and falls
/// back: the attack's while the quantity rises and the release's while it falls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Follower {
    pub(super) attack: f32,
    pub(super) release: f32,
}

impl Follower {
    /// Holds: a coefficient of 0 closes none of the gap, so a state stays where it is. What a
    /// bank that is not yet aimed starts with; [`of`](Self::of) replaces it before any frame
    /// runs.
    pub(super) const NEUTRAL: Self = Self {
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

#[cfg(test)]
mod tests;
