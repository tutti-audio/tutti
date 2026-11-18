//! The range and split checks every view shares, so each speaks one message,
//! in frames.

use core::ops::Range;
use tutti_units::Samples;

/// The frame range as indices, or `None` unless `start <= end <= frames`.
#[inline]
pub(crate) fn get(range: &Range<Samples>, frames: usize) -> Option<Range<usize>> {
    (range.start <= range.end && range.end.get() <= frames)
        .then(|| range.start.get()..range.end.get())
}

/// The frame range as indices.
///
/// # Panics
///
/// Unless `start <= end <= frames`.
#[inline]
#[track_caller]
pub(crate) fn slice(range: &Range<Samples>, frames: usize) -> Range<usize> {
    match get(range, frames) {
        Some(r) => r,
        None => panic!(
            "slice {}..{} is out of bounds for {} frames",
            range.start, range.end, frames
        ),
    }
}

/// The split point as an index.
///
/// # Panics
///
/// If `at` is past `frames`.
#[inline]
#[track_caller]
pub(crate) fn split(at: Samples, frames: usize) -> usize {
    assert!(
        at.get() <= frames,
        "split at {at} is past the {frames} frames"
    );
    at.get()
}

/// The index of frame `i`.
///
/// # Panics
///
/// If `i` is not below `frames`.
#[inline]
#[track_caller]
pub(crate) fn index(i: Samples, frames: usize) -> usize {
    assert!(
        i.get() < frames,
        "frame {i} is out of bounds for {frames} frames"
    );
    i.get()
}
