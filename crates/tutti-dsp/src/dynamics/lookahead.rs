//! The lookahead limiter's delay ring with its sliding gain minimum, and the linked-peak
//! fold. The [`LimiterBank`](super::LimiterBank) holds them in its state.

use tutti_buffers::Float;

use super::gain_pass::{held, level};
use super::sliding_min::MonotonicMinDeque;
use tutti_units::Samples;

use crate::DelayLine;

/// Lookahead ring buffers + sliding-window-minimum tracker for the limiter. The gains it
/// pushes and the minimum it holds are per-sample levels in `F`.
#[derive(Clone, Debug)]
pub(crate) struct LookaheadRing<F: Float> {
    /// One delay line per audio channel, read at the lookahead by the whole-frame reader;
    /// `lines.len()` == width. Built at construction, never resized in the RT path.
    lines: Vec<DelayLine<F>>,
    lookahead: usize,
    min_deque: MonotonicMinDeque<F>,
    sample_counter: u64,
}

impl<F: Float> LookaheadRing<F> {
    /// A ring of `channels` channels delaying by `lookahead_samples` frames, whose gain
    /// window holds the newest `window` frames of gain: at least one and at most
    /// `lookahead_samples + 1` (the delayed frame's own and the lookahead after it).
    #[must_use]
    pub(crate) fn new(channels: usize, lookahead_samples: usize, window: usize) -> Self {
        let n = lookahead_samples.max(1);
        debug_assert!(
            (1..=n + 1).contains(&window),
            "a window of {window} over {n}"
        );
        Self {
            lines: (0..channels)
                .map(|_| DelayLine::new(Samples::new(n)))
                .collect(),
            lookahead: n,
            min_deque: MonotonicMinDeque::new(window),
            sample_counter: 0,
        }
    }

    /// The channels the ring delays.
    pub(crate) fn width(&self) -> usize {
        self.lines.len()
    }

    /// Empties the rings and the minimum tracker.
    pub(crate) fn clear(&mut self) {
        for line in &mut self.lines {
            line.reset();
        }
        self.min_deque.clear();
        self.sample_counter = 0;
    }

    /// Feeds `gain` to the sliding minimum and returns the window-min gain
    /// (the linked gain reduction all channels share). The window is the
    /// delayed sample's own frame and the `lookahead_samples` after it, so the
    /// gain that scales a sample is never later than its own target's: the
    /// push drops the gain leaving the window before adding this one.
    #[inline]
    pub(crate) fn push_gain(&mut self, gain: F) -> F {
        self.min_deque.push(self.sample_counter, gain);
        self.sample_counter = self.sample_counter.wrapping_add(1);
        self.min_deque.min().unwrap_or(gain)
    }

    /// Delays `input` by the lookahead through channel `c`'s line and writes it to `out`
    /// scaled by the matching frame of `gains`: each frame is pushed, then the sample
    /// pushed `lookahead` frames before it is read back. The channels' lines are
    /// independent, so a block runs one channel at a time.
    #[inline]
    pub(crate) fn delay_scaled(&mut self, c: usize, input: &[F], gains: &[F], out: &mut [F]) {
        let line = &mut self.lines[c];
        for ((o, &x), &g) in out.iter_mut().zip(input).zip(gains) {
            line.push_sample(x);
            let d = line.read_whole(self.lookahead);
            // A NaN in both operands: x86 returns the first one's and which
            // is first is up to the compiler, so pick the delayed sample's
            // (as the per-frame product did) to keep the sign bit. An infinite sample
            // is held to the level the detector read it at, so its product with the
            // gain (a divide of the ceiling by that level, or 0) is finite and not
            // `inf * 0`.
            *o = if d.is_nan() {
                d + d
            } else {
                d.clamp(-held::<F>(), held::<F>()) * g
            };
        }
    }
}

/// The linked-peak fold: a NaN sample never reaches the peak (it reads as quiet) and an
/// infinity reads as the largest finite level, so the peak's dB is finite.
#[inline]
pub(crate) fn fold_peak<F: Float>(peak: F, x: F) -> F {
    peak.max(level(x))
}

#[cfg(test)]
mod tests;
