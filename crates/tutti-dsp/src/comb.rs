//! [`CombFilter`]: a delay line summed with the signal, forward or back.
//!
//! A **feedforward** comb of `D` frames and gain `g` is `y[n] = x[n] + g x[n - D]`,
//! `H(z) = 1 + g z^-D`: notches at the odd multiples of `fs / 2D`, where the two terms
//! cancel to `1 - g`, and peaks of `1 + g` at the multiples of `fs / D`. A **feedback** comb
//! recirculates its own output: the loop `v[n] = x[n] + g y[n]` with the output `y[n] = v[n - D]`,
//! `H(z) = z^-D / (1 - g z^-D)`, peaks of magnitude `1 / (1 - g)` at the multiples of
//! `fs / D` and `1 / (1 + g)` between them. The feedback comb's output is the loop's
//! delayed read, so its first echo is at `D` and it has no direct term; its magnitude is the
//! textbook comb `1 / (1 - g z^-D)`'s.
//!
//! The feedback form has a damping: the loop reads the output through the one-pole
//! `s[n] = (1 - d) y[n] + d s[n - 1]`, so each trip loses highs, and `d = 0` is the plain
//! comb. Freeverb's combs are this filter at their own tunings.

use crate::BlockMismatch;
use crate::block::expect_widths;
use core::num::NonZeroUsize;
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Feedback, SampleRate, Samples, Seconds, Tail};

use crate::MaxDelay;
use crate::TAIL_FLOOR;
use crate::delay::{DelayRing, MAX_FRAMES, ring_seconds, trips};

tutti_units::ranged! {
    /// The delay of a comb: the time its ring holds.
    ///
    /// A range-checked value: finite, above zero and at most [`CAP`](Self::CAP).
    /// [`new`](Self::new) refuses anything else and never clamps: the delay becomes an
    /// allocation (a ring per channel), so a NaN, a negative time or one of 1e30 seconds must
    /// not reach it.
    #[derive(PartialOrd)]
    CombDelay(Seconds), CombDelayError, "a comb delay, above zero and at most 10 seconds",
    |raw| raw.get().is_finite() && raw.get() > 0.0 && raw.get() <= CombDelay::CAP.get()
}

impl CombDelay {
    /// The longest delay [`new`](Self::new) accepts: 10 s, a [`MaxDelay`]'s
    /// longest.
    pub const CAP: Seconds = MaxDelay::CAP;

    /// The frames of the ring at `rate`: `ceil(delay * rate)`, which is at least one for a
    /// delay above zero. Saturated where [`MaxDelay::frames`] saturates, so an absurd rate
    /// cannot make an allocation that aborts.
    #[must_use]
    pub fn frames(self, rate: SampleRate) -> NonZeroUsize {
        NonZeroUsize::new(self.0.to_samples_ceil(rate).get().min(MAX_FRAMES))
            .unwrap_or(NonZeroUsize::MIN)
    }
}

/// Which way a [`CombFilter`] sums its delay in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CombForm {
    /// `y[n] = x[n] + g x[n - D]`: a finite response, `D` frames long.
    Feedforward,
    /// The delay recirculates through the gain: an echo every `D` frames, falling by `g`.
    Feedback,
}

impl CombForm {
    /// The coefficient of a comb of this form that a driven `gain` is held to: a gain that is
    /// not finite is 0, a feedforward comb's is within `-1..=1` (`1 + g z^-D` is a valid
    /// filter of either sign: a negative gain moves the notches to where the peaks were) and a
    /// feedback comb's is within `0..=`[`Feedback::MAX_STABLE`] (the loop must decay; a
    /// negative feedback is stable too, but [`Feedback`] is a non-negative coefficient).
    ///
    /// The hold of a coefficient driven live (a cell or a control signal sets it with no
    /// control side to refuse a value): [`CombBank::retarget`] applies it.
    #[must_use]
    pub fn held(self, gain: Feedback) -> Feedback {
        let gain = gain.get();
        let gain = if gain.is_finite() { gain } else { 0.0 };
        match self {
            Self::Feedforward => Feedback::new(gain.clamp(-1.0, 1.0)),
            Self::Feedback => Feedback::new_clamped(gain),
        }
    }
}

/// One comb filter of `D` frames over the float `F`.
///
/// Built on the control side, where it allocates its ring; nothing after that does.
#[derive(Clone, Debug)]
pub struct CombFilter<F: Float> {
    form: CombForm,
    ring: DelayRing<F>,
    gain: F,
    /// The loop's one-pole: `damp` the pole and `pass` its complement `1 - damp`.
    damp: F,
    pass: F,
    store: F,
}

impl<F: Float> CombFilter<F> {
    /// A silent comb of `form`, its delay `delay` frames and `gain` as its
    /// coefficient (a feedback comb needs it below 1: [`Feedback`]'s own contract), with no
    /// damping.
    #[must_use]
    pub fn new(form: CombForm, delay: NonZeroUsize, gain: Feedback) -> Self {
        Self {
            form,
            ring: DelayRing::new(delay.get()),
            gain: F::from_unit(gain),
            damp: F::ZERO,
            pass: F::ONE,
            store: F::ZERO,
        }
    }

    /// The delay in frames.
    #[must_use]
    pub fn delay(&self) -> Samples {
        Samples::new(self.ring.len())
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.store = F::ZERO;
    }

    /// Takes the coefficient and, of a feedback comb, the loop's damping pole in `0..1`, both
    /// already in `F`. No allocation.
    pub(crate) fn retune(&mut self, gain: F, damping: F) {
        self.gain = gain;
        self.damp = damping;
        self.pass = F::ONE - damping;
    }

    /// The coefficient now.
    #[cfg(test)]
    pub(crate) fn gain(&self) -> F {
        self.gain
    }

    /// The damping pole now.
    #[cfg(test)]
    pub(crate) fn damping(&self) -> F {
        self.damp
    }

    /// Takes the coefficient alone, for a caller with no damping. No allocation.
    pub(crate) fn set_gain(&mut self, gain: F) {
        self.gain = gain;
    }

    /// One frame in, one out.
    #[inline]
    pub fn tick(&mut self, x: F) -> F {
        let old = self.ring.oldest();
        match self.form {
            CombForm::Feedforward => {
                self.ring.push(x);
                x + self.gain * old
            }
            CombForm::Feedback => {
                self.store = old * self.pass + self.store * self.damp;
                self.ring.push(x + self.store * self.gain);
                old
            }
        }
    }

    /// Runs `block` through the comb in place.
    pub fn process(&mut self, block: &mut [F]) {
        for x in block {
            *x = self.tick(*x);
        }
    }
}

/// The controls of a [`CombBank`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombControls {
    /// The coefficient `g`, driven live and held by the comb's form ([`CombForm::held`]): one
    /// that is not finite reads as 0, a feedforward comb's is inside `-1..=1` and a feedback
    /// comb's is inside [`Feedback::MAX_STABLE`] (the loop must decay).
    pub gain: Feedback,
}

impl CombControls {
    /// Half the coefficient's range: a comb that is heard without ringing for long.
    pub const DEFAULT: Self = Self {
        gain: Feedback::new(0.5),
    };
}

impl Default for CombControls {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A comb on every channel of a view: [`CombFilter`]s of one form and delay, sharing one
/// coefficient.
#[derive(Clone, Debug)]
pub struct CombBank<F: Float> {
    combs: Vec<CombFilter<F>>,
}

impl<F: Float> CombBank<F> {
    /// `width` silent combs (an empty layout has none) of `form` and `delay`, at `gain`. Allocates.
    #[must_use]
    pub fn new(form: CombForm, delay: NonZeroUsize, gain: Feedback, width: ChannelLayout) -> Self {
        Self {
            combs: vec![CombFilter::new(form, delay, gain); usize::from(width.count())],
        }
    }

    /// Takes `controls` on every channel, the coefficient held by the form
    /// ([`CombForm::held`]). No allocation.
    pub fn retarget(&mut self, controls: &CombControls) {
        let gain = F::from_unit(self.combs[0].form.held(controls.gain));
        for comb in &mut self.combs {
            comb.set_gain(gain);
        }
    }

    /// How long it rings after its input stops, to [`TAIL_FLOOR`], erring long, at `gain`: a
    /// feedforward comb's delay, and a feedback comb's echoes (`1 + trips` periods of the
    /// delay, as [`FreeverbBank::tail`](crate::FreeverbBank::tail) counts them).
    #[must_use]
    pub fn tail(&self, gain: Feedback, rate: SampleRate) -> Tail {
        let frames = self.delay().get();
        match self.combs[0].form {
            CombForm::Feedforward => Tail::Finite(Samples::new(frames)),
            CombForm::Feedback => {
                let trips = trips(gain.get().min(Feedback::MAX_STABLE.get()), TAIL_FLOOR);
                Tail::Finite(ring_seconds(frames, trips, rate).to_samples_ceil(rate))
            }
        }
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        for comb in &mut self.combs {
            comb.reset();
        }
    }

    /// The delay in frames.
    #[must_use]
    pub fn delay(&self) -> Samples {
        self.combs[0].delay()
    }

    /// Runs `input` into `output`, channel by channel. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.combs.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        for (c, comb) in self.combs.iter_mut().enumerate().take(width) {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            for (y, &x) in plane.samples_mut()[..frames].iter_mut().zip(x) {
                *y = comb.tick(x);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
