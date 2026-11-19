//! The one-pole's pole and how long it rings: `exp(-2 pi fc / fs)` and the frames its
//! decay takes to fall to a floor.

use tutti_units::{Db, Hz, SampleRate};

/// The pole `exp(-2 pi fc / fs)` of the one-pole low-pass of cutoff `cutoff` at `rate`.
pub(crate) fn cutoff_pole(cutoff: Hz, rate: SampleRate) -> f64 {
    libm::exp(-cutoff.angle_per_frame(rate).get())
}

/// The complement `1 - exp(-2 pi fc / fs)` of that pole, solved without the cancellation a
/// pole near 1 (a low cutoff at a high rate) would suffer in `1 - pole`: the coefficient a
/// one-pole's update `y += c (x - y)` runs on.
pub(crate) fn cutoff_complement(cutoff: Hz, rate: SampleRate) -> f64 {
    -libm::expm1(-cutoff.angle_per_frame(rate).get())
}

/// The frames, fractional (a count `Samples` does not cover), for the decay `pole^n` to reach `floor`: `ln(floor) / ln(pole)`.
/// A pole of 0 or less decays at once (0 frames), and one of 1 or more never does
/// (infinite); a floor at or above 0 dB is already reached (0 frames).
pub(crate) fn ring_frames(pole: f64, floor: Db) -> f64 {
    if pole <= 0.0 {
        return 0.0;
    }
    if pole >= 1.0 {
        return f64::INFINITY;
    }
    let target = libm::log(floor.to_amplitude().get());
    (target / libm::log(pole)).max(0.0)
}

#[cfg(test)]
mod tests;
