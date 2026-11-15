//! What the delay and the modulated delay share: a control that is not finite, one
//! interpolated read as taps, and the length of a ring-out.

use tutti_types::{Db, SampleRate, Seconds};

use crate::{InterpolationMode, cubic_hermite};

/// `x` if it is finite, else `default`.
pub(crate) fn finite_or(x: f32, default: f32) -> f32 {
    if x.is_finite() { x } else { default }
}

/// What one fractional read of a line is, as taps: `(frames back from the newest write,
/// weight)`, the sum read by [`DelayLine::read_sample`](crate::DelayLine::read_sample) at
/// `delay` (clamped to `cap`).
/// The index arithmetic is the line's own, so a model built from these has the line's
/// behaviour at the edges too (a cubic read at delay below 1 reads one sample twice).
pub(crate) fn read_taps(
    delay: f32,
    cap: usize,
    mode: InterpolationMode,
) -> ([(usize, f32); 4], usize) {
    let delay = delay.clamp(0.0, cap as f32);
    let mut taps = [(0, 0.0); 4];
    match mode {
        InterpolationMode::None => {
            taps[0] = ((delay.round() as usize).min(cap), 1.0);
            (taps, 1)
        }
        InterpolationMode::Linear => {
            let floor = delay.floor() as usize;
            let frac = delay - floor as f32;
            taps[0] = (floor.min(cap), 1.0 - frac);
            taps[1] = ((floor + 1).min(cap), frac);
            (taps, 2)
        }
        InterpolationMode::CubicHermite => {
            let floor = delay.floor() as usize;
            let frac = delay - floor as f32;
            let at = [floor.saturating_sub(1), floor, floor + 1, floor + 2];
            for (k, slot) in taps.iter_mut().enumerate() {
                let mut e = [0.0f32; 4];
                e[k] = 1.0;
                // Linear in the four taps: the weight of tap `k` is the read of its basis.
                *slot = (at[k].min(cap), cubic_hermite(e[0], e[1], e[2], e[3], frac));
            }
            (taps, 4)
        }
    }
}

/// The trips an echo makes before it is `floor` under the first one, at loop gain `q`.
pub(crate) fn trips(q: f64, floor: Db) -> f64 {
    if q <= 0.0 {
        return 0.0;
    }
    let target = libm::log(10f64.powf(f64::from(floor.get()) / 20.0));
    (target / libm::log(q)).ceil().max(0.0)
}

/// How long the echoes of a delay of `frames` frames ring: `1 + trips` periods of the
/// delay and its interpolator's spread (the echo `k` is spread over `k + 1` frames at
/// most; two per trip errs long), in seconds.
pub(crate) fn ring_seconds(frames: usize, trips: f64, rate: SampleRate) -> Seconds {
    Seconds((1.0 + trips) * (frames as f64 + 2.0) / rate.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cubic read's taps are a partition of unity at every fraction: a constant
    /// signal reads back as itself.
    #[test]
    fn the_cubic_taps_sum_to_one() {
        for i in 0..=1000 {
            let delay = 10.0 + i as f32 / 1000.0;
            let (taps, n) = read_taps(delay, 100, InterpolationMode::CubicHermite);
            let sum: f32 = taps[..n].iter().map(|t| t.1).sum();
            assert!((sum - 1.0).abs() < 1e-5, "delay {delay}: sum {sum}");
        }
    }
}
