//! What the delay and the reverbs share: a control that is not finite, and the length of a
//! ring-out.

use tutti_types::{Db, SampleRate, Seconds};

use crate::level::db_to_ratio_f64;

/// `x` if it is finite, else `default`.
pub(crate) fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

/// The trips an echo makes before it is `floor` under the first one, at loop gain `q`.
pub(crate) fn trips(q: f64, floor: Db) -> f64 {
    if q <= 0.0 {
        return 0.0;
    }
    let target = libm::log(db_to_ratio_f64(f64::from(floor.get())));
    (target / libm::log(q)).ceil().max(0.0)
}

/// How long the echoes of a delay of `frames` frames ring: `1 + trips` periods of the
/// delay and its interpolator's spread (the echo `k` is spread over `k + 1` frames at
/// most; two per trip errs long), in seconds.
pub(crate) fn ring_seconds(frames: usize, trips: f64, rate: SampleRate) -> Seconds {
    Seconds((1.0 + trips) * (frames as f64 + 2.0) / rate.get())
}
