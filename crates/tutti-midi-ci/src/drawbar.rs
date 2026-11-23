//! The Drawbar Organ Profile (M2-121) as typed controls.
//!
//! While the Profile is enabled on a channel, nine Registered Controllers set
//! the organ's drawbars (§5.1.2, Table 6 and Table 7) and a few optional ones
//! its vibrato/chorus, percussion and key click (§6.1.2, Table 10).
//! [`Drawbar`] is the nine, [`DrawbarSetting`] the nine positions a drawbar
//! has and the 32-bit range each covers, [`OrganOption`] the optional
//! controllers, and [`OrganFeatures`] the Reply to a Profile Details Inquiry
//! (§4.2). The data routes nothing; an application binds it.
//! [`OrganLevel::enabled_on`] says whether a peer has the Profile enabled on a
//! channel.

use core::ops::RangeInclusive;

use tutti_ump::{ControlValue, MidiChannel, Rpn, U7};

use super::{
    CiDeviceId, InquiryTarget, ProfileId, ProfileState, ProfileTable, StandardProfile,
    SwitchMeaning,
};

/// How completely a device implements the Profile (M2-121 §4.1, Table 4, the
/// Profile Level byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrganLevel {
    /// Level `0x00`: some implementation, not to the minimum requirements.
    Partial,
    /// Level `0x01`: meets the minimum requirements.
    MeetsMinimum,
    /// Level `0x02`: implements some extended or optional features.
    Extended,
}

impl OrganLevel {
    /// The Profile Id of the Profile at this level (M2-121 Table 4): Bank
    /// `0x20`, Number `0x01`, Version `0x01`.
    #[must_use]
    pub const fn profile(self) -> ProfileId {
        let level = match self {
            Self::Partial => 0x00,
            Self::MeetsMinimum => 0x01,
            Self::Extended => 0x02,
        };
        ProfileId::Standard(StandardProfile::new(
            U7::of(0x20),
            U7::of(0x01),
            U7::of(0x01),
            U7::of(level),
        ))
    }

    /// The level `profile` is the Id of, or `None` for any other Profile.
    #[must_use]
    pub fn of_profile(profile: ProfileId) -> Option<Self> {
        [Self::Extended, Self::MeetsMinimum, Self::Partial]
            .into_iter()
            .find(|level| level.profile() == profile)
    }

    /// The level of the Profile enabled on `channel` in `table`, or `None`
    /// when no level is. The Profile works on a Channel (M2-121 §4), so only a
    /// channel address counts.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable, channel: MidiChannel) -> Option<Self> {
        [Self::Extended, Self::MeetsMinimum, Self::Partial]
            .into_iter()
            .find(|level| {
                table
                    .get(CiDeviceId::Channel(channel), level.profile())
                    .is_some_and(|entry| entry.state() == ProfileState::Enabled)
            })
    }
}

/// The lowest Note Number the organ sounds (M2-121 §5.1.1): 36.
pub const ORGAN_LOWEST_NOTE: U7 = U7::of(36);
/// The highest Note Number the organ sounds (M2-121 §5.1.1): 96.
pub const ORGAN_HIGHEST_NOTE: U7 = U7::of(96);
/// The Note Number that sounds A=440 Hz on the 8' drawbar by default (M2-121
/// §5.1.1): 69.
pub const ORGAN_REFERENCE_NOTE: U7 = U7::of(69);

/// One of the nine drawbars of a voice (M2-121 §3.1, §5.1.2, Table 7), named
/// for its footage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Drawbar {
    /// 16', RC `0x40 0x30`.
    Feet16,
    /// 5-1/3', RC `0x40 0x31`.
    Feet5And1Third,
    /// 8', RC `0x40 0x32`.
    Feet8,
    /// 4', RC `0x40 0x33`.
    Feet4,
    /// 2-2/3', RC `0x40 0x34`.
    Feet2And2Thirds,
    /// 2', RC `0x40 0x35`.
    Feet2,
    /// 1-3/5', RC `0x40 0x36`.
    Feet1And3Fifths,
    /// 1-1/3', RC `0x40 0x37`.
    Feet1And1Third,
    /// 1', RC `0x40 0x38`.
    Feet1,
}

/// The bank of every Registered Controller of the Profile (M2-121 Table 7,
/// Table 10).
const BANK: U7 = U7::of(0x40);
/// The index of the first drawbar.
const FIRST_DRAWBAR: u8 = 0x30;

impl Drawbar {
    /// Every drawbar, in order of footage from the lowest partial.
    pub const ALL: [Self; 9] = [
        Self::Feet16,
        Self::Feet5And1Third,
        Self::Feet8,
        Self::Feet4,
        Self::Feet2And2Thirds,
        Self::Feet2,
        Self::Feet1And3Fifths,
        Self::Feet1And1Third,
        Self::Feet1,
    ];

    /// The Registered Controller that sets the drawbar (Table 7).
    #[must_use]
    pub const fn controller(self) -> Rpn {
        Rpn::new(BANK, U7::of(FIRST_DRAWBAR + self as u8))
    }

    /// The drawbar a Registered Controller sets, or `None`.
    #[must_use]
    pub fn of_controller(controller: Rpn) -> Option<Self> {
        let index = controller.index().get();
        (controller.bank() == BANK)
            .then(|| index.checked_sub(FIRST_DRAWBAR))
            .flatten()
            .and_then(|i| Self::ALL.get(usize::from(i)).copied())
    }
}

tutti_units::ranged! {
    /// A drawbar position, `0..=8`: 0 is Off or In, 8 is Full or Out (M2-121
    /// §5.1.2, Table 6).
    #[derive(Eq, Hash)]
    DrawbarSetting(u8), NotADrawbarSetting, "a drawbar setting (0 to 8)",
    |raw| raw <= 8
}

/// Table 6: the first value each setting covers and its discrete value.
const SETTINGS: [(u32, u32); 9] = [
    (0x0000_0000, 0x0E38_E38E),
    (0x1C71_C71C, 0x2AAA_AAAB),
    (0x38E3_8E38, 0x471C_71C7),
    (0x5555_5555, 0x638E_38E4),
    (0x71C7_1C71, 0x8000_0000),
    (0x8E38_E38D, 0x9C71_C71D),
    (0xAAAA_AAAA, 0xB8E3_8E39),
    (0xC71C_71C6, 0xD555_5556),
    (0xE38E_38E2, 0xF1C7_1C72),
];

impl DrawbarSetting {
    /// Off or In.
    pub const OFF: Self = Self::of(0);
    /// Full or Out.
    pub const FULL: Self = Self::of(8);

    /// The setting a received value falls in. A receiver supports the full
    /// range (§5.1.2): every value is in one of the nine.
    #[must_use]
    pub fn of_value(value: ControlValue) -> Self {
        // The first row starts at 0, so at least one row starts at or below
        // any value, and at most nine do: the setting is `0..=8`.
        let started = SETTINGS.partition_point(|&(first, _)| first <= value.get());
        match u8::try_from(started - 1) {
            Ok(setting) if setting <= 8 => Self::of(setting),
            _ => unreachable!("the nine rows of Table 6 cover every value"),
        }
    }

    /// The values the setting covers.
    #[must_use]
    pub fn range(self) -> RangeInclusive<ControlValue> {
        let first = SETTINGS[usize::from(self.get())].0;
        let last = SETTINGS
            .get(usize::from(self.get()) + 1)
            .map_or(u32::MAX, |&(next, _)| next - 1);
        ControlValue::new(first)..=ControlValue::new(last)
    }

    /// The value a sender gives the setting when it switches to it.
    #[must_use]
    pub fn discrete_value(self) -> ControlValue {
        ControlValue::new(SETTINGS[usize::from(self.get())].1)
    }
}

/// The names of the six vibrato/chorus types (M2-121 §6.1.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VibratoChorus {
    /// V1.
    V1,
    /// C1.
    C1,
    /// V2.
    V2,
    /// C2.
    C2,
    /// V3.
    V3,
    /// C3.
    C3,
}

/// §6.1.2.1: the first value each type covers.
const VIBRATO_CHORUS: [(u32, VibratoChorus); 6] = [
    (0x0000_0000, VibratoChorus::V1),
    (0x2AAA_AAAA, VibratoChorus::C1),
    (0x5555_5555, VibratoChorus::V2),
    (0x8000_0000, VibratoChorus::C2),
    (0xAAAA_AAAA, VibratoChorus::V3),
    (0xD555_5555, VibratoChorus::C3),
];

impl VibratoChorus {
    /// The type a received value falls in.
    #[must_use]
    pub fn of_value(value: ControlValue) -> Self {
        // The first row starts at 0, so at least one row starts at or below
        // any value and `at` is at least 1.
        let at = VIBRATO_CHORUS.partition_point(|&(first, _)| first <= value.get());
        VIBRATO_CHORUS[at - 1].1
    }

    /// The values the type covers.
    #[must_use]
    pub fn range(self) -> RangeInclusive<ControlValue> {
        let at = VIBRATO_CHORUS
            .iter()
            .position(|&(_, t)| t == self)
            .unwrap_or(0);
        let last = VIBRATO_CHORUS
            .get(at + 1)
            .map_or(u32::MAX, |&(next, _)| next - 1);
        ControlValue::new(VIBRATO_CHORUS[at].0)..=ControlValue::new(last)
    }
}

/// How an optional controller's value is read (M2-121 §6.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrganValue {
    /// One of the six [`VibratoChorus`] types.
    VibratoChorus,
    /// A [`TwoState`](super::TwoState), meaning [`OrganOption::switch_meaning`].
    Switch,
    /// `0x00000000` to `0xFFFFFFFF`, none to full.
    Amount,
}

/// An optional Registered Controller of the Profile (M2-121 §6.1.2, Table 10).
/// RC `0x40 0x3F` is reserved and has no variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OrganOption {
    /// Vibrato / Chorus Type, RC `0x40 0x39`.
    VibratoChorusType,
    /// Vibrato / Chorus Off / On, RC `0x40 0x3A`.
    VibratoChorusOn,
    /// Percussion Off / On, RC `0x40 0x3B`.
    PercussionOn,
    /// Percussion Normal / Soft, RC `0x40 0x3C`.
    PercussionSoft,
    /// Percussion Slow / Fast, RC `0x40 0x3D`.
    PercussionFast,
    /// Percussion 2nd / 3rd Harmonic, RC `0x40 0x3E`.
    PercussionThird,
    /// Amount of Key Click, RC `0x40 0x40`.
    KeyClick,
    /// Amount of Crosstalk / Leakage, RC `0x40 0x41`.
    Crosstalk,
}

impl OrganOption {
    /// Every optional controller, in order of index.
    pub const ALL: [Self; 8] = [
        Self::VibratoChorusType,
        Self::VibratoChorusOn,
        Self::PercussionOn,
        Self::PercussionSoft,
        Self::PercussionFast,
        Self::PercussionThird,
        Self::KeyClick,
        Self::Crosstalk,
    ];

    /// The Registered Controller (Table 10).
    #[must_use]
    pub const fn controller(self) -> Rpn {
        let index = match self {
            Self::VibratoChorusType => 0x39,
            Self::VibratoChorusOn => 0x3A,
            Self::PercussionOn => 0x3B,
            Self::PercussionSoft => 0x3C,
            Self::PercussionFast => 0x3D,
            Self::PercussionThird => 0x3E,
            Self::KeyClick => 0x40,
            Self::Crosstalk => 0x41,
        };
        Rpn::new(BANK, U7::of(index))
    }

    /// The option a Registered Controller sets, or `None`.
    #[must_use]
    pub fn of_controller(controller: Rpn) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.controller() == controller)
    }

    /// How its value is read.
    #[must_use]
    pub const fn value_kind(self) -> OrganValue {
        match self {
            Self::VibratoChorusType => OrganValue::VibratoChorus,
            Self::KeyClick | Self::Crosstalk => OrganValue::Amount,
            _ => OrganValue::Switch,
        }
    }

    /// What the two states mean, for the switches (`None` for the others).
    #[must_use]
    pub const fn switch_meaning(self) -> Option<SwitchMeaning> {
        match self {
            Self::VibratoChorusOn | Self::PercussionOn => Some(SwitchMeaning::OffOn),
            Self::PercussionSoft => Some(SwitchMeaning::NormalSoft),
            Self::PercussionFast => Some(SwitchMeaning::SlowFast),
            Self::PercussionThird => Some(SwitchMeaning::SecondThirdHarmonic),
            Self::VibratoChorusType | Self::KeyClick | Self::Crosstalk => None,
        }
    }

    /// The default value, for the options that state one: Vibrato/Chorus
    /// Off, and `0x80000000` for the two amounts.
    #[must_use]
    pub const fn default_value(self) -> Option<ControlValue> {
        match self {
            Self::VibratoChorusOn => Some(ControlValue::new(0)),
            Self::KeyClick | Self::Crosstalk => Some(ControlValue::new(0x8000_0000)),
            _ => None,
        }
    }
}

/// The optional features a receiver declares in its Reply to a Profile
/// Details Inquiry for the Optional Features target (M2-121 §4.2, Table 5): the
/// one-byte Inquiry Target Data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OrganFeatures {
    /// Soft Pedal, CC#67, the Volume Normal/Soft tab (`D0`).
    pub soft_pedal: bool,
    /// Both vibrato/chorus controllers (`D1`).
    pub vibrato_chorus: bool,
    /// All four percussion controllers (`D2`).
    pub percussion: bool,
    /// Amount of Key Click (`D3`).
    pub key_click: bool,
    /// Amount of Crosstalk/Leakage (`D4`).
    pub crosstalk: bool,
}

impl OrganFeatures {
    /// The Inquiry Target of the Optional Features Details message (§4.2.1).
    pub const INQUIRY_TARGET: InquiryTarget = InquiryTarget::new(U7::of(0x01));

    /// The Inquiry Target Data. The reserved bits `D5` to `D7` are zero.
    #[must_use]
    pub const fn to_data(self) -> [u8; 1] {
        [self.soft_pedal as u8
            | (self.vibrato_chorus as u8) << 1
            | (self.percussion as u8) << 2
            | (self.key_click as u8) << 3
            | (self.crosstalk as u8) << 4]
    }

    /// Reads the Inquiry Target Data, or `None` for anything but one byte
    /// with the reserved bits clear.
    #[must_use]
    pub const fn from_data(data: &[u8]) -> Option<Self> {
        let [byte] = *data else {
            return None;
        };
        if byte > 0b1_1111 {
            return None;
        }
        Some(Self {
            soft_pedal: byte & 1 == 1,
            vibrato_chorus: byte >> 1 & 1 == 1,
            percussion: byte >> 2 & 1 == 1,
            key_click: byte >> 3 & 1 == 1,
            crosstalk: byte >> 4 & 1 == 1,
        })
    }
}

#[cfg(test)]
mod tests;
