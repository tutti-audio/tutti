//! The controllers of the Note On Orchestral Articulation Profile (M2-123 §8):
//! Orchestral Mute Type and Mute Amount, two Registered Controllers, and
//! Playing Position, a Registered Per-Note Controller.
//!
//! [`MuteType`] divides the 32-bit value into 32 subranges (§8.1.1, Table 19);
//! a subrange the table reserves is refused, never read as a default.
//! [`MuteState`] is a receiver's state: selecting a Mute Type sets the Mute
//! Amount to fully closed (§8.1.1), and the Amount alone changes it afterwards
//! (§8.1.2).

use crate::{ControlValue, NoteNumber, Rpn, U7, Voice2};

/// A Playing Position (M2-123 §8.2, Table 20): where on the playing surface a
/// note sounds, as the 32-bit value of the Registered Per-Note Controller
/// `0x0C`. Each instrument has its own normal position, at
/// [`NORMAL`](Self::NORMAL); the value runs from [`BRIDGE`](Self::BRIDGE) to
/// [`NUT`](Self::NUT).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayingPosition(u32);

impl PlayingPosition {
    /// `0x00000000`: at the bridge for a bowed or plucked instrument, at the
    /// center for drums and cymbals.
    pub const BRIDGE: Self = Self(0x0000_0000);
    /// `0x80000000`, the default: the normal playing position.
    pub const NORMAL: Self = Self(0x8000_0000);
    /// `0xFFFFFFFF`: at the nut for a bowed or plucked instrument, at the rim
    /// for drums and cymbals.
    pub const NUT: Self = Self(0xFFFF_FFFF);

    /// The Registered Per-Note Controller number, `0x0C` (§8.2).
    pub const CONTROLLER: u8 = 0x0C;

    /// A position from its 32-bit value; every value is one.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The 32-bit value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The Registered Per-Note Controller message that sets the position of
    /// `note`.
    #[must_use]
    pub const fn to_voice(self, note: NoteNumber) -> Voice2 {
        Voice2::RegisteredPerNote {
            note,
            controller: crate::RegisteredPerNoteController::PlayingPosition,
            value: ControlValue::new(self.0),
        }
    }

    /// The position a Registered Per-Note Controller message sets, or `None`
    /// for any other message.
    #[must_use]
    pub const fn from_voice(message: &Voice2) -> Option<(NoteNumber, Self)> {
        match message {
            Voice2::RegisteredPerNote {
                note,
                controller: crate::RegisteredPerNoteController::PlayingPosition,
                value,
            } => Some((*note, Self(value.get()))),
            _ => None,
        }
    }
}

impl Default for PlayingPosition {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// An Orchestral Mute Type: the subrange of the 32-bit value of Registered
/// Controller `0x20/0x22` (M2-123 §8.1.1, Table 19). Each subrange is 2^27
/// values wide and every value inside it means the same. The sixteen named
/// types are the first sixteen subranges; ten reserved subranges follow, and
/// the last six are the manufacturer's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MuteType {
    /// Subrange 0, `0x00000000..=0x07FFFFFF`: no mute, the default.
    #[default]
    NoMute,
    /// Subrange 1: straight mute (brass) or standard mute (strings and others).
    Straight,
    /// Subrange 2: practice mute.
    Practice,
    /// Subrange 3: cup mute.
    Cup,
    /// Subrange 4: wah-wah or Harmon mute, stem in.
    WahStemIn,
    /// Subrange 5: wah-wah or Harmon mute, stem extended.
    WahStemExtended,
    /// Subrange 6: wah-wah or Harmon mute, stem removed.
    WahStemRemoved,
    /// Subrange 7: plunger mute.
    Plunger,
    /// Subrange 8: bucket mute.
    Bucket,
    /// Subrange 9: mica mute.
    Mica,
    /// Subrange 10: solotone mute.
    Solotone,
    /// Subrange 11: whispa or whisper mute.
    Whispa,
    /// Subrange 12: hat.
    Hat,
    /// Subrange 13: hand.
    Hand,
    /// Subrange 14: stopped (transposing mute).
    Stopped,
    /// Subrange 15: into the stand.
    IntoTheStand,
    /// Subrange 26, `0xD0000000..=0xD7FFFFFF`: manufacturer specific mute 1.
    Manufacturer1,
    /// Subrange 27: manufacturer specific mute 2.
    Manufacturer2,
    /// Subrange 28: manufacturer specific mute 3.
    Manufacturer3,
    /// Subrange 29: manufacturer specific mute 4.
    Manufacturer4,
    /// Subrange 30: manufacturer specific mute 5.
    Manufacturer5,
    /// Subrange 31, `0xF8000000..=0xFFFFFFFF`: manufacturer specific mute 6.
    Manufacturer6,
}

/// A Mute Type value in one of the ten subranges Table 19 reserves,
/// `0x80000000..=0xCFFFFFFF`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("the mute type value {0:#010x} is in a reserved subrange")]
pub struct ReservedMuteType(pub u32);

impl MuteType {
    /// The Registered Controller address, bank `0x20` index `0x22` (§8.1.1).
    pub const RC: Rpn = Rpn::new(U7::of(0x20), U7::of(0x22));

    /// Every Mute Type, in the order of its subrange.
    pub const ALL: &'static [Self] = &[
        Self::NoMute,
        Self::Straight,
        Self::Practice,
        Self::Cup,
        Self::WahStemIn,
        Self::WahStemExtended,
        Self::WahStemRemoved,
        Self::Plunger,
        Self::Bucket,
        Self::Mica,
        Self::Solotone,
        Self::Whispa,
        Self::Hat,
        Self::Hand,
        Self::Stopped,
        Self::IntoTheStand,
        Self::Manufacturer1,
        Self::Manufacturer2,
        Self::Manufacturer3,
        Self::Manufacturer4,
        Self::Manufacturer5,
        Self::Manufacturer6,
    ];

    /// The number of the subrange, 0 to 31.
    #[must_use]
    pub const fn subrange(self) -> u8 {
        match self {
            Self::NoMute => 0,
            Self::Straight => 1,
            Self::Practice => 2,
            Self::Cup => 3,
            Self::WahStemIn => 4,
            Self::WahStemExtended => 5,
            Self::WahStemRemoved => 6,
            Self::Plunger => 7,
            Self::Bucket => 8,
            Self::Mica => 9,
            Self::Solotone => 10,
            Self::Whispa => 11,
            Self::Hat => 12,
            Self::Hand => 13,
            Self::Stopped => 14,
            Self::IntoTheStand => 15,
            Self::Manufacturer1 => 26,
            Self::Manufacturer2 => 27,
            Self::Manufacturer3 => 28,
            Self::Manufacturer4 => 29,
            Self::Manufacturer5 => 30,
            Self::Manufacturer6 => 31,
        }
    }

    /// The Mute Type of a 32-bit value.
    ///
    /// # Errors
    ///
    /// [`ReservedMuteType`] for a value in a reserved subrange.
    pub const fn from_value(value: u32) -> Result<Self, ReservedMuteType> {
        Ok(match value >> 27 {
            0 => Self::NoMute,
            1 => Self::Straight,
            2 => Self::Practice,
            3 => Self::Cup,
            4 => Self::WahStemIn,
            5 => Self::WahStemExtended,
            6 => Self::WahStemRemoved,
            7 => Self::Plunger,
            8 => Self::Bucket,
            9 => Self::Mica,
            10 => Self::Solotone,
            11 => Self::Whispa,
            12 => Self::Hat,
            13 => Self::Hand,
            14 => Self::Stopped,
            15 => Self::IntoTheStand,
            26 => Self::Manufacturer1,
            27 => Self::Manufacturer2,
            28 => Self::Manufacturer3,
            29 => Self::Manufacturer4,
            30 => Self::Manufacturer5,
            31 => Self::Manufacturer6,
            _ => return Err(ReservedMuteType(value)),
        })
    }

    /// The first value of the subrange: what a sender writes, since every value
    /// of a subrange means the same.
    #[must_use]
    pub const fn value(self) -> u32 {
        (self.subrange() as u32) << 27
    }
}

/// An Orchestral Mute Amount: the value of Registered Controller `0x20/0x23`,
/// from fully open to fully closed (M2-123 §8.1.2, Figure 4). A mute with one position of use is [`CLOSED`](Self::CLOSED).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MuteAmount(u32);

impl MuteAmount {
    /// The Registered Controller address, bank `0x20` index `0x23` (§8.1.2).
    pub const RC: Rpn = Rpn::new(U7::of(0x20), U7::of(0x23));
    /// `0x00000000`: fully open, off.
    pub const OPEN: Self = Self(0x0000_0000);
    /// `0xFFFFFFFF`: fully closed, muted or dampened. The default.
    pub const CLOSED: Self = Self(0xFFFF_FFFF);

    /// An amount from its 32-bit value; every value is one.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The 32-bit value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for MuteAmount {
    fn default() -> Self {
        Self::CLOSED
    }
}

/// A Registered Controller message of the mute controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MuteMessage {
    /// Registered Controller `0x20/0x22`.
    Type(MuteType),
    /// Registered Controller `0x20/0x23`.
    Amount(MuteAmount),
}

impl MuteMessage {
    /// The Registered Controller message.
    #[must_use]
    pub const fn to_voice(self) -> Voice2 {
        let (rc, value) = match self {
            Self::Type(kind) => (MuteType::RC, kind.value()),
            Self::Amount(amount) => (MuteAmount::RC, amount.get()),
        };
        Voice2::registered(rc, ControlValue::new(value))
    }

    /// The mute message a Registered Controller carries: `None` for any other
    /// message, and the error for a Mute Type in a reserved subrange.
    ///
    /// # Errors
    ///
    /// [`ReservedMuteType`] when the message is a Mute Type whose value is in a
    /// reserved subrange.
    pub const fn from_voice(message: &Voice2) -> Option<Result<Self, ReservedMuteType>> {
        let Voice2::Registered { bank, index, value } = message else {
            return None;
        };
        if bank.get() != 0x20 {
            return None;
        }
        match index.get() {
            0x22 => Some(match MuteType::from_value(value.get()) {
                Ok(kind) => Ok(Self::Type(kind)),
                Err(error) => Err(error),
            }),
            0x23 => Some(Ok(Self::Amount(MuteAmount(value.get())))),
            _ => None,
        }
    }
}

/// A receiver's mute state: the type and the amount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MuteState {
    /// The Mute Type, [`MuteType::NoMute`] by default.
    pub mute_type: MuteType,
    /// The Mute Amount, [`MuteAmount::CLOSED`] by default.
    pub amount: MuteAmount,
}

impl MuteState {
    /// The state after `message`: a Mute Type sets the Amount to
    /// [`MuteAmount::CLOSED`] (M2-123 §8.1.1), and an Amount sets only itself.
    #[must_use]
    pub const fn apply(self, message: MuteMessage) -> Self {
        match message {
            MuteMessage::Type(mute_type) => Self {
                mute_type,
                amount: MuteAmount::CLOSED,
            },
            MuteMessage::Amount(amount) => Self { amount, ..self },
        }
    }
}
