//! What the compressor, gate and limiter banks share: the chunk a pass over a block takes,
//! the ramp length of a gain that moves, and the key's level.

use std::ops::Range;

use tutti_buffers::{Float, Planes};
use tutti_types::{SampleRate, Seconds};

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
pub(super) fn key_levels<S: Float>(key: &Planes<'_, S>, frames: Range<usize>, out: &mut [f32]) {
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

/// The attack and release of a follower on a quantity that grows toward a target and falls
/// back: one-pole coefficients at the rate, the attack's while the quantity rises and the
/// release's while it falls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Ballistics {
    attack: f32,
    release: f32,
}

impl Ballistics {
    /// Instant: each step lands on the target. The start of a bank that is not yet aimed.
    pub(super) const NEUTRAL: Self = Self {
        attack: 0.0,
        release: 0.0,
    };

    /// The follower of time constants `attack` and `release` at `rate`; a time under 0 reads
    /// as 0 (instant) and an infinite one holds, as [`Seconds::one_pole_coeff`] says.
    pub(super) fn new(attack: Seconds, release: Seconds, rate: SampleRate) -> Self {
        Self {
            attack: attack.one_pole_coeff(rate),
            release: release.one_pole_coeff(rate),
        }
    }

    /// One step of `state` toward `target`, at the attack's coefficient while the target is
    /// above `state` and at the release's otherwise.
    #[inline]
    pub(super) fn follow(&self, state: f32, target: f32) -> f32 {
        let coeff = if target > state {
            self.attack
        } else {
            self.release
        };
        Smoother::toward(state, target, coeff)
    }
}
