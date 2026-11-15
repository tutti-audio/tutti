//! The Rotary Speaker Profile (M2-122) as typed controls.
//!
//! While the Profile is enabled on a channel, fourteen Registered Controllers
//! set the speaker (§3.2, Table 5 and Table 6); [`RotaryControl`] is the
//! table: the controller, how its value is read and its default. The data
//! routes nothing; an application binds it. [`RotarySpeaker::enabled_on`] says
//! whether a peer has the Profile enabled on a channel.

use tutti_ump::{ControlValue, MidiChannel, Rpn, U7};

use super::{CiDeviceId, ProfileId, ProfileState, ProfileTable};

/// The Profile itself (M2-122 §3.1, Table 4): a single level, so a single
/// [`ProfileId`], [`ProfileId::ROTARY_SPEAKER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RotarySpeaker {}

impl RotarySpeaker {
    /// Whether the Profile is enabled on `channel` in `table`. The Profile
    /// works on a Channel, so only a channel address counts.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable, channel: MidiChannel) -> bool {
        table
            .get(CiDeviceId::Channel(channel), ProfileId::ROTARY_SPEAKER)
            .is_some_and(|entry| entry.state() == ProfileState::Enabled)
    }
}

/// How a controller's value is read (M2-122 Table 5, Table 6, the Value Range
/// column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RotaryValue {
    /// A two-state switch: [`TwoState`](super::TwoState), the labels the
    /// variant names.
    Switch,
    /// `0x00000000` to `0xFFFFFFFF`, slowest to fastest.
    Speed,
    /// `0x00000000` to `0xFFFFFFFF`, off to full.
    Level,
}

/// A Registered Controller of the Profile (M2-122 §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RotaryControl {
    /// Rotary Speed, RC `0x60 0x20`: slow (chorale) or fast (tremolo). The one
    /// controller a Device shall respond to (§3.2.1); it may also read it as
    /// continuous.
    Speed,
    /// Rotary Effect, RC `0x60 0x21`: off or on.
    Effect,
    /// Rotary Brake, RC `0x60 0x22`: rotate or stop.
    Brake,
    /// Horn Slow Speed, RC `0x60 0x23`.
    HornSlowSpeed,
    /// Horn Fast Speed, RC `0x60 0x24`.
    HornFastSpeed,
    /// Woofer Slow Speed, RC `0x60 0x25`.
    WooferSlowSpeed,
    /// Woofer Fast Speed, RC `0x60 0x26`.
    WooferFastSpeed,
    /// Horn Accelerate Time, RC `0x60 0x27`.
    HornAccelerateTime,
    /// Horn Decelerate Time, RC `0x60 0x28`.
    HornDecelerateTime,
    /// Woofer Accelerate Time, RC `0x60 0x29`.
    WooferAccelerateTime,
    /// Woofer Decelerate Time, RC `0x60 0x2A`.
    WooferDecelerateTime,
    /// Horn Level, RC `0x60 0x2B`.
    HornLevel,
    /// Woofer Level, RC `0x60 0x2C`.
    WooferLevel,
    /// Rotary Overdrive Amount, RC `0x60 0x2D`.
    Overdrive,
}

/// The bank of every Registered Controller of the Profile.
const BANK: U7 = U7::of(0x60);

impl RotaryControl {
    /// Every control, in order of index.
    pub const ALL: [Self; 14] = [
        Self::Speed,
        Self::Effect,
        Self::Brake,
        Self::HornSlowSpeed,
        Self::HornFastSpeed,
        Self::WooferSlowSpeed,
        Self::WooferFastSpeed,
        Self::HornAccelerateTime,
        Self::HornDecelerateTime,
        Self::WooferAccelerateTime,
        Self::WooferDecelerateTime,
        Self::HornLevel,
        Self::WooferLevel,
        Self::Overdrive,
    ];

    /// The Registered Controller.
    #[must_use]
    pub const fn controller(self) -> Rpn {
        Rpn::new(BANK, U7::of(0x20 + self as u8))
    }

    /// The control a Registered Controller sets, or `None`.
    #[must_use]
    pub fn of_controller(controller: Rpn) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.controller() == controller)
    }

    /// How its value is read.
    #[must_use]
    pub const fn value_kind(self) -> RotaryValue {
        match self {
            Self::Speed | Self::Effect | Self::Brake => RotaryValue::Switch,
            Self::HornLevel | Self::WooferLevel | Self::Overdrive => RotaryValue::Level,
            _ => RotaryValue::Speed,
        }
    }

    /// Whether a Device shall respond to it (§3.2.1): only Rotary Speed. The
    /// others it may (§3.2.2).
    #[must_use]
    pub const fn is_required(self) -> bool {
        matches!(self, Self::Speed)
    }

    /// The value Reset All Controllers sets it to (§3.2.3.1, Table 5, Table 6).
    #[must_use]
    pub const fn default_value(self) -> ControlValue {
        ControlValue::new(match self {
            Self::Speed | Self::Brake | Self::Overdrive => 0x0000_0000,
            Self::Effect => 0xFFFF_FFFF,
            Self::HornLevel | Self::WooferLevel => 0xC800_0000,
            _ => 0x8000_0000,
        })
    }
}

#[cfg(test)]
mod tests;
