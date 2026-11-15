//! [`MaxDelay`]: the longest delay a delay line is built to hold, the [`Domain`] of a
//! delay's time.

use tutti_types::{SampleRate, Seconds};

use crate::Domain;

tutti_types::ranged! {
    /// The longest delay a delay's time control may ask for: a [`Domain`] of
    /// [`Reach`](crate::Reach), and the size of the line a [`DelayBank`](crate::DelayBank)
    /// allocates.
    ///
    /// A range-checked value: finite, above zero and at
    /// most [`CAP`](Self::CAP). [`new`](Self::new) refuses anything else and never clamps: the
    /// maximum becomes an allocation (a line per channel), so a NaN, a negative span or a
    /// finite one of 1e30 seconds must not reach it.
    #[derive(PartialOrd)]
    MaxDelay(Seconds), MaxDelayError, "a longest delay, above zero and at most 10 seconds",
    |raw| raw.get().is_finite() && raw.get() > 0.0 && raw.get() <= MaxDelay::CAP.get()
}

impl MaxDelay {
    /// The longest delay of a node built without one: 2 s.
    pub const DEFAULT: Self = Self(Seconds(2.0));
    /// The most [`new`](Self::new) accepts: 10 s. A line of that length at 192 kHz is
    /// 1.92 million frames of `f32`, so 16 channels hold 123 MB: the cap is what a
    /// capacity the node allocates at the device's rate can be.
    pub const CAP: Seconds = Seconds(10.0);

    /// The frames a line holds for it at `rate`: `ceil(max * rate)`, from the exact
    /// product. Saturated at 16.7 million frames: a rate no device has must not make an
    /// allocation that aborts.
    #[must_use]
    pub fn frames(self, rate: SampleRate) -> usize {
        self.0.to_samples_ceil(rate).get().min(MAX_FRAMES)
    }
}

/// The most frames a line is allocated for whatever the rate: 16.7 million, over 87 s
/// at 192 kHz, far past the 1.92 million that [`MaxDelay::CAP`] asks for at any rate a
/// device runs at. It guards only an absurd rate, since a prepare must not abort.
pub(crate) const MAX_FRAMES: usize = 1 << 24;

impl Default for MaxDelay {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Domain for MaxDelay {
    const DEFAULT: Self = MaxDelay::DEFAULT;
}
