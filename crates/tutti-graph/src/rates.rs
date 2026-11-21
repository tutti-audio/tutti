//! The sample rates a node runs at.

use tutti_units::SampleRate;

/// The sample rates a node runs at: every rate from `min` to `max`, both
/// included. [`Shape::with_rates`](crate::Shape::with_rates) declares it,
/// [`SampleRates::ALL`] being the default of a node that runs at any.
///
/// A session's `Graph::set_sample_rate` and a fork ([`Editor::fork`], a bounce
/// or an export) refuse a rate that a node's rates leave out, and change
/// nothing.
///
/// [`Editor::fork`]: crate::Editor::fork
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleRates {
    min: SampleRate,
    max: SampleRate,
}

// A `SampleRate` is finite (never NaN), so equality is reflexive.
impl Eq for SampleRates {}

impl SampleRates {
    /// Every rate there is: the default of a [`Shape`](crate::Shape).
    pub const ALL: Self = Self {
        min: SampleRate::of(f64::MIN_POSITIVE),
        max: SampleRate::of(f64::MAX),
    };

    /// The rates from `min` to `max`, both included; `None` when `min` is
    /// above `max`.
    #[must_use]
    pub const fn new(min: SampleRate, max: SampleRate) -> Option<Self> {
        if min.get() <= max.get() {
            Some(Self { min, max })
        } else {
            None
        }
    }

    /// [`new`](Self::new) for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `min` is above `max`.
    #[must_use]
    pub const fn of(min: SampleRate, max: SampleRate) -> Self {
        match Self::new(min, max) {
            Some(rates) => rates,
            None => panic!("the lowest sample rate is above the highest"),
        }
    }

    /// The lowest rate, included.
    #[must_use]
    pub const fn min(self) -> SampleRate {
        self.min
    }

    /// The highest rate, included.
    #[must_use]
    pub const fn max(self) -> SampleRate {
        self.max
    }

    /// Whether `rate` is one of them.
    #[must_use]
    pub const fn contains(self, rate: SampleRate) -> bool {
        self.min.get() <= rate.get() && rate.get() <= self.max.get()
    }
}

impl core::fmt::Display for SampleRates {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} to {}", self.min, self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: SampleRate = SampleRate::SR_44K1;
    const HIGH: SampleRate = SampleRate::SR_96K;

    /// Both ends are included and nothing outside is.
    #[test]
    fn contains_includes_both_ends_and_nothing_beyond() {
        let rates = SampleRates::of(LOW, HIGH);
        assert!(rates.contains(LOW));
        assert!(rates.contains(SampleRate::SR_48K));
        assert!(rates.contains(HIGH));
        assert!(!rates.contains(SampleRate::SR_8K));
        assert!(!rates.contains(SampleRate::SR_192K));
        assert!(!rates.contains(SampleRate::of(LOW.get() - 1.0)));
        assert!(!rates.contains(SampleRate::of(HIGH.get() + 1.0)));
    }

    /// A range of one rate holds that rate; a reversed one is refused.
    #[test]
    fn new_refuses_a_reversed_range_and_holds_a_single_rate() {
        assert_eq!(SampleRates::new(HIGH, LOW), None);
        let one = SampleRates::new(LOW, LOW).expect("one rate");
        assert!(one.contains(LOW));
        assert!(!one.contains(SampleRate::SR_48K));
    }

    /// The default holds every named rate.
    #[test]
    fn all_holds_every_named_rate() {
        for rate in [
            SampleRate::SR_8K,
            SampleRate::SR_44K1,
            SampleRate::SR_48K,
            SampleRate::SR_192K,
        ] {
            assert!(SampleRates::ALL.contains(rate));
        }
    }
}
