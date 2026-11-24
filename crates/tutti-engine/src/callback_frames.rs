//! The sizes of the callbacks a stream is handed: what the device delivers, as
//! the callback itself counts it.
//!
//! A device is opened asking for a callback size, and the backend may deliver
//! another, or a different one each time. [`CallbackFrames`] holds the smallest
//! and the largest callback one stream ran since the control side last took
//! them, so a reading covers every callback in between, not the last one.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::OutputQuantum;

/// The value of `smallest` that holds no callback.
const NO_SMALLEST: u32 = u32::MAX;
/// The value of `largest` that holds no callback: a callback is at least one
/// frame.
const NO_LARGEST: u32 = 0;

/// The smallest and the largest callback of one interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallbackSpan {
    smallest: OutputQuantum,
    largest: OutputQuantum,
}

impl CallbackSpan {
    /// The span from `smallest` to `largest`, or `None` when `smallest` is
    /// the larger.
    #[must_use]
    pub fn new(smallest: OutputQuantum, largest: OutputQuantum) -> Option<Self> {
        (smallest.get() <= largest.get()).then_some(Self { smallest, largest })
    }

    /// The span of callbacks that were all `size` frames.
    #[must_use]
    pub fn steady(size: OutputQuantum) -> Self {
        Self {
            smallest: size,
            largest: size,
        }
    }

    /// The fewest frames in a callback of the interval.
    #[must_use]
    pub fn smallest(&self) -> OutputQuantum {
        self.smallest
    }

    /// The most frames in a callback of the interval.
    #[must_use]
    pub fn largest(&self) -> OutputQuantum {
        self.largest
    }

    /// The size every callback of the interval had, or `None` when they
    /// differed.
    #[must_use]
    pub fn steady_size(&self) -> Option<OutputQuantum> {
        (self.smallest == self.largest).then_some(self.smallest)
    }
}

/// The smallest and largest callback one stream ran since the last
/// [`take`](Self::take): two atomics, each updated by one wait-free
/// `fetch_min` or `fetch_max`.
///
/// A callback is one to [`MAX_FRAMES`](crate::MAX_FRAMES) frames here: the
/// count is of what the callback rendered, and a longer buffer's tail is
/// silenced and counted as truncated. A callback of no frames is not a size
/// and is not recorded.
#[derive(Debug)]
pub struct CallbackFrames {
    smallest: AtomicU32,
    largest: AtomicU32,
}

impl Default for CallbackFrames {
    fn default() -> Self {
        Self::new()
    }
}

impl CallbackFrames {
    /// A cell with no callback.
    #[must_use]
    pub fn new() -> Self {
        Self {
            smallest: AtomicU32::new(NO_SMALLEST),
            largest: AtomicU32::new(NO_LARGEST),
        }
    }

    /// Records a callback of `frames`. Wait-free: callable on the audio
    /// thread.
    pub fn record(&self, frames: OutputQuantum) {
        // At most `MAX_FRAMES`, far under `u32::MAX`.
        let frames = frames.max_block().get_u32();
        self.smallest.fetch_min(frames, Ordering::Relaxed);
        self.largest.fetch_max(frames, Ordering::Relaxed);
    }

    /// The span of the callbacks since the last take (or since the cell was
    /// made or reset), emptying the cell; `None` when none ran.
    ///
    /// The two ends are swapped one after the other, so a callback that lands
    /// between the swaps is counted in this interval's span or the next one's,
    /// at worst split between them (its size in one end of each). That is
    /// harmless: a reading needs several intervals to mean anything.
    pub fn take(&self) -> Option<CallbackSpan> {
        let smallest = self.smallest.swap(NO_SMALLEST, Ordering::Relaxed);
        let largest = self.largest.swap(NO_LARGEST, Ordering::Relaxed);
        let (smallest, largest) = match (smallest, largest) {
            (NO_SMALLEST, NO_LARGEST) => return None,
            // One end of a callback that landed between the swaps.
            (NO_SMALLEST, only) | (only, NO_LARGEST) => (only, only),
            both => both,
        };
        let size = |frames: u32| OutputQuantum::new(frames as usize);
        CallbackSpan::new(size(smallest)?, size(largest)?)
    }

    /// Forgets the callbacks, as a restart does: the new stream has run none
    /// yet.
    pub fn reset(&self) {
        self.smallest.store(NO_SMALLEST, Ordering::Relaxed);
        self.largest.store(NO_LARGEST, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(frames: usize) -> OutputQuantum {
        OutputQuantum::of(frames)
    }

    /// A take covers every callback since the last one, empties the cell, and
    /// reads nothing before a callback and after a reset.
    #[test]
    fn a_take_spans_every_callback_since_the_last_and_empties_the_cell() {
        let cell = CallbackFrames::new();
        assert_eq!(cell.take(), None, "no callback yet");
        for frames in [480, 512, 471] {
            cell.record(q(frames));
        }
        let span = cell.take().expect("callbacks ran");
        assert_eq!((span.smallest(), span.largest()), (q(471), q(512)));
        assert_eq!(span.steady_size(), None);
        assert_eq!(cell.take(), None, "a take empties it");
        cell.record(q(480));
        assert_eq!(cell.take(), Some(CallbackSpan::steady(q(480))));
        cell.record(q(480));
        cell.reset();
        assert_eq!(cell.take(), None, "a reset forgets");
    }

    /// A span is ordered: the smaller end first.
    #[test]
    fn a_span_has_its_smaller_end_first() {
        assert_eq!(CallbackSpan::new(q(512), q(480)), None);
        assert!(CallbackSpan::new(q(480), q(512)).is_some());
    }
}
