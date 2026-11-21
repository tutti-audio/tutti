//! [`EarlyReflections`]: the realization of the reverb model's [`Taps`] stage.
//!
//! The stage is a sum of delayed, scaled copies of one signal, each tap `g z^-d` on an output
//! channel: `y_c[n] = sum over the taps t of channel c of g_t x[n - d_t]`, with the one signal
//! `x` the mean of the input's channels (the model's rule for a stage that takes one signal).
//! There is no feedback and no direct term unless a tap sits at delay zero, so the response is
//! finite, `d_max` frames long. A whole-frame delay reads one frame of the history ring; a
//! fractional one is the linear blend of the two frames around it, so an impulse puts
//! `g (1 - f)` at `floor(d)` and `g f` one frame later, `f` the fraction of the delay.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{ReadPosition, SampleSpan, Samples};

use crate::delay::DelayRing;
use crate::{MAX_CHANNELS, Taps, lerp};

/// The frames the input is averaged in at a time.
const CHUNK: usize = 64;

/// The longest tap a stage holds, in frames: ten seconds at 192 kHz, the ring a tap that late
/// needs being 7.7 MB of `f32`.
pub const MAX_TAP_DELAY: Samples = Samples::new(1_920_000);

/// Why a [`Taps`] cannot be realized.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum EarlyReflectionsError {
    /// A tap's gain is NaN or infinite.
    #[error("tap {index} has a gain that is not finite")]
    Gain {
        /// The tap's place in the stage.
        index: usize,
    },
    /// A tap feeds an output channel past the last one a stage has.
    #[error("tap {index} feeds channel {channel}, past the {MAX_CHANNELS} channels a stage has")]
    Channel {
        /// The tap's place in the stage.
        index: usize,
        /// The channel it names.
        channel: u8,
    },
    /// A tap is later than the longest delay a line holds.
    #[error("tap {index} is {delay} frames late, past the longest delay ({longest})")]
    Delay {
        /// The tap's place in the stage.
        index: usize,
        /// The tap's delay.
        delay: SampleSpan,
        /// The longest delay, [`MAX_TAP_DELAY`].
        longest: Samples,
    },
}

/// One tap, lowered to the float and to a read of the ring.
#[derive(Clone, Copy, Debug)]
struct Reflection<F> {
    /// The whole frames of the delay.
    whole: usize,
    /// The fraction of the delay, in `[0, 1)`.
    frac: F,
    gain: F,
    channel: usize,
}

/// The [`Taps`] stage of a reverb model, running over the float `F`.
///
/// Built on the control side, where it allocates its history ring and tap list; nothing it
/// does after that allocates.
#[derive(Clone, Debug)]
pub struct EarlyReflections<F: Float> {
    ring: DelayRing<F>,
    taps: Vec<Reflection<F>>,
    channels: usize,
    tail: Samples,
}

impl<F: Float> TryFrom<&Taps> for EarlyReflections<F> {
    type Error = EarlyReflectionsError;

    /// The stage `taps` describes. Allocates.
    ///
    /// The output has one channel more than the highest a tap names (one for a stage with no
    /// taps, which is silent).
    ///
    /// # Errors
    ///
    /// [`EarlyReflectionsError`] for a tap whose gain is not finite, whose channel is past
    /// [`MAX_CHANNELS`] or whose delay is past [`MAX_TAP_DELAY`].
    fn try_from(taps: &Taps) -> Result<Self, Self::Error> {
        let longest = MAX_TAP_DELAY;
        let mut list = Vec::with_capacity(taps.taps.len());
        let (mut channels, mut last) = (1, SampleSpan::ZERO);
        for (index, tap) in taps.taps.iter().enumerate() {
            if !tap.gain.get().is_finite() {
                return Err(EarlyReflectionsError::Gain { index });
            }
            let channel = usize::from(tap.channel);
            if channel >= MAX_CHANNELS {
                return Err(EarlyReflectionsError::Channel {
                    index,
                    channel: tap.channel,
                });
            }
            if tap.delay.to_samples_ceil() > longest {
                return Err(EarlyReflectionsError::Delay {
                    index,
                    delay: tap.delay,
                    longest,
                });
            }
            let (whole, frac) = F::split_position(ReadPosition::from_span(tap.delay));
            list.push(Reflection {
                whole: whole.get(),
                frac,
                gain: F::from_unit(tap.gain),
                channel,
            });
            channels = channels.max(channel + 1);
            last = last.max(tap.delay);
        }
        let tail = last.to_samples_ceil();
        Ok(Self {
            // The newest frame is one slot back, and a fractional read reaches one frame past
            // the whole part of the longest delay.
            ring: DelayRing::new(tail.get() + 2),
            taps: list,
            channels,
            tail,
        })
    }
}

impl<F: Float> EarlyReflections<F> {
    /// The channels it outputs: one more than the highest a tap feeds.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// How long it rings after its input stops: the last tap's delay, in whole frames
    /// rounded up.
    #[must_use]
    pub fn tail(&self) -> Samples {
        self.tail
    }

    /// Back to silence. No allocation.
    pub fn reset(&mut self) {
        self.ring.clear();
    }

    /// The frame `t` reads: `x[n - d]` of the frame just pushed.
    #[inline]
    fn read(&self, t: &Reflection<F>) -> F {
        let a = self.ring.back(t.whole + 1);
        if t.frac == F::ZERO {
            a
        } else {
            lerp(a, self.ring.back(t.whole + 2), t.frac)
        }
    }

    /// Runs `input` into `output`: the mean of the input's channels through the taps, the
    /// fewer of the two views' frames, each output channel from the taps that feed it and
    /// silence for a channel no tap feeds. No allocation.
    pub fn process(&mut self, input: Planes<'_, F>, mut output: PlanesMut<'_, '_, F>) {
        let frames = input.frames().get().min(output.frames().get());
        let scale = F::from_design(1.0 / input.channels().max(1) as f64);
        let mut planes = output.iter_mut();
        let mut ys: [Option<&mut [F]>; MAX_CHANNELS] =
            core::array::from_fn(|_| planes.next().map(|p| &mut p.into_samples_mut()[..frames]));
        for start in (0..frames).step_by(CHUNK) {
            let end = (start + CHUNK).min(frames);
            let mut mean = [F::ZERO; CHUNK];
            for plane in input.iter() {
                for (m, &x) in mean.iter_mut().zip(&plane.samples()[start..end]) {
                    *m += x;
                }
            }
            for (i, &x) in mean[..end - start].iter().enumerate() {
                self.ring.push(x * scale);
                let mut sum = [F::ZERO; MAX_CHANNELS];
                for t in &self.taps {
                    sum[t.channel] += t.gain * self.read(t);
                }
                for (y, &s) in ys.iter_mut().zip(&sum) {
                    if let Some(y) = y {
                        y[start + i] = s;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
