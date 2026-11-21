//! The helpers a design shares: the prewarped gain and the ramp fraction.

use tutti_buffers::Float;
use tutti_units::{Hz, Normalized, SampleRate, Samples};

/// `k / n` of the way along a ramp of `n` steps, as data of the width `F`: how far a blend
/// of two designs has gone at step `k`.
#[inline]
pub(crate) fn along<F: Float>(k: u32, n: u32) -> F {
    F::from_unit(Normalized::progress(
        Samples::new(k as usize),
        Samples::new(n as usize),
    ))
}

/// The prewarped integrator gain `tan(pi fc / fs)` of a trapezoidal section at the corner
/// `fc` and the rate: where the section's frequency axis sits. It diverges at the Nyquist, so
/// a caller holds `fc` below it.
#[must_use]
pub(crate) fn prewarp(fc: Hz, rate: SampleRate) -> f64 {
    libm::tan(core::f64::consts::PI * fc.get() / rate.get())
}

/// The frames under which a time is none: one that short has no frame to reach its end in, and a
/// slope over it would not be finite.
pub(crate) const NO_FRAME: f64 = 1e-12;
