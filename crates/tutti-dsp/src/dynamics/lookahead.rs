//! The lookahead limiter's delay ring with its sliding gain minimum, and the linked-peak
//! fold. The [`LimiterBank`](super::LimiterBank) holds them in its state.

use tutti_buffers::{Float, Sample};

use super::window::MonotonicMinDeque;
use tutti_types::Samples;

use crate::DelayLine;

/// Lookahead ring buffers + sliding-window-minimum tracker for the limiter.
#[derive(Clone, Debug)]
pub(crate) struct LookaheadRing {
    /// One delay line per audio channel, read at the lookahead by the whole-frame reader;
    /// `lines.len()` == width. Built at construction, never resized in the RT path.
    lines: Vec<DelayLine>,
    lookahead: usize,
    min_deque: MonotonicMinDeque,
    sample_counter: u64,
}

impl LookaheadRing {
    /// A ring of `channels` channels delaying by `lookahead_samples` frames.
    #[must_use]
    pub(crate) fn new(channels: usize, lookahead_samples: usize) -> Self {
        let n = lookahead_samples.max(1);
        Self {
            lines: (0..channels.max(1))
                .map(|_| DelayLine::new(Samples(n)))
                .collect(),
            lookahead: n,
            // n + 1 frames: the one delayed out now and the n after it.
            min_deque: MonotonicMinDeque::new(n + 1),
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
    pub(crate) fn push_gain(&mut self, gain: f32) -> f32 {
        self.min_deque.push(self.sample_counter, gain);
        self.sample_counter = self.sample_counter.wrapping_add(1);
        self.min_deque.min().unwrap_or(gain)
    }

    /// Delays `input` by the lookahead through channel `c`'s line and writes it to `out`
    /// scaled by the matching frame of `gains`: each frame is pushed, then the sample
    /// pushed `lookahead` frames before it is read back. The channels' lines are
    /// independent, so a block runs one channel at a time.
    #[inline]
    pub(crate) fn delay_scaled<S: Float>(
        &mut self,
        c: usize,
        input: &[S],
        gains: &[f32],
        out: &mut [S],
    ) {
        let line = &mut self.lines[c];
        for ((o, x), &g) in out.iter_mut().zip(input).zip(gains) {
            line.push_sample(x.convert_to::<f32>());
            let d = line.read_whole(self.lookahead);
            // A NaN in both operands: x86 returns the first one's and which
            // is first is up to the compiler, so pick the delayed sample's
            // (as the per-frame product did) to keep the sign bit. An infinite sample
            // is held to the largest finite one, as the detector read it, so its
            // product with the gain (a divide of the ceiling by that level, or 0) is
            // finite and not `inf * 0`.
            *o = (if d.is_nan() {
                d + d
            } else {
                d.clamp(-f32::MAX, f32::MAX) * g
            })
            .convert_to::<S>();
        }
    }
}

/// The linked-peak fold: a NaN sample never reaches the peak (it reads as quiet) and an
/// infinity reads as the largest finite level, so the peak's dB is finite.
#[inline]
pub(crate) fn fold_peak(peak: f32, x: f32) -> f32 {
    peak.max(super::shared::level(x))
}

#[cfg(test)]
mod tests {
    use super::fold_peak;

    /// The peak fold ignores a NaN (`max` returns the other operand), so a NaN sample never
    /// reaches the detector, and takes the magnitude of a negative one.
    #[test]
    fn a_nan_fed_to_the_peak_fold_is_ignored() {
        assert_eq!(fold_peak(0.5, f32::NAN), 0.5);
        assert_eq!(fold_peak(0.0, f32::NAN), 0.0);
        assert_eq!(fold_peak(0.5, -0.75), 0.75);
    }
}
