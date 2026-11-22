//! [`TransientBank`]: the transient shaper over buffers views.
//!
//! A transient shaper moves the level of a signal's attacks and of its sustain apart. It finds
//! the signal's level (the loudest channel at each frame), follows it with a **fast** and a
//! **slow** one-pole envelope (a one-pole follower with one time constant each), and multiplies
//! **every** channel by one gain read from how the fast envelope stands against the slow one:
//! the gain is linked, so the stereo image does not move.
//!
//! # The gain
//!
//! With `f` the fast envelope and `s` the slow one at a frame, the gain in dB is
//!
//! ```text
//!                 / attack  * (1 - s / f)   f > s   (the level is rising: an attack)
//!   gain_db  =   {  sustain * (1 - f / s)   f < s   (the level is falling: a decay)
//!                 \ 0                       f = s   (a steady level)
//! ```
//!
//! Each branch is in `[0, 1)` times its amount, and both are 0 where the envelopes meet. A
//! positive `attack` lifts the frames the fast envelope has run ahead of the slow one by the
//! fraction of the fast envelope that the slow one has not reached. The amount of a branch is
//! the most that branch adds: a decay that falls to nothing under a slow envelope still
//! holding level tends to `sustain`. An amount of 0 dB in both is the identity.
//!
//! A steady level is left alone to the slow follower's stall: the follower stops moving when
//! `c * (L - s)` is under half an ulp of `s`, which leaves `|L - s| / L` near `eps / (2 c)` for
//! the one-pole coefficient `c` and the float's epsilon `eps` (`f32`: about 2e-3 dB at 24 dB and
//! a 30 ms slow time, rate 48 kHz; `f64` is 2^29 times finer).
//!
//! After a step from silence to a constant level, `f_n = L (1 - af^n)` and `s_n = L (1 - as^n)`
//! with `a = exp(-1 / (tau * rate))`, so the gain at frame `n` is
//! `attack * (1 - (1 - as^n) / (1 - af^n))`.
//!
//! The amounts ramp over [`GAIN_RAMP`], as a compressor's makeup does; the
//! times are the bank's build. The shaper has no latency and no tail: it only scales.

use crate::BlockMismatch;
use crate::block::expect_same_width;
use tutti_buffers::{Float, InputOutput};
use tutti_units::{Db, SampleRate, Seconds};

use super::curves::db_gain;
use super::gain_pass::{CHUNK, GAIN_RAMP, floored, key_levels};
use crate::follower::{Follower, Smoothed};
use crate::{Lerp, Ramp};

/// The controls of a [`TransientBank`]: the dB the two branches of the gain reach.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransientControls {
    /// The most the gain adds while the fast envelope is above the slow one (the attack of a
    /// note). Read within [`TransientControls::MAX_AMOUNT`] either way; a non-finite amount is 0.
    pub attack: Db,
    /// The most the gain adds while the fast envelope is below the slow one (the decay and
    /// the room). Read as `attack` is.
    pub sustain: Db,
}

impl TransientControls {
    /// The most either amount reaches, either way: 24 dB.
    pub const MAX_AMOUNT: Db = Db::new(24.0);

    /// No change: both amounts 0 dB.
    pub const DEFAULT: Self = Self {
        attack: Db::UNITY,
        sustain: Db::UNITY,
    };

    /// These controls within range, a non-finite amount as the default's.
    fn designed(self) -> Amounts {
        let amount = |db: Db| {
            db.finite_or(Db::UNITY).clamp(
                -TransientControls::MAX_AMOUNT,
                TransientControls::MAX_AMOUNT,
            )
        };
        Amounts {
            attack: amount(self.attack),
            sustain: amount(self.sustain),
        }
    }
}

impl Default for TransientControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The two amounts in dB, as a pair a ramp blends.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Amounts {
    attack: Db,
    sustain: Db,
}

impl Lerp for Amounts {
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        Self {
            attack: Db::new(self.attack.get().lerp(&to.attack.get(), k, n)),
            sustain: Db::new(self.sustain.get().lerp(&to.sustain.get(), k, n)),
        }
    }
}

/// The gain in dB at the envelopes `fast` and `slow` (both at least 0 and finite) for the
/// amounts `attack` and `sustain`, per-sample values in `F`: the closed form of the
/// [module](self).
#[inline]
fn gain_db<F: Float>(fast: F, slow: F, attack: F, sustain: F) -> F {
    if fast > slow {
        attack * (F::ONE - slow / fast)
    } else if slow > fast {
        sustain * (F::ONE - fast / slow)
    } else {
        F::ZERO
    }
}

/// A transient shaper of any width: one gain from the loudest channel's fast and slow
/// envelopes, applied to every channel.
///
/// Built on the control side for one rate; the state and the views are `F`, the graph's float.
/// Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct TransientBank<F: Float + Lerp> {
    fast: Follower<F>,
    slow: Follower<F>,
    /// The fast and the slow envelope, both at 0 until a level is heard: per-sample levels in
    /// `F`.
    envelopes: (Smoothed<F>, Smoothed<F>),
    /// The amounts in dB, ramping over [`GAIN_RAMP`].
    amounts: Ramp<Amounts>,
    steps: u32,
}

impl<F: Float + Lerp> TransientBank<F> {
    /// A shaper at `rate` whose fast envelope has the time constant `fast` and whose slow one
    /// has `slow`, at no change. The times are floored at 0 (instant); a NaN one reads as 0.
    #[must_use]
    pub fn new(rate: SampleRate, fast: Seconds, slow: Seconds) -> Self {
        let follower = |t: Seconds| Follower::of(floored(t, t), rate);
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            fast: follower(fast),
            slow: follower(slow),
            envelopes: (Smoothed::ZERO, Smoothed::ZERO),
            amounts: Ramp::new(TransientControls::DEFAULT.designed()),
            steps,
        }
    }

    /// Back to silence: both envelopes at 0 and the amounts at rest. No allocation.
    pub fn reset(&mut self) {
        self.envelopes = (Smoothed::ZERO, Smoothed::ZERO);
        self.amounts.jump_to_target();
    }

    /// Lands the amounts on their target now.
    pub fn jump_to_target(&mut self) {
        self.amounts.jump_to_target();
    }

    /// Aims at `controls`: the amounts ramp over [`GAIN_RAMP`]. No allocation.
    pub fn retarget(&mut self, controls: &TransientControls) {
        let to = controls.designed();
        if to != self.amounts.target() {
            self.amounts.retarget_in(to, self.steps);
        }
    }

    /// Runs `input` into `output`, scaled by one gain from the loudest channel of `input`.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`. A NaN sample reads as quiet and an
    /// infinite one as the largest finite level. Frame `i` reads the amounts at the ramp's
    /// step `taken + 1 + i`, then the block's steps are taken. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the output has other channels than the input.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = expect_same_width(block)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let blended = self.amounts.blended_frames(frames);
        let taken = self.amounts.steps_taken();
        let mut gains = [F::ZERO; CHUNK];
        let mut start = 0;
        while start < frames {
            let n = (frames - start).min(CHUNK);
            let end = start + n;
            let gains = &mut gains[..n];
            key_levels(&input, start..end, gains);
            for (k, g) in gains.iter_mut().enumerate() {
                let i = start + k;
                let a = if i < blended {
                    self.amounts
                        .at(taken.saturating_add(1).saturating_add(i as u32))
                } else {
                    self.amounts.target()
                };
                let fast = self.fast.follow_raw(&mut self.envelopes.0, *g);
                let slow = self.slow.follow_raw(&mut self.envelopes.1, *g);
                *g = db_gain(gain_db(
                    fast,
                    slow,
                    F::from_unit(a.attack),
                    F::from_unit(a.sustain),
                ));
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, &x), &g) in ys.iter_mut().zip(xs).zip(gains.iter()) {
                    *y = x * g;
                }
            }
            start = end;
        }
        self.amounts
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
