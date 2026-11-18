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
//! A steady level is left alone to the slow follower's `f32` stall: the follower stops moving
//! when `c * (L - s)` is under half an ulp of `s`, which leaves `|L - s| / L` near `6e-8 / c`
//! for the one-pole coefficient `c` (about 2e-3 dB at 24 dB and a 30 ms slow time, rate 48 kHz).
//!
//! After a step from silence to a constant level, `f_n = L (1 - af^n)` and `s_n = L (1 - as^n)`
//! with `a = exp(-1 / (tau * rate))`, so the gain at frame `n` is
//! `attack * (1 - (1 - as^n) / (1 - af^n))`.
//!
//! The amounts glide over [`GAIN_GLIDE`], as a compressor's makeup does; the
//! times are the bank's build. The shaper has no latency and no tail: it only scales.

use tutti_buffers::{Float, Planes, PlanesMut, Sample};
use tutti_units::{Amplitude, Db, SampleRate, Seconds};

use super::shared::{CHUNK, GAIN_GLIDE, floored, key_levels};
use crate::finite::finite_or;
use crate::follower::Follower;
use crate::{Glide, Lerp};

/// The controls of a [`TransientBank`]: the dB the two branches of the gain reach.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransientControls {
    /// The most the gain adds while the fast envelope is above the slow one (the attack of a
    /// note). Read within [`TransientBank::MAX_AMOUNT`] either way; a non-finite amount is 0.
    pub attack: Db,
    /// The most the gain adds while the fast envelope is below the slow one (the decay and
    /// the room). Read as `attack` is.
    pub sustain: Db,
}

impl TransientControls {
    /// No change: both amounts 0 dB.
    pub const DEFAULT: Self = Self {
        attack: Db::UNITY,
        sustain: Db::UNITY,
    };

    /// These controls within range, a non-finite amount as the default's.
    fn designed(self) -> Amounts {
        let amount = |db: Db| {
            Db(finite_or(db.get(), Db::UNITY.get()).clamp(
                -TransientBank::MAX_AMOUNT.get(),
                TransientBank::MAX_AMOUNT.get(),
            ))
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

/// The two amounts in dB, as a pair a glide blends.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Amounts {
    attack: Db,
    sustain: Db,
}

impl Lerp for Amounts {
    #[inline]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self {
        let t = k as f32 / n as f32;
        Self {
            attack: Db(crate::lerp(self.attack.get(), to.attack.get(), t)),
            sustain: Db(crate::lerp(self.sustain.get(), to.sustain.get(), t)),
        }
    }
}

/// The gain in dB at the envelopes `fast` and `slow` (both at least 0 and finite) for the
/// amounts `attack` and `sustain`: the closed form of the [module](self).
#[inline]
fn gain_db(fast: Amplitude, slow: Amplitude, attack: Db, sustain: Db) -> Db {
    if fast > slow {
        Db(attack.get() * (1.0 - slow.get() / fast.get()))
    } else if slow > fast {
        Db(sustain.get() * (1.0 - fast.get() / slow.get()))
    } else {
        Db::UNITY
    }
}

/// A transient shaper of any width: one gain from the loudest channel's fast and slow
/// envelopes, applied to every channel.
///
/// Built on the control side for one rate; the state is `f32` and a view of another sample
/// format is converted at the edges. Nothing it holds grows.
#[derive(Clone, Debug)]
pub struct TransientBank {
    fast: Follower,
    slow: Follower,
    /// The fast and the slow envelope, both at 0 until a level is heard.
    envelopes: (Amplitude, Amplitude),
    /// The amounts in dB, gliding over [`GAIN_GLIDE`].
    amounts: Glide<Amounts>,
    steps: u32,
}

impl TransientBank {
    /// The most either amount reaches, either way: 24 dB.
    pub const MAX_AMOUNT: Db = Db(24.0);

    /// A shaper at `rate` whose fast envelope has the time constant `fast` and whose slow one
    /// has `slow`, at no change. The times are floored at 0 (instant); a NaN one reads as 0.
    #[must_use]
    pub fn new(rate: SampleRate, fast: Seconds, slow: Seconds) -> Self {
        let follower = |t: Seconds| Follower::of(floored(t, t), rate);
        let steps = u32::try_from(GAIN_GLIDE.to_samples_ceil(rate).get()).unwrap_or(u32::MAX);
        Self {
            fast: follower(fast),
            slow: follower(slow),
            envelopes: (Amplitude::SILENT, Amplitude::SILENT),
            amounts: Glide::landed(TransientControls::DEFAULT.designed()),
            steps,
        }
    }

    /// Back to silence: both envelopes at 0 and the amounts landed. No allocation.
    pub fn reset(&mut self) {
        self.envelopes = (Amplitude::SILENT, Amplitude::SILENT);
        self.amounts.settle();
    }

    /// Lands the amounts on their target now.
    pub fn settle(&mut self) {
        self.amounts.settle();
    }

    /// Aims at `controls`: the amounts glide over [`GAIN_GLIDE`]. No allocation.
    pub fn aim(&mut self, controls: &TransientControls) {
        let to = controls.designed();
        if to != self.amounts.target() {
            self.amounts.aim_in(to, self.steps);
        }
    }

    /// Runs `input` into `output`, scaled by one gain from the loudest channel of `input`.
    ///
    /// Channel `c` of `input` feeds channel `c` of `output`; a channel past the narrower of the
    /// two views is not touched, and the frames rendered are the fewer of the two views'. A NaN
    /// sample reads as quiet and an infinite one as the largest finite level. Frame `i` reads
    /// the amounts at the glide's step `taken + 1 + i`, then the block's steps are taken. No
    /// allocation.
    pub fn process<S: Float>(&mut self, input: Planes<'_, S>, mut output: PlanesMut<'_, '_, S>) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels().min(output.channels());
        let blended = self.amounts.blended(frames);
        let taken = self.amounts.taken();
        let mut gains = [0.0f32; CHUNK];
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
                let level = Amplitude::new(*g);
                let fast = self.fast.follow(self.envelopes.0, level);
                let slow = self.slow.follow(self.envelopes.1, level);
                self.envelopes = (fast, slow);
                *g = gain_db(fast, slow, a.attack, a.sustain)
                    .to_amplitude()
                    .get();
            }
            for c in 0..width {
                let xs = &input.plane(c).samples()[start..end];
                let mut plane = output.plane_mut(c);
                let ys = &mut plane.samples_mut()[start..end];
                for ((y, x), &g) in ys.iter_mut().zip(xs).zip(gains.iter()) {
                    *y = (x.convert_to::<f32>() * g).convert_to::<S>();
                }
            }
            start = end;
        }
        self.amounts
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
    }
}

#[cfg(test)]
mod tests;
