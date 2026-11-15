//! The two-state reading of a 32-bit controller value.

use tutti_ump::ControlValue;

/// A controller value read as a switch: the lower half of the range is one
/// state, the upper half the other (M2-121 §6.1.1, §6.1.2; M2-122 Table 5,
/// Table 6). What the two states mean is the control's [`SwitchMeaning`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TwoState {
    /// `0x00000000` to `0x7FFFFFFF`.
    Low,
    /// `0x80000000` to `0xFFFFFFFF`.
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

    /// The value a sender gives the state: the end of its half,
    /// `0x00000000` or `0xFFFFFFFF`. M2-122 Table 5 says a two-position
    /// controller should send these; M2-121 §6.1 states only the ranges of
    /// its switches, so for it these are this crate's choice of value inside
    /// the range.
    #[must_use]
    pub const fn value(self) -> ControlValue {
        match self {
            Self::Low => ControlValue::new(0),
            Self::High => ControlValue::MAX,
        }
    }
}

/// What the two states of a switch control mean (M2-121 §6.1.2.2 to
/// §6.1.2.6; M2-122 Table 5, Table 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SwitchMeaning {
    /// Off or on.
    OffOn,
    /// Normal or soft.
    NormalSoft,
    /// Slow or fast.
    SlowFast,
    /// Rotate or stop.
    RotateStop,
    /// The 2nd or the 3rd harmonic.
    SecondThirdHarmonic,
}

impl SwitchMeaning {
    /// The name of `state` under this meaning.
    #[must_use]
    pub const fn name(self, state: TwoState) -> &'static str {
        match (self, state) {
            (Self::OffOn, TwoState::Low) => "off",
            (Self::OffOn, TwoState::High) => "on",
            (Self::NormalSoft, TwoState::Low) => "normal",
            (Self::NormalSoft, TwoState::High) => "soft",
            (Self::SlowFast, TwoState::Low) => "slow",
            (Self::SlowFast, TwoState::High) => "fast",
            (Self::RotateStop, TwoState::Low) => "rotate",
            (Self::RotateStop, TwoState::High) => "stop",
            (Self::SecondThirdHarmonic, TwoState::Low) => "2nd harmonic",
            (Self::SecondThirdHarmonic, TwoState::High) => "3rd harmonic",
        }
    }
}

#[cfg(test)]
mod tests;
