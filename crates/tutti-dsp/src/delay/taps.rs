//! What the delay and the reverbs share: the length of a ring-out.

use tutti_types::{Db, SampleRate, Seconds};

use crate::onepole::ring_frames;

/// The trips an echo makes before it is `floor` under the first one, at loop gain `q`.
pub(crate) fn trips(q: f64, floor: Db) -> f64 {
    ring_frames(q, floor).ceil()
}

/// How long the echoes of a delay of `frames` frames ring: `1 + trips` periods of the
/// delay and its interpolator's spread (the echo `k` is spread over `k + 1` frames at
/// most; two per trip errs long), in seconds.
pub(crate) fn ring_seconds(frames: usize, trips: f64, rate: SampleRate) -> Seconds {
    Seconds((1.0 + trips) * (frames as f64 + 2.0) / rate.get())
}
