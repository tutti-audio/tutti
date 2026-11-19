//! What the compressor, gate and limiter banks share: the chunk a pass over a block takes,
//! the ramp length of a gain that moves, and the key's level.

use std::ops::Range;

use tutti_buffers::{Float, Planes};
use tutti_units::{Ballistics, Db, Seconds};

/// The frames a pass over a block takes at a time: the levels of a stretch become its gains
/// in a stack array.
pub(super) const CHUNK: usize = 64;

/// How long a makeup (or a gate's range) that moves takes to arrive: 5 ms, counted on the
/// stream, so the de-zipper does not depend on the block.
pub const GAIN_RAMP: Seconds = Seconds::new(0.005);

/// The level an infinite sample is held to: `f32::MAX` (770 dB) at both widths, so the level's
/// dB, and the gain over it, stay finite at the narrowest float.
#[inline]
pub(super) fn held<F: Float>() -> F {
    F::lit(3.402_823_466_385_288_6e38)
}

/// `x`'s magnitude as a level: a NaN reads as quiet (0) and an infinity as [`held`], so a
/// detector never sees a value its dB conversion turns into an infinity.
#[inline]
pub(super) fn level<F: Float>(x: F) -> F {
    let a = x.abs();
    if a.is_nan() { F::ZERO } else { a.min(held()) }
}

/// The key's level at each frame of `frames`: the loudest channel of `key`, into `out`
/// (as long as `frames`): per-sample levels in `F`.
#[inline]
pub(crate) fn key_levels<F: Float>(key: &Planes<'_, F>, frames: Range<usize>, out: &mut [F]) {
    out.fill(F::ZERO);
    for plane in key.iter() {
        for (m, &x) in out.iter_mut().zip(&plane.samples()[frames.clone()]) {
            *m = m.max(level(x));
        }
    }
}

/// The widest knee, in dB, a detector's curve runs: the knee term `(1 - 1/ratio) x^2 / (2 knee)`
/// with `x` inside the knee is at most `knee / 2`, and `x^2` stays finite.
pub(super) const MAX_KNEE: Db = Db::new(400.0);

/// The attack and release a control pair names, as a valid [`Ballistics`]: a time under 0 or
/// NaN reads as 0 (instant) and an infinite one holds (coefficient 0), as
/// [`Seconds::one_pole_coeff`] says. `Ballistics::new` refuses those, so a hostile control is
/// brought into range here.
pub(crate) fn floored(attack: Seconds, release: Seconds) -> Ballistics {
    // `max` reads NaN and a negative time as 0. `clamp` caps +inf at `f64::MAX` seconds, whose
    // coefficient is 0 at every rate: `t * rate` overflows to infinity above 1 Hz, and below
    // it `exp(-1 / (t * rate))` of a subnormal exponent is exactly 1.
    let time = |t: Seconds| Seconds::new(t.get().max(0.0).clamp(0.0, f64::MAX));
    Ballistics::new(time(attack), time(release))
        .expect("a time floored at 0 and capped at f64::MAX is finite and not negative")
}

#[cfg(test)]
mod tests;
