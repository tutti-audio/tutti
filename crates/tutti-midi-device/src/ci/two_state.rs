//! The two-state reading of a 32-bit controller value.

use tutti_ump::ControlValue;

/// A controller value read as a switch: the lower half of the range is one
/// state, the upper half the other (M2-121 §6.1.1, §6.1.2; M2-122 Table 5,
/// Table 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TwoState {
    /// `0x00000000` to `0x7FFFFFFF`: off, normal, slow, rotate, 2nd harmonic.
    Low,
    /// `0x80000000` to `0xFFFFFFFF`: on, soft, fast, stop, 3rd harmonic.
    High,
}

impl TwoState {
    /// The state `value` is in.
    #[must_use]
    pub const fn of(value: ControlValue) -> Self {
        if value.get() >= 0x8000_0000 {
            Self::High
        } else {
            Self::Low
        }
    }

    /// The value a sender gives the state: the end of its half, `0x00000000`
    /// or `0xFFFFFFFF` (M2-122 Table 5 note).
    #[must_use]
    pub const fn value(self) -> ControlValue {
        match self {
            Self::Low => ControlValue::new(0),
            Self::High => ControlValue::MAX,
        }
    }
}
