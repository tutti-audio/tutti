//! The Default Control Change Mapping Profile (M2-113) as suggested mappings.
//!
//! While the Profile is enabled on a channel, each Control Change number has
//! the function the specification's Appendix A assigns (M2-113 §2.1, §2.3).
//! [`SuggestedCc::of`] is that table as data: the function, how the value is
//! read, whether the number is the MSB or the LSB of a 14-bit pair, and the
//! value Reset All Controllers sets (§2.4). They are suggestions an
//! application may bind to its controls, and route nothing.
//! [`DefaultCcLevel::enabled_on`] says whether a peer has the Profile enabled
//! on a channel.

use tutti_ump::{CCNumber, MidiChannel, U7};

use super::{CiDeviceId, ProfileId, ProfileState, ProfileTable, StandardProfile};

/// How completely a device implements the Profile (M2-113 §2.2, the Profile
/// Level byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DefaultCcLevel {
    /// Level `0x00`: some implementation, not to the minimum requirements.
    Partial,
    /// Level `0x01`: meets the minimum requirements.
    MeetsMinimum,
}

impl DefaultCcLevel {
    /// The Profile Id of the Profile at this level (M2-113 §2.2): Bank `0x21`,
    /// Number `0x00`, Version `0x01`.
    #[must_use]
    pub const fn profile(self) -> ProfileId {
        let level = match self {
            Self::Partial => 0x00,
            Self::MeetsMinimum => 0x01,
        };
        ProfileId::Standard(StandardProfile::new(
            U7::of(0x21),
            U7::of(0x00),
            U7::of(0x01),
            U7::of(level),
        ))
    }

    /// The level of the Default Control Change Mapping Profile enabled on
    /// `channel` in `table`, or `None` when neither level is. The Profile
    /// works per channel (M2-113 §2.2.1), so only a channel address counts.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable, channel: MidiChannel) -> Option<Self> {
        [Self::MeetsMinimum, Self::Partial]
            .into_iter()
            .find(|level| {
                table
                    .get(CiDeviceId::Channel(channel), level.profile())
                    .is_some_and(|entry| entry.state() == ProfileState::Enabled)
            })
    }
}

/// What a Control Change number does under the Profile (M2-113 Appendix A).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcFunction {
    /// Bank Select.
    BankSelect,
    /// Modulation Wheel or Lever.
    ModWheel,
    /// Breath Controller.
    Breath,
    /// Foot Controller.
    Foot,
    /// Portamento Time.
    PortamentoTime,
    /// Data Entry.
    DataEntry,
    /// Channel Volume.
    Volume,
    /// Balance.
    Balance,
    /// Pan.
    Pan,
    /// Expression Controller.
    Expression,
    /// Effect Control 1.
    EffectControl1,
    /// Effect Control 2.
    EffectControl2,
    /// General Purpose Controller 1.
    GeneralPurpose1,
    /// General Purpose Controller 2.
    GeneralPurpose2,
    /// General Purpose Controller 3.
    GeneralPurpose3,
    /// General Purpose Controller 4.
    GeneralPurpose4,
    /// General Purpose Controller 5.
    GeneralPurpose5,
    /// General Purpose Controller 6.
    GeneralPurpose6,
    /// General Purpose Controller 7.
    GeneralPurpose7,
    /// General Purpose Controller 8.
    GeneralPurpose8,
    /// Damper Pedal (Sustain).
    Sustain,
    /// Portamento On/Off.
    PortamentoSwitch,
    /// Sostenuto.
    Sostenuto,
    /// Soft Pedal.
    SoftPedal,
    /// Legato Footswitch.
    Legato,
    /// Hold 2.
    Hold2,
    /// Sound Controller 1: Sound Variation.
    SoundVariation,
    /// Sound Controller 2: Timbre or Harmonic Intensity (`CCNumber::RESONANCE`).
    Resonance,
    /// Sound Controller 3: Release Time.
    ReleaseTime,
    /// Sound Controller 4: Attack Time.
    AttackTime,
    /// Sound Controller 5: Brightness.
    Brightness,
    /// Sound Controller 6: Decay Time.
    DecayTime,
    /// Sound Controller 7: Vibrato Rate.
    VibratoRate,
    /// Sound Controller 8: Vibrato Depth.
    VibratoDepth,
    /// Sound Controller 9: Vibrato Delay.
    VibratoDelay,
    /// Sound Controller 10: no default function.
    SoundController10,
    /// Portamento Control.
    PortamentoControl,
    /// High Resolution Velocity Prefix.
    VelocityPrefix,
    /// Effects 1 Depth: Reverb Send Level.
    ReverbSend,
    /// Effects 2 Depth.
    Effects2Depth,
    /// Effects 3 Depth: Chorus Send Level.
    ChorusSend,
    /// Effects 4 Depth.
    Effects4Depth,
    /// Effects 5 Depth.
    Effects5Depth,
    /// Data Increment.
    DataIncrement,
    /// Data Decrement.
    DataDecrement,
    /// The Non-Registered Parameter Number, whose byte the number is.
    NrpnSelect,
    /// The Registered Parameter Number, whose byte the number is.
    RpnSelect,
    /// Channel Mode: All Sound Off.
    AllSoundOff,
    /// Channel Mode: Reset All Controllers.
    ResetAll,
    /// Channel Mode: Local Control On/Off.
    LocalControl,
    /// Channel Mode: All Notes Off.
    AllNotesOff,
    /// Channel Mode: Omni Mode Off.
    OmniModeOff,
    /// Channel Mode: Omni Mode On.
    OmniModeOn,
    /// Channel Mode: Mono Mode On.
    MonoModeOn,
    /// Channel Mode: Poly Mode On.
    PolyModeOn,
    /// The Profile assigns no function; the device decides.
    Undefined,
}

/// How the value of a Control Change is read (M2-113 Appendix A, the Value
/// column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcValue {
    /// `0..=127`.
    Range,
    /// A switch: up to 63 is off, 64 and above is on.
    Switch,
    /// Up to 63 is normal, 64 and above is legato.
    LegatoSwitch,
    /// The value is not used: a Data Increment or Decrement is the message.
    Unused,
    /// The value is 0.
    Zero,
    /// 0 is off, 127 is on.
    OnOff127,
    /// The number of channels, or zero when it equals the number of voices.
    ChannelCount,
    /// The Profile defines no value.
    Undefined,
}

/// The part of a 14-bit value a Control Change number carries (M2-113
/// Appendix A, the Value Used As column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcPart {
    /// The most significant 7 bits.
    Msb,
    /// The least significant 7 bits.
    Lsb,
    /// Neither: the number is a whole value.
    Whole,
}

/// What Reset All Controllers sets the number to (M2-113 §2.4, Appendix A).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CcReset {
    /// The value.
    To(U7),
    /// The number is not set.
    Keep,
    /// The value is the device's own.
    DeviceSpecific,
}

/// What the Profile assigns to one Control Change number (M2-113 Appendix A).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SuggestedCc {
    controller: CCNumber,
    function: CcFunction,
    value: CcValue,
    part: CcPart,
    reset: CcReset,
}

/// The 128 rows, in order of number.
const TABLE: [SuggestedCc; 128] = table();

impl SuggestedCc {
    /// The assignment for `controller`.
    #[must_use]
    pub const fn of(controller: CCNumber) -> Self {
        TABLE[controller.get() as usize]
    }

    /// Every number's assignment, in order of number.
    pub fn all() -> impl Iterator<Item = Self> {
        TABLE.into_iter()
    }

    /// The Control Change number.
    #[must_use]
    pub const fn controller(self) -> CCNumber {
        self.controller
    }

    /// What it does.
    #[must_use]
    pub const fn function(self) -> CcFunction {
        self.function
    }

    /// How its value is read.
    #[must_use]
    pub const fn value(self) -> CcValue {
        self.value
    }

    /// The part of a 14-bit value it carries.
    #[must_use]
    pub const fn part(self) -> CcPart {
        self.part
    }

    /// What Reset All Controllers sets it to.
    #[must_use]
    pub const fn reset(self) -> CcReset {
        self.reset
    }
}

/// The table: numbers 0 to 63 are a fine control's MSB and LSB, whichever of
/// the two [`CCNumber::lsb`] says, and 64 to 127 are rows of their own.
const fn table() -> [SuggestedCc; 128] {
    let mut rows = [single_row(0); 128];
    let mut n = 0;
    while n < 128 {
        let controller = CCNumber::of(n);
        let mut row = if n < 32 {
            msb_row(n)
        } else if n < 64 {
            msb_row(n - 32)
        } else {
            single_row(n)
        };
        if n < 64 {
            row.part = if controller.lsb().is_some() {
                CcPart::Msb
            } else {
                CcPart::Lsb
            };
        }
        row.controller = controller;
        rows[n as usize] = row;
        n += 1;
    }
    rows
}

const fn row(function: CcFunction, value: CcValue, part: CcPart, reset: CcReset) -> SuggestedCc {
    SuggestedCc {
        controller: CCNumber::FIRST,
        function,
        value,
        part,
        reset,
    }
}

const fn to(value: u8) -> CcReset {
    CcReset::To(U7::of(value))
}

/// The rows of numbers 0 to 31, each the MSB of a 14-bit pair whose LSB is
/// 32 above it.
const fn msb_row(n: u8) -> SuggestedCc {
    use CcFunction as F;
    let (function, reset) = match n {
        0 => (F::BankSelect, CcReset::Keep),
        1 => (F::ModWheel, to(0)),
        2 => (F::Breath, to(0)),
        4 => (F::Foot, to(0)),
        5 => (F::PortamentoTime, to(0)),
        6 => (F::DataEntry, CcReset::Keep),
        7 => (F::Volume, CcReset::Keep),
        8 => (F::Balance, to(64)),
        10 => (F::Pan, CcReset::Keep),
        11 => (F::Expression, to(127)),
        12 => (F::EffectControl1, to(0)),
        13 => (F::EffectControl2, to(0)),
        16 => (F::GeneralPurpose1, CcReset::DeviceSpecific),
        17 => (F::GeneralPurpose2, CcReset::DeviceSpecific),
        18 => (F::GeneralPurpose3, CcReset::DeviceSpecific),
        19 => (F::GeneralPurpose4, CcReset::DeviceSpecific),
        _ => (F::Undefined, CcReset::DeviceSpecific),
    };
    row(function, CcValue::Range, CcPart::Msb, reset)
}

/// The rows of numbers 64 to 127, each a value of its own.
const fn single_row(n: u8) -> SuggestedCc {
    use CcFunction as F;
    use CcPart::{Lsb, Msb, Whole};
    use CcValue as V;
    match n {
        64 => row(F::Sustain, V::Switch, Whole, to(0)),
        65 => row(F::PortamentoSwitch, V::Switch, Whole, to(0)),
        66 => row(F::Sostenuto, V::Switch, Whole, to(0)),
        67 => row(F::SoftPedal, V::Switch, Whole, to(0)),
        68 => row(F::Legato, V::LegatoSwitch, Whole, to(0)),
        69 => row(F::Hold2, V::Switch, Whole, to(0)),
        70 => row(F::SoundVariation, V::Range, Lsb, CcReset::Keep),
        71 => row(F::Resonance, V::Range, Lsb, CcReset::Keep),
        72 => row(F::ReleaseTime, V::Range, Lsb, CcReset::Keep),
        73 => row(F::AttackTime, V::Range, Lsb, CcReset::Keep),
        74 => row(F::Brightness, V::Range, Lsb, CcReset::Keep),
        75 => row(F::DecayTime, V::Range, Lsb, CcReset::Keep),
        76 => row(F::VibratoRate, V::Range, Lsb, CcReset::Keep),
        77 => row(F::VibratoDepth, V::Range, Lsb, CcReset::Keep),
        78 => row(F::VibratoDelay, V::Range, Lsb, CcReset::Keep),
        79 => row(F::SoundController10, V::Range, Lsb, CcReset::Keep),
        80 => row(F::GeneralPurpose5, V::Range, Lsb, CcReset::DeviceSpecific),
        81 => row(F::GeneralPurpose6, V::Range, Lsb, CcReset::DeviceSpecific),
        82 => row(F::GeneralPurpose7, V::Range, Lsb, CcReset::DeviceSpecific),
        83 => row(F::GeneralPurpose8, V::Range, Lsb, CcReset::DeviceSpecific),
        84 => row(F::PortamentoControl, V::Range, Lsb, CcReset::Keep),
        88 => row(F::VelocityPrefix, V::Range, Lsb, CcReset::Keep),
        91 => row(F::ReverbSend, V::Range, Whole, CcReset::Keep),
        92 => row(F::Effects2Depth, V::Range, Whole, CcReset::Keep),
        93 => row(F::ChorusSend, V::Range, Whole, CcReset::Keep),
        94 => row(F::Effects4Depth, V::Range, Whole, CcReset::Keep),
        95 => row(F::Effects5Depth, V::Range, Whole, CcReset::Keep),
        96 => row(F::DataIncrement, V::Unused, Whole, CcReset::Keep),
        97 => row(F::DataDecrement, V::Unused, Whole, CcReset::Keep),
        98 => row(F::NrpnSelect, V::Range, Lsb, to(127)),
        99 => row(F::NrpnSelect, V::Range, Msb, to(127)),
        100 => row(F::RpnSelect, V::Range, Lsb, to(127)),
        101 => row(F::RpnSelect, V::Range, Msb, to(127)),
        120 => row(F::AllSoundOff, V::Zero, Whole, CcReset::Keep),
        121 => row(F::ResetAll, V::Zero, Whole, CcReset::Keep),
        122 => row(F::LocalControl, V::OnOff127, Whole, CcReset::Keep),
        123 => row(F::AllNotesOff, V::Zero, Whole, CcReset::Keep),
        124 => row(F::OmniModeOff, V::Zero, Whole, CcReset::Keep),
        125 => row(F::OmniModeOn, V::Zero, Whole, CcReset::Keep),
        126 => row(F::MonoModeOn, V::ChannelCount, Whole, CcReset::Keep),
        127 => row(F::PolyModeOn, V::Zero, Whole, CcReset::Keep),
        _ => row(F::Undefined, V::Undefined, Whole, CcReset::DeviceSpecific),
    }
}

#[cfg(test)]
mod tests;
