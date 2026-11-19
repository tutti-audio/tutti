//! The RT60 to loop-gain formula and its inverse.
//!
//! A signal that goes round a loop of `d` frames at gain `g` loses
//! `20 log10 g` dB per trip, and RT60 is the time to lose 60 dB, so
//! `g = 10^(-3 d / (RT60 fs))`. These two functions are that formula both
//! ways, in `f64` and through `libm` so a design does not depend on the
//! platform's C math library.

use tutti_units::{Amplitude, Db, SampleRate, SampleSpan, Seconds};

/// The loop gain that makes a loop of `delay` frames decay by 60 dB in `rt60`:
/// `g = 10^(-3 d / (RT60 fs))`.
///
/// The gain is an `Amplitude`: a loop gain is non-negative. A
/// zero, negative or NaN `rt60` has no memory and gives gain 0; an infinite
/// one, or one so long its frame count overflows, never decays and gives 1.
#[must_use]
pub(crate) fn loop_gain(delay: SampleSpan, rt60: Seconds, rate: SampleRate) -> Amplitude {
    let Some(trips) = SampleSpan::from_seconds(rt60, rate) else {
        // Not a span: a positive RT60 whose frames overflow (infinity included)
        // never decays, anything else (negative, NaN) has no memory.
        return if rt60.get() > 0.0 {
            Amplitude::UNITY
        } else {
            Amplitude::SILENT
        };
    };
    if trips == SampleSpan::ZERO {
        return Amplitude::SILENT;
    }
    Db(-60.0 * delay.get() / trips.get()).to_amplitude()
}

/// The RT60 of a loop of `delay` frames at gain `gain`, `-60 d / (fs · 20 log10 g)`: the
/// time a signal circulating in it takes to fall 60 dB.
///
/// A gain of 0 decays at once (zero seconds); a gain of 1 or more, and a gain
/// that is NaN or negative (not a loop gain), never settle: infinity.
#[must_use]
pub fn rt60_from_gain(gain: Amplitude, delay: SampleSpan, rate: SampleRate) -> Seconds {
    decay_seconds(delay.get(), Db::from_amplitude(gain), rate)
}

/// RT60 of a loop set (`delay_sum` frames in all, `db_sum` the sum of the loop gains' decibels
/// per trip): `-60 Σm / (fs Σ dB)`. The frames are a bare count because an unbounded line is
/// infinite, which a [`SampleSpan`] cannot hold.
pub(crate) fn decay_seconds(delay_sum: f64, db_sum: Db, rate: SampleRate) -> Seconds {
    let db_sum = db_sum.get();
    Seconds(if db_sum.is_nan() || db_sum >= 0.0 {
        f64::INFINITY
    } else if db_sum == f64::NEG_INFINITY {
        0.0
    } else {
        -60.0 * delay_sum / (rate.get() * db_sum)
    })
}

#[cfg(test)]
mod tests;
