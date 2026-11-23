//! The Default Drum Note Map Profile (M2-125) as a typed table.
//!
//! While the Profile is enabled on a channel, each Note Number from 27 to 88
//! plays the drum sound M2-125 §4.1, Table 5 assigns it. [`DrumInstrument`] is
//! that table: the sound, its Mutually Exclusive Set, the recommended pan, and
//! the bit that declares it in the Profile Details Reply. [`DrumFeatures`] is the
//! reply's Inquiry Target Data (§7.1.3), the optional features and the Note
//! Numbers with a sound available. [`DrumControl`] names the controllers the
//! Profile responds to per note (§6.1) and per channel (§5). The data routes
//! nothing; an application binds it. [`DrumNoteMapLevel::enabled_on`] says
//! whether a peer has the Profile enabled on a channel.

use tutti_ump::{CCNumber, MidiChannel, RegisteredPerNoteController, U7};
use tutti_units::Pan;

use super::{CiDeviceId, InquiryTarget, ProfileId, ProfileState, ProfileTable, StandardProfile};

/// How completely a device implements the Profile (M2-125 §3.1, Table 4, the
/// Profile Level byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DrumNoteMapLevel {
    /// Level `0x00`: some implementation, not to the minimum requirements.
    Partial,
    /// Level `0x01`: meets the minimum requirements.
    MeetsMinimum,
}

impl DrumNoteMapLevel {
    /// The Profile Id of the Profile at this level (M2-125 Table 4): Bank
    /// `0x20`, Number `0x03`, Version `0x01`.
    #[must_use]
    pub const fn profile(self) -> ProfileId {
        let level = match self {
            Self::Partial => 0x00,
            Self::MeetsMinimum => 0x01,
        };
        ProfileId::Standard(StandardProfile::new(
            U7::of(0x20),
            U7::of(0x03),
            U7::of(0x01),
            U7::of(level),
        ))
    }

    /// The level `profile` is the Id of, or `None` for any other Profile.
    #[must_use]
    pub fn of_profile(profile: ProfileId) -> Option<Self> {
        [Self::MeetsMinimum, Self::Partial]
            .into_iter()
            .find(|level| level.profile() == profile)
    }

    /// The level of the Profile enabled on `channel` in `table`, or `None`
    /// when neither level is. The Profile works on a Channel (M2-125 §3), so
    /// only a channel address counts.
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

tutti_units::ranged! {
    /// A Mutually Exclusive Set (M2-125 §4.1), numbered `1..=7` as Table 5
    /// numbers them: when any sound of a set starts, the other sounds of the
    /// same set are muted.
    #[derive(Eq, PartialOrd, Ord, Hash)]
    MutuallyExclusiveSet(u8), NotAMutuallyExclusiveSet, "a Mutually Exclusive Set (1 to 7)",
    |raw| raw >= 1 && raw <= 7
}

impl MutuallyExclusiveSet {
    /// The sounds of the set, in order of Note Number.
    pub fn members(self) -> impl Iterator<Item = DrumInstrument> {
        DrumInstrument::all().filter(move |i| i.mutually_exclusive_set() == Some(self))
    }
}

/// Defines the instruments of Table 5 in order of Note Number: the variant,
/// the name, the Mutually Exclusive Set (0 for none) and the recommended pan
/// (percent, negative to the left).
macro_rules! instruments {
    ($($variant:ident, $name:literal, $mes:literal, $pan:literal;)*) => {
        /// A drum sound of the Default Drum Note Map (M2-125 §4.1, Table 5), named
        /// for the sound its Note Number plays. A sound's tone is the device's
        /// own; only its role is defined.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum DrumInstrument {
            $(
                #[doc = $name]
                $variant,
            )*
        }

        /// Every instrument, in order of Note Number.
        const INSTRUMENTS: [DrumInstrument; 62] = [$(DrumInstrument::$variant),*];
        /// The name of each instrument.
        const NAMES: [&str; 62] = [$($name),*];
        /// The Mutually Exclusive Set of each instrument; 0 is none.
        const SETS: [u8; 62] = [$($mes),*];
        /// The recommended pan of each instrument in percent, negative to the left.
        const PANS: [i8; 62] = [$($pan),*];
    };
}

instruments! {
    HighQ, "High Q", 0, -23;
    Slap, "Slap", 0, -23;
    ScratchPush, "Scratch Push", 7, -16;
    ScratchPull, "Scratch Pull", 7, -16;
    Sticks, "Sticks", 0, 0;
    SquareClick, "Square Click", 0, -16;
    MetronomeClick, "Metronome Click", 0, 0;
    MetronomeBell, "Metronome Bell", 0, 0;
    AcousticBassDrum, "Acoustic Bass Drum", 0, 0;
    BassDrum1, "Bass Drum 1", 0, 0;
    SideStick, "Side Stick", 0, 0;
    AcousticSnare, "Acoustic Snare", 0, 0;
    HandClap, "Hand Clap", 0, -16;
    ElectricSnare, "Electric Snare", 0, 0;
    LowFloorTom, "Low Floor Tom", 0, -47;
    ClosedHiHat, "Closed Hi-hat", 1, 32;
    HighFloorTom, "High Floor Tom", 0, -28;
    PedalHiHat, "Pedal Hi-hat", 1, 32;
    LowTom, "Low Tom", 0, -9;
    OpenHiHat, "Open Hi-hat", 1, 32;
    LowMidTom, "Low-Mid Tom", 0, 10;
    HighMidTom, "High Mid Tom", 0, 29;
    CrashCymbal1, "Crash Cymbal 1", 0, 32;
    HighTom, "High Tom", 0, 48;
    RideCymbal1, "Ride Cymbal 1", 0, -31;
    ChineseCymbal, "Chinese Cymbal", 0, -31;
    RideBell, "Ride Bell", 0, -31;
    Tambourine, "Tambourine", 0, 16;
    SplashCymbal, "Splash Cymbal", 0, -16;
    Cowbell, "Cowbell", 0, 32;
    CrashCymbal2, "Crash Cymbal 2", 0, -31;
    VibraSlap, "Vibra-slap", 0, -55;
    RideCymbal2, "Ride Cymbal 2", 0, -31;
    HighBongo, "High Bongo", 0, 56;
    LowBongo, "Low Bongo", 0, 56;
    MuteHighConga, "Mute High Conga", 0, -39;
    OpenHighConga, "Open High Conga", 0, -39;
    LowConga, "Low Conga", 0, -39;
    HighTimbale, "High Timbale", 0, 32;
    LowTimbale, "Low Timbale", 0, 32;
    HighAgogo, "High Agogo", 0, -55;
    LowAgogo, "Low Agogo", 0, -55;
    Cabasa, "Cabasa", 0, -55;
    Maracas, "Maracas", 0, -63;
    ShortWhistle, "Short Whistle", 2, 56;
    LongWhistle, "Long Whistle", 2, 56;
    ShortGuiro, "Short Guiro", 3, 48;
    LongGuiro, "Long Guiro", 3, 48;
    Claves, "Claves", 0, 32;
    HighWoodBlock, "High Wood Block", 0, 56;
    LowWoodBlock, "Low Wood Block", 0, 56;
    MuteCuica, "Mute Cuica", 4, -31;
    OpenCuica, "Open Cuica", 4, -31;
    MuteTriangle, "Mute Triangle", 5, -63;
    OpenTriangle, "Open Triangle", 5, -63;
    Shaker, "Shaker", 0, 48;
    JingleBell, "Jingle Bell", 0, 56;
    BellTree, "Bell Tree", 0, 60;
    Castanets, "Castanets", 0, -47;
    MuteSurdo, "Mute Surdo", 6, -31;
    OpenSurdo, "Open Surdo", 6, -31;
    Applause, "Applause", 0, 0;
}

impl DrumInstrument {
    /// The lowest Note Number of the map (27).
    pub const FIRST_NOTE: U7 = U7::of(27);
    /// The highest Note Number of the map (88).
    pub const LAST_NOTE: U7 = U7::of(88);

    /// The instrument on `note`, or `None` for a Note Number outside 27 to 88.
    /// A device may place extra sounds on 0 through 26 and 89 through 127
    /// (M2-125 §4.1); the map does not name them.
    #[must_use]
    pub const fn of_note(note: U7) -> Option<Self> {
        let n = note.get();
        if n >= Self::FIRST_NOTE.get() && n <= Self::LAST_NOTE.get() {
            Some(INSTRUMENTS[(n - Self::FIRST_NOTE.get()) as usize])
        } else {
            None
        }
    }

    /// Every instrument, in order of Note Number.
    pub fn all() -> impl Iterator<Item = Self> {
        INSTRUMENTS.into_iter()
    }

    const fn index(self) -> usize {
        self as usize
    }

    /// The Note Number the instrument plays on.
    #[must_use]
    pub const fn note(self) -> U7 {
        // The discriminant is at most 61, so the note is at most 88 and
        // `masked` leaves it unchanged.
        U7::masked(Self::FIRST_NOTE.get() + self as u8)
    }

    /// The name Table 5 gives it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        NAMES[self.index()]
    }

    /// The Mutually Exclusive Set it belongs to, if any.
    #[must_use]
    pub const fn mutually_exclusive_set(self) -> Option<MutuallyExclusiveSet> {
        MutuallyExclusiveSet::new(SETS[self.index()])
    }

    /// The recommended (optional) pan position (M2-125 §4.1, Table 5).
    #[must_use]
    pub fn recommended_pan(self) -> Pan {
        Pan::new(f64::from(PANS[self.index()]) / 100.0)
    }

    /// The bit that declares a sound on this Note Number in the Profile
    /// Details Reply (Table 5, Table 11).
    #[must_use]
    pub const fn bitmap_position(self) -> BitmapPosition {
        // The index is at most 61, inside the position's range.
        BitmapPosition::of(self.index() as u8)
    }
}

tutti_units::ranged! {
    /// Where a bit sits in the Inquiry Target Data (M2-125 Table 5 and Table 11):
    /// the bit's index counted from `D0` of data byte 2, seven bits to a byte, so
    /// `0..=62` (data bytes 2 to 10). The data byte, counted from 1, and the bit
    /// in it, `D0` to `D6`, are [`byte`](Self::byte) and [`bit`](Self::bit).
    #[derive(Eq, Hash)]
    BitmapPosition(u8), NotABitmapPosition, "a bit index within 0..=62", |raw| raw <= 62
}

impl BitmapPosition {
    /// The data byte, `2..=10`.
    #[must_use]
    pub const fn byte(self) -> u8 {
        2 + self.get() / 7
    }

    /// The bit in the data byte, `0..=6`.
    #[must_use]
    pub const fn bit(self) -> u8 {
        self.get() % 7
    }
}

/// A control the Profile responds to on a Note Number or a Channel (M2-125 §5
/// and §6.1, Table 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DrumControl {
    /// Volume, CC#7 and RPNC#7; its curve is [`volume_gain`](super::volume_gain).
    Volume,
    /// Pan, RPNC#10 (and Key-Based Instrument Controller 10).
    Pan,
    /// Timbre/Harmonic Intensity, controller 71.
    Timbre,
    /// Release Time, controller 72.
    ReleaseTime,
    /// Attack Time, controller 73.
    AttackTime,
    /// Brightness, controller 74.
    Brightness,
    /// Decay Time, controller 75.
    DecayTime,
    /// Reverb Send Level (Effects 1 Depth), CC#91 and RPNC#91; its curve is
    /// [`send_level`](super::send_level).
    ReverbSend,
}

impl DrumControl {
    /// Every control, in order of controller number.
    pub const ALL: [Self; 8] = [
        Self::Volume,
        Self::Pan,
        Self::Timbre,
        Self::ReleaseTime,
        Self::AttackTime,
        Self::Brightness,
        Self::DecayTime,
        Self::ReverbSend,
    ];

    /// The controller number: the Registered Per-Note Controller's index
    /// (§6.1), the Key-Based Instrument Controller's `nn` (§6.2, Table 8) and,
    /// for Volume and Reverb Send, the Control Change's (§5.1, §5.2).
    #[must_use]
    pub const fn number(self) -> U7 {
        U7::of(match self {
            Self::Volume => 7,
            Self::Pan => 10,
            Self::Timbre => 71,
            Self::ReleaseTime => 72,
            Self::AttackTime => 73,
            Self::Brightness => 74,
            Self::DecayTime => 75,
            Self::ReverbSend => 91,
        })
    }

    /// The Registered Per-Note Controller that sets it on one Note Number.
    #[must_use]
    pub const fn per_note(self) -> RegisteredPerNoteController {
        match self {
            Self::Volume => RegisteredPerNoteController::Volume,
            Self::Pan => RegisteredPerNoteController::Pan,
            Self::Timbre => RegisteredPerNoteController::Timbre,
            Self::ReleaseTime => RegisteredPerNoteController::ReleaseTime,
            Self::AttackTime => RegisteredPerNoteController::AttackTime,
            Self::Brightness => RegisteredPerNoteController::Brightness,
            Self::DecayTime => RegisteredPerNoteController::DecayTime,
            Self::ReverbSend => RegisteredPerNoteController::ReverbSend,
        }
    }

    /// The Control Change that sets it for the whole Channel, for the two the
    /// Profile defines per Channel (§5.1, §5.2).
    #[must_use]
    pub const fn channel_cc(self) -> Option<CCNumber> {
        match self {
            Self::Volume => Some(CCNumber::of(7)),
            Self::ReverbSend => Some(CCNumber::of(91)),
            _ => None,
        }
    }

    /// The default 32-bit value, for the controls with one (§5.1, §6.1.1,
    /// §6.1.3 to §6.1.7): Volume `0xC8000000`, the relative sound controls
    /// `0x80000000` (no change). Pan and Reverb Send state none.
    #[must_use]
    pub const fn default_value(self) -> Option<tutti_ump::ControlValue> {
        match self {
            Self::Volume => Some(tutti_ump::ControlValue::new(0xC800_0000)),
            Self::Timbre
            | Self::ReleaseTime
            | Self::AttackTime
            | Self::Brightness
            | Self::DecayTime => Some(tutti_ump::ControlValue::new(0x8000_0000)),
            Self::Pan | Self::ReverbSend => None,
        }
    }
}

/// The set of Note Numbers that have a sound available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DrumSounds(u64);

impl DrumSounds {
    /// No sound available.
    pub const NONE: Self = Self(0);

    /// The set with `instrument` added.
    #[must_use]
    pub const fn with(self, instrument: DrumInstrument) -> Self {
        Self(self.0 | 1 << instrument.index())
    }

    /// Whether `instrument` has a sound available.
    #[must_use]
    pub const fn contains(self, instrument: DrumInstrument) -> bool {
        self.0 >> instrument.index() & 1 == 1
    }

    /// The instruments in the set, in order of Note Number.
    pub fn iter(self) -> impl Iterator<Item = DrumInstrument> {
        DrumInstrument::all().filter(move |i| self.contains(*i))
    }
}

impl FromIterator<DrumInstrument> for DrumSounds {
    fn from_iter<I: IntoIterator<Item = DrumInstrument>>(iter: I) -> Self {
        iter.into_iter().fold(Self::NONE, Self::with)
    }
}

/// What a receiver declares in its Reply to a Profile Details Inquiry for the
/// Optional Features target (M2-125 §7.1.3, Table 11): the Inquiry Target Data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DrumFeatures {
    /// Whether it responds to MIDI 2.0 Per-Note Controllers (byte 1, `D0`).
    pub per_note_controllers: bool,
    /// Whether it responds to MIDI 1.0 Key-Based Instrument Controllers (byte
    /// 1, `D1`).
    pub key_based_controllers: bool,
    /// The Note Numbers that have a default sound available (bytes 2 to 10).
    pub sounds: DrumSounds,
}

impl DrumFeatures {
    /// The Inquiry Target of the Optional Features Details message (§7.1.1).
    pub const INQUIRY_TARGET: InquiryTarget = InquiryTarget::new(U7::of(0x01));

    /// The length of the Inquiry Target Data in bytes: the features byte and
    /// nine bytes of sounds (Table 11).
    pub const DATA_LEN: usize = 10;

    /// The Inquiry Target Data. The reserved bits are zero.
    #[must_use]
    pub fn to_data(self) -> [u8; Self::DATA_LEN] {
        let mut data = [0u8; Self::DATA_LEN];
        data[0] = u8::from(self.per_note_controllers) | u8::from(self.key_based_controllers) << 1;
        for instrument in self.sounds.iter() {
            let at = instrument.bitmap_position();
            data[usize::from(at.byte()) - 1] |= 1 << at.bit();
        }
        data
    }

    /// Reads the Inquiry Target Data, or `None` for anything but ten bytes with
    /// the reserved bits clear: a reserved bit is refused, not ignored.
    #[must_use]
    pub fn from_data(data: &[u8]) -> Option<Self> {
        let data: &[u8; Self::DATA_LEN] = data.try_into().ok()?;
        if data[0] > 0b11 || data[9] > 0b0011_1111 || data.iter().any(|b| b & 0x80 != 0) {
            return None;
        }
        let sounds = DrumInstrument::all()
            .filter(|i| {
                let at = i.bitmap_position();
                data[usize::from(at.byte()) - 1] >> at.bit() & 1 == 1
            })
            .collect();
        Some(Self {
            per_note_controllers: data[0] & 1 == 1,
            key_based_controllers: data[0] >> 1 & 1 == 1,
            sounds,
        })
    }
}

#[cfg(test)]
mod tests;
