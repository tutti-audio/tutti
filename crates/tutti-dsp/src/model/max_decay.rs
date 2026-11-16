//! [`MaxDecay`]: the longest decay a reverb is built to.

use tutti_types::Seconds;

tutti_types::ranged! {
    /// The longest decay a reverb's control may ask for.
    ///
    /// A range-checked value: finite, at least [`MIN_DECAY`](crate::MIN_DECAY) and at most
    /// [`CAP`](Self::CAP).
    /// [`new`](Self::new) refuses anything else and never clamps: a network sized from it
    /// becomes a reverb's allocations (a 60 s decay at 192 kHz is already some 11.5 M
    /// frames of line to hold).
    MaxDecay(Seconds), MaxDecayError, "a longest decay, at least 50 milliseconds and at most 60 seconds",
    |raw| raw.get().is_finite() && raw.get() >= crate::MIN_DECAY.get() && raw.get() <= MaxDecay::CAP.get()
}

impl MaxDecay {
    /// The longest decay a control may ask for by default: 8 s.
    pub const DEFAULT: MaxDecay = MaxDecay(Seconds(8.0));
    /// The most [`new`](Self::new) accepts: 60 s.
    pub const CAP: Seconds = Seconds(60.0);
}

#[cfg(test)]
mod tests;
