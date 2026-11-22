//! [`TransientDetector`]: the positions of attacks, to the sample.
//!
//! The detector reads the level the transient shaper reads (the magnitude of the signal,
//! through the shaper's follower) at a **fast** and a **slow** time constant, and calls a
//! frame an attack when both hold:
//!
//! - the fast envelope runs ahead of the slow one by more than the threshold,
//!   `fast - slow > threshold` (the envelope difference of the shaper's gain, with an absolute
//!   threshold in place of its gain curve); and
//! - the fast envelope has risen by more than the threshold within the last
//!   `LOOKBACK_TIME_CONSTANTS` fast time constants, `fast - min(recent fast) > threshold`.
//!
//! After a step from silence to level `L`, `fast - slow` peaks near `L (c_fast - c_slow)` for
//! the one-pole coefficients `c`, so a level `L` is found when `L` times the fast envelope's
//! per-sample step exceeds the threshold. The second condition keeps the ripple of a periodic
//! signal, and the slow envelope's catching up after an attack, from reading as a new attack:
//! a rectified sine of amplitude `A` and frequency `f` ripples the fast envelope (time constant
//! `tau`) by at most `0.84 A / sqrt(1 + (4 pi f tau)^2)` peak to peak, which must stay under the
//! threshold.
//!
//! # Position
//!
//! The difference crosses the threshold some frames after the attack began, so the crossing
//! is not the position. The position is the frame of the **steepest rise of the fast
//! envelope** in the last `LOOKBACK_TIME_CONSTANTS` fast time constants: the frame at which
//! the fast envelope gained most. A step to a level reaches its largest rise at its first frame.
//!
//! # Gaps
//!
//! A detection disarms the detector until `fast - slow` falls under half the threshold
//! (hysteresis), and no two positions are closer than the minimum gap: an attack inside the
//! gap of the last position is consumed, not reported later.
//!
//! A slow rise is not an attack: a linear fade of slope `g` keeps `fast - slow` near
//! `g (tau_slow - tau_fast)` times the mean magnitude and moves the fast envelope by
//! `g * 5 tau_fast` in the lookback, under the threshold when the fade is long enough.

use tutti_buffers::Float;
use tutti_units::{Amplitude, SampleRate, Samples, Seconds};

use super::gain_pass::{floored, level};
use crate::follower::{Follower, Smoothed};

/// How many fast time constants back the steepest rise is looked for: the fast envelope
/// closes `1 - e^-5` (99.3 %) of a step in that time, so a step that has pushed the difference
/// over the threshold began within it.
const LOOKBACK_TIME_CONSTANTS: f64 = 5.0;

/// The share of the threshold the difference must fall under before a new attack counts.
const REARM: f64 = 0.5;

tutti_units::ranged! {
    /// How far the fast envelope must rise above the slow one for an attack: finite and above
    /// zero, in the signal's own scale (a full-scale sample is `1.0`).
    TransientThreshold(Amplitude), TransientThresholdError, "a transient threshold above zero",
    |raw| raw.get() > 0.0 && raw.get().is_finite()
}

tutti_units::ranged! {
    /// The time constant of one of the two envelopes: within `0.1 ms..=1 s`.
    TransientTime(Seconds), TransientTimeError, "an envelope time within 0.1 ms..=1 s",
    |raw| raw.get() >= 1e-4 && raw.get() <= 1.0
}

/// The fast and the slow envelope's time constants of a [`TransientDetector`], the fast one
/// strictly the shorter: a pair that is not so cannot be built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransientTimes {
    fast: TransientTime,
    slow: TransientTime,
}

/// A [`TransientTimes`] whose fast time is not below its slow one, so the envelopes'
/// difference would not mark a rise.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("fast envelope time {fast} is not below slow envelope time {slow}")]
pub struct TransientTimesError {
    /// The fast envelope's time constant; not below `slow`.
    pub fast: Seconds,
    /// The slow envelope's time constant.
    pub slow: Seconds,
}

impl TransientTimes {
    /// The pair of `fast` and `slow`.
    ///
    /// # Errors
    ///
    /// [`TransientTimesError`] when `fast` is not below `slow`.
    pub fn new(fast: TransientTime, slow: TransientTime) -> Result<Self, TransientTimesError> {
        if fast.get() < slow.get() {
            Ok(Self { fast, slow })
        } else {
            Err(TransientTimesError {
                fast: fast.get(),
                slow: slow.get(),
            })
        }
    }

    /// The fast envelope's time constant: the shorter it is, the shorter an attack it sees and
    /// the more of a low tone's ripple it follows.
    #[must_use]
    pub fn fast(&self) -> TransientTime {
        self.fast
    }

    /// The slow envelope's time constant: the level an attack is measured against, and the
    /// slope of a fade it tolerates.
    #[must_use]
    pub fn slow(&self) -> TransientTime {
        self.slow
    }
}

/// 1 ms fast and 30 ms slow.
impl Default for TransientTimes {
    fn default() -> Self {
        Self {
            fast: TransientTime::of(Seconds::new(0.001)),
            slow: TransientTime::of(Seconds::new(0.03)),
        }
    }
}

/// The threshold of a detector that is given none: 0.05.
const DEFAULT_THRESHOLD: Amplitude = Amplitude::new(0.05);

/// The minimum gap of a detector that is given none: 50 ms.
const DEFAULT_MIN_GAP: Seconds = Seconds::new(0.05);

/// An attack detector over one channel, fed a sample at a time.
///
/// Built on the control side for one rate, with chained options; a push allocates nothing. The state and the
/// samples are `F`, the graph's float.
#[derive(Clone, Debug)]
pub struct TransientDetector<F: Float> {
    rate: SampleRate,
    times: TransientTimes,
    fast: Follower<F>,
    slow: Follower<F>,
    envelopes: (Smoothed<F>, Smoothed<F>),
    threshold: F,
    /// The difference under which a disarmed detector arms again.
    rearm: F,
    min_gap: usize,
    /// The fast envelope at each of the last frames, as a ring of the lookback plus one.
    recent: Vec<F>,
    /// The ring's next slot to write.
    head: usize,
    /// How many of the newest ring slots count: the frames since the last detection, the
    /// detecting frame included, up to the ring's length.
    filled: usize,
    armed: bool,
    last: Option<usize>,
    /// The frames pushed.
    frames: usize,
}

impl<F: Float> TransientDetector<F> {
    /// A detector at `rate` with a 0.05 threshold, the default [`TransientTimes`] (1 ms and
    /// 30 ms) and a 50 ms minimum gap.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let times = TransientTimes::default();
        Self {
            rate,
            times,
            fast: Follower::NEUTRAL,
            slow: Follower::NEUTRAL,
            envelopes: (Smoothed::ZERO, Smoothed::ZERO),
            threshold: F::ZERO,
            rearm: F::ZERO,
            min_gap: DEFAULT_MIN_GAP.to_samples(rate).get(),
            recent: Vec::new(),
            head: 0,
            filled: 0,
            armed: true,
            last: None,
            frames: 0,
        }
        .times(times)
        .threshold(TransientThreshold::of(DEFAULT_THRESHOLD))
    }

    /// The envelopes' time constants. Builds the envelopes and the lookback ring (allocates:
    /// the control side), and restarts the detector.
    #[must_use]
    pub fn times(mut self, times: TransientTimes) -> Self {
        let (fast, slow) = (times.fast().get(), times.slow().get());
        let lookback = (LOOKBACK_TIME_CONSTANTS * fast.get() * self.rate.get()).ceil() as usize + 1;
        self.times = times;
        self.fast = Follower::of(floored(fast, fast), self.rate);
        self.slow = Follower::of(floored(slow, slow), self.rate);
        self.recent = vec![F::ZERO; lookback + 1];
        self.reset();
        self
    }

    /// The threshold `fast - slow` and the fast envelope's rise must pass.
    #[must_use]
    pub fn threshold(mut self, threshold: TransientThreshold) -> Self {
        let threshold = threshold.get();
        self.threshold = F::from_unit(threshold);
        self.rearm = F::from_unit(Amplitude::new(threshold.get() * REARM));
        self
    }

    /// The least spacing between reported positions.
    #[must_use]
    pub fn min_gap(mut self, gap: Samples) -> Self {
        self.min_gap = gap.get();
        self
    }

    /// Back to silence: both envelopes at 0, armed, no position kept. No allocation.
    pub fn reset(&mut self) {
        self.envelopes = (Smoothed::ZERO, Smoothed::ZERO);
        self.recent.fill(F::ZERO);
        self.head = 0;
        self.filled = 0;
        self.armed = true;
        self.last = None;
        self.frames = 0;
    }

    /// Takes the next sample; returns the position, counted from the first sample pushed since
    /// the last [`reset`](Self::reset), of the attack this sample confirmed. The position is at
    /// or before this sample. A NaN reads as quiet and an infinity as the largest finite level.
    /// No allocation.
    pub fn push(&mut self, x: F) -> Option<Samples> {
        let key = level(x);
        let fast = self.fast.follow_raw(&mut self.envelopes.0, key);
        let slow = self.slow.follow_raw(&mut self.envelopes.1, key);
        let at = self.frames;
        self.frames += 1;

        self.recent[self.head] = fast;
        self.head = (self.head + 1) % self.recent.len();
        self.filled = (self.filled + 1).min(self.recent.len());

        let excess = fast - slow;
        if !self.armed {
            if excess < self.rearm {
                self.armed = true;
            }
            return None;
        }
        let attack = excess > self.threshold && fast - self.lowest() > self.threshold;
        if !attack {
            return None;
        }
        self.armed = false;
        let spaced = self.last.is_none_or(|last| at - last >= self.min_gap);
        let position = self.steepest(at);
        self.filled = 1;
        if !spaced {
            return None;
        }
        self.last = Some(position);
        Some(Samples::new(position))
    }

    /// The `i`-th newest fast envelope value of the counted frames, 0 the newest.
    fn newest(&self, i: usize) -> F {
        let len = self.recent.len();
        self.recent[(self.head + len - 1 - i) % len]
    }

    /// The lowest fast envelope value among the counted frames.
    fn lowest(&self) -> F {
        (0..self.filled)
            .map(|i| self.newest(i))
            .fold(self.newest(0), |low, v| if v < low { v } else { low })
    }

    /// The frame of the largest rise of the fast envelope among the counted frames, the frame
    /// `at` being the newest; the earliest of equal rises. `at` itself when one frame counts.
    fn steepest(&self, at: usize) -> usize {
        let mut best = at;
        let mut best_rise = None;
        for back in (0..self.filled.saturating_sub(1)).rev() {
            let rise = self.newest(back) - self.newest(back + 1);
            if best_rise.is_none_or(|b| rise > b) {
                best_rise = Some(rise);
                best = at - back;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests;
