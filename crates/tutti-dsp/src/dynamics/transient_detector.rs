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

/// An attack detector over one channel, fed a sample at a time.
///
/// Built on the control side for one rate; a push allocates nothing. The state and the
/// samples are `F`, the graph's float.
#[derive(Clone, Debug)]
pub struct TransientDetector<F: Float> {
    fast: Follower<F>,
    slow: Follower<F>,
    envelopes: (Smoothed<F>, Smoothed<F>),
    threshold: F,
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
    /// A detector at `rate` whose fast envelope has the time constant `fast` and whose slow
    /// one has `slow`, reporting attacks where `fast - slow` exceeds `threshold` at least
    /// `min_gap` apart. The times are read as the shaper reads them (floored at 0).
    #[must_use]
    pub fn new(
        rate: SampleRate,
        fast: Seconds,
        slow: Seconds,
        threshold: Amplitude,
        min_gap: Samples,
    ) -> Self {
        let lookback =
            (LOOKBACK_TIME_CONSTANTS * fast.get().max(0.0) * rate.get()).ceil() as usize + 1;
        Self {
            fast: Follower::of(floored(fast, fast), rate),
            slow: Follower::of(floored(slow, slow), rate),
            envelopes: (Smoothed::ZERO, Smoothed::ZERO),
            threshold: F::from_unit(threshold),
            min_gap: min_gap.get(),
            recent: vec![F::ZERO; lookback + 1],
            head: 0,
            filled: 0,
            armed: true,
            last: None,
            frames: 0,
        }
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
            if excess < self.threshold * F::lit(REARM) {
                self.armed = true;
            }
            return None;
        }
        if !(excess > self.threshold) || !(fast - self.lowest() > self.threshold) {
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
