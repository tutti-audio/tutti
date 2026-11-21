//! What the string models share: the loop's tuning and its decay.
//!
//! A string loop of `P` frames per cycle holds a fixed delay `D` (its loss filters) and a
//! delay line of `N` whole frames, and a first-order all-pass supplies the fraction: the
//! all-pass `H(z) = (eta + z^-1) / (1 + eta z^-1)` has the phase delay
//! `tau(w) = 1 - (2 / w) atan2(eta sin w, 1 + eta cos w)` frames, and
//! `eta = sin((1 - d) w / 2) / sin((1 + d) w / 2)` makes it exactly `d` at `w`. The loop is
//! then `N + D + d = P` frames long at the fundamental `w = 2 pi / P`, so the string sounds
//! `fs / P` exactly.
//!
//! The delay `d` lies in `[1, 1 + step)` and the argument `(1 + d) w / 2` stays at or under
//! `pi / 2` while `P >= 2 (2 + step)`, which keeps `|eta| < 1`: the all-pass is stable. A
//! caller holds its frequency to `fs / ` [`min_period`] and no higher.

use tutti_units::{Feedback, Hz, SampleRate, SampleSpan, Samples, Seconds};

/// `ln 1000`: the natural-log span of 60 dB of amplitude.
pub(crate) const LN_1000: f64 = 3.0 * core::f64::consts::LN_10;

/// The shortest loop the tuning designs: `2 (2 + step)` frames.
pub(crate) fn min_period(step: usize) -> SampleSpan {
    SampleSpan::new(2.0 * (2.0 + step as f64)).unwrap_or(SampleSpan::ZERO)
}

/// The loop's period in frames at `frequency`, held between `lowest` and the shortest period
/// the tuning designs. A frequency that is not finite and positive reads as `lowest`.
pub(crate) fn period(frequency: Hz, lowest: Hz, rate: SampleRate, step: usize) -> SampleSpan {
    let f = if frequency.get().is_finite() && frequency.get() > 0.0 {
        frequency
    } else {
        lowest
    };
    let frames = rate.get() / f.get().max(lowest.get());
    SampleSpan::clamped(frames).max(min_period(step))
}

/// The loop's split into whole frames and an all-pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tuned {
    /// The delay line's frames, a multiple of the step.
    pub whole: Samples,
    /// The all-pass's phase delay at the fundamental, in `[1, 1 + step)`.
    pub delay: SampleSpan,
    /// The coefficient of that all-pass in the form of [`AllPassFilter`](crate::AllPassFilter)
    /// (`g = -eta`, in `[0, 1)`).
    pub gain: Feedback,
}

/// Splits `period` into `whole` frames (a multiple of `step`), the `fixed` delay of the
/// loop's other parts and an all-pass of delay `d` in `[1, 1 + step)`.
pub(crate) fn tune(period: SampleSpan, fixed: SampleSpan, step: usize) -> Tuned {
    let step_frames = step as f64;
    let remaining = period.get() - fixed.get();
    let whole = step_frames * libm::floor((remaining - 1.0) / step_frames);
    let d = remaining - whole;
    let w = core::f64::consts::TAU / period.get();
    let eta = libm::sin((1.0 - d) * w / 2.0) / libm::sin((1.0 + d) * w / 2.0);
    Tuned {
        whole: Samples::new(whole as usize),
        delay: SampleSpan::clamped(d),
        gain: Feedback::new_clamped(-eta),
    }
}

/// The gain a loop of `trip` seconds holds so that the signal falls 60 dB in `t60`:
/// `exp(-ln(1000) trip / t60)`, below 1 for every finite `t60` above zero. The loop's average
/// and all-pass have `|H| <= 1`, so any gain below 1 is stable: [`Feedback::MAX_STABLE`], a
/// comb's feedback bound, does not apply, and a string rings as long as it is asked to. The gain
/// is held at the largest `f64` below 1 (an infinite `t60`, or a `t60` so long that the
/// exponential rounds to 1). A `t60` that is not above zero or is NaN is a gain of 0.
pub(crate) fn per_trip_decay(trip: Seconds, t60: Seconds) -> Feedback {
    if t60.get() > 0.0 && trip.get() >= 0.0 {
        Feedback::new(libm::exp(-LN_1000 * (trip / t60)).min(LARGEST_BELOW_ONE))
    } else {
        Feedback::NONE
    }
}

/// The largest `f64` below 1, `1 - 2^-53`.
const LARGEST_BELOW_ONE: f64 = 1.0 - f64::EPSILON / 2.0;
